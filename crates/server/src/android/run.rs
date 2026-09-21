use std::os::fd::RawFd;
use std::time::{Duration, Instant};

use droidmirror_proto::{encode_handshake, Codec, Handshake, Message, CAP_H265, CAP_UINPUT, VERSION};
use jni::objects::{JObject, JObjectArray, JString};
use jni::JNIEnv;

use super::capture::{display_info, Capture, DisplayInfo};
use super::inject_sdk::SdkInject;
use super::log_line;
use super::net::{self, Listener};
use super::uinput::{ShellInput, UInput};
use crate::control::{fit_max_size, parse_server_args, ControlDemux, Inject, ServerConfig};

pub fn serve_from_java(mut env: JNIEnv, args: JObject) {
    let argv = java_args(&mut env, args);
    let cfg = parse_server_args(&argv);
    if let Err(e) = serve(&mut env, &cfg) {
        log_line(&format!("server exit: {e}"));
    }
}

fn java_args(env: &mut JNIEnv, args: JObject) -> Vec<String> {
    if args.is_null() {
        return Vec::new();
    }
    let arr = JObjectArray::from(args);
    let Ok(len) = env.get_array_length(&arr) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..len {
        let Ok(elem) = env.get_object_array_element(&arr, i) else {
            continue;
        };
        if elem.is_null() {
            continue;
        }
        if let Ok(s) = env.get_string(&JString::from(elem)) {
            out.push(s.into());
        }
    }
    out
}

fn serve(env: &mut JNIEnv, cfg: &ServerConfig) -> Result<(), String> {
    let info = display_info(env).unwrap_or_else(|e| {
        log_line(&format!("display query failed ({e}); using 1080x1920"));
        DisplayInfo {
            width: 1080,
            height: 1920,
            rotation: 0,
            name: "Android".into(),
        }
    });
    let orig_w = info.width;
    let orig_h = info.height;
    let name = info.name.clone();
    let mut rotation = info.rotation;
    let (w, h) = fit_max_size(orig_w as u32, orig_h as u32, cfg.max_size);
    log_line(&format!(
        "{name} display {orig_w}x{orig_h} rot {rotation} → capture {w}x{h}"
    ));

    let mut capture = Capture::start(env, w as i32, h as i32, cfg.bitrate, cfg.max_fps, cfg.h265)?;
    let mut caps = 0u8;
    if cfg.h265 {
        caps |= CAP_H265;
    }
    let mut inject: Box<dyn Inject> = match SdkInject::open(env, w as i32, h as i32, orig_w, orig_h) {
        Ok(sdk) => {
            log_line("input manager ready");
            Box::new(sdk)
        }
        Err(e) => {
            log_line(&format!("input manager unavailable ({e}); trying uinput"));
            match UInput::open(w as i32, h as i32) {
                Ok(u) => {
                    caps |= CAP_UINPUT;
                    log_line("uinput ready");
                    Box::new(u)
                }
                Err(e) => {
                    log_line(&format!("uinput unavailable ({e}); debug `input` shim"));
                    Box::new(ShellInput)
                }
            }
        }
    };

    let listener = Listener::bind("droidmirror").map_err(|e| e.to_string())?;
    log_line("listening on localabstract:droidmirror");
    let client = listener.accept().map_err(|e| e.to_string())?;
    net::set_nonblock(client);

    let handshake = Handshake {
        version: VERSION,
        codec: capture.codec(),
        width: w as u16,
        height: h as u16,
        csd: capture.wait_csd(env),
        device_name: name,
        caps,
    };
    let raw = encode_handshake(&handshake).map_err(|e| e.to_string())?;
    net::write_all(client, &raw).map_err(|e| e.to_string())?;
    log_line("handshake sent");

    let mut demux = ControlDemux::new();
    let mut read_buf = [0u8; 64 * 1024];
    let mut last_rot = Instant::now();
    let mut seen_w = orig_w;
    let mut seen_h = orig_h;

    loop {
        match net::poll_in(client, 5) {
            Ok(true) => match net::read_some(client, &mut read_buf) {
                Ok(0) => {
                    log_line("client closed");
                    break;
                }
                Ok(n) => {
                    if let Err(e) = demux.push(&read_buf[..n], inject.as_mut(), &mut capture) {
                        log_line(&format!("control: {e}"));
                    }
                }
                Err(e) => {
                    log_line(&format!("read: {e}"));
                    break;
                }
            },
            Ok(false) => {}
            Err(e) => {
                log_line(&format!("poll: {e}"));
                break;
            }
        }
        if capture.take_params_dirty() {
            capture.apply_params(env);
        }
        if let Some(au) = capture.poll(env, 0) {
            let msg = Message::VideoFrame {
                pts_us: au.pts_us,
                keyframe: au.keyframe,
                nal: au.nal,
            };
            if let Ok(frame) = msg.encode() {
                if net::write_all(client, &frame).is_err() {
                    break;
                }
            }
        }
        if last_rot.elapsed() > Duration::from_millis(500) {
            last_rot = Instant::now();
            if let Ok(now) = display_info(env) {
                if now.rotation != rotation || now.width != seen_w || now.height != seen_h {
                    rotation = now.rotation;
                    seen_w = now.width;
                    seen_h = now.height;
                    log_line(&format!("display changed {}x{} rot {}", now.width, now.height, now.rotation));
                    let (nw, nh) = fit_max_size(now.width as u32, now.height as u32, cfg.max_size);
                    if let Err(e) = reconfigure(env, &mut capture, client, nw, nh, cfg) {
                        log_line(&format!("reconfigure: {e}"));
                    }
                }
            }
        }
    }
    capture.release(env);
    unsafe { libc::close(client) };
    Ok(())
}

fn reconfigure(
    env: &mut JNIEnv,
    capture: &mut Capture,
    client: RawFd,
    w: u32,
    h: u32,
    cfg: &ServerConfig,
) -> Result<(), String> {
    capture.release(env);
    *capture = Capture::start(env, w as i32, h as i32, cfg.bitrate, cfg.max_fps, cfg.h265)?;
    let msg = Message::Configure {
        codec: if cfg.h265 { Codec::H265 } else { Codec::H264 },
        width: w as u16,
        height: h as u16,
        csd: capture.wait_csd(env),
    };
    let bytes = msg.encode().map_err(|e| e.to_string())?;
    net::write_all(client, &bytes).map_err(|e| e.to_string())
}
