//! Inbound control demux. The socket reader feeds bytes; complete frames update
//! injection and the encoder. Video messages are not expected on this path.

use droidmirror_proto::{decode_scroll, try_decode_frame, Message};

pub trait Inject {
    fn touch(&mut self, action: u8, id: u8, x: i32, y: i32, pressure: u16);
    fn key(&mut self, action: u8, keycode: u32, meta: u32);
    fn text(&mut self, s: &str);
    fn scroll(&mut self, x: i32, y: i32, h: i32, v: i32);
}

pub trait EncoderCtrl {
    fn set_encoding(&mut self, bitrate: u32, max_fps: u32, max_size: u16);
    fn pause(&mut self, on: bool);
}

pub struct ControlDemux {
    buf: Vec<u8>,
}

impl Default for ControlDemux {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlDemux {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(
        &mut self,
        data: &[u8],
        inj: &mut dyn Inject,
        enc: &mut dyn EncoderCtrl,
    ) -> Result<(), String> {
        self.buf.extend_from_slice(data);
        loop {
            match try_decode_frame(&self.buf) {
                Ok(None) => return Ok(()),
                Ok(Some((msg, n))) => {
                    self.buf.drain(..n);
                    apply_message(msg, inj, enc);
                }
                Err(e) => {
                    self.buf.clear();
                    return Err(e.to_string());
                }
            }
        }
    }
}

pub fn apply_message(msg: Message, inj: &mut dyn Inject, enc: &mut dyn EncoderCtrl) {
    match msg {
        Message::Touch {
            action,
            pointer_id,
            x,
            y,
            pressure,
        } => inj.touch(action, pointer_id, x, y, pressure),
        Message::Key {
            action,
            keycode,
            meta,
        } => inj.key(action, keycode, meta),
        Message::KeySyn { action, keycode } => inj.key(action, keycode, 0),
        Message::Text(s) => inj.text(&s),
        Message::Scroll { x, y, h, v } => inj.scroll(x, y, notches(h), notches(v)),
        Message::SetEncoding {
            bitrate,
            max_fps,
            max_size,
        } => enc.set_encoding(bitrate, max_fps, max_size),
        Message::PauseResume { on } => enc.pause(on),
        Message::VideoFrame { .. } | Message::Configure { .. } | Message::Reserved { .. } => {}
    }
}

fn notches(fixed: i32) -> i32 {
    let v = decode_scroll(fixed);
    if v == 0.0 {
        return 0;
    }
    let n = v.round() as i32;
    if n != 0 {
        n
    } else if v > 0.0 {
        1
    } else {
        -1
    }
}

/// Even dimensions, longer side ≤ `max_size` (0 = unchanged).
pub fn fit_max_size(w: u32, h: u32, max_size: u16) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w, h);
    }
    let (mut nw, mut nh) = (w, h);
    if max_size > 0 {
        let max_side = w.max(h);
        if max_side > max_size as u32 {
            let scale = max_size as f32 / max_side as f32;
            nw = ((w as f32) * scale).round() as u32;
            nh = ((h as f32) * scale).round() as u32;
        }
    }
    nw = (nw & !1).max(2);
    nh = (nh & !1).max(2);
    (nw, nh)
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bitrate: u32,
    pub max_fps: u32,
    pub max_size: u16,
    pub h265: bool,
    pub lib: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bitrate: 8_000_000,
            max_fps: 60,
            max_size: 0,
            h265: false,
            lib: None,
        }
    }
}

pub fn parse_server_args<I, S>(args: I) -> ServerConfig
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut cfg = ServerConfig::default();
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = args.get(i + 1).map(|s| s.as_str());
        match a {
            "--bitrate" => {
                if let Some(v) = next.and_then(|s| s.parse().ok()) {
                    cfg.bitrate = v;
                    i += 1;
                }
            }
            "--max-fps" => {
                if let Some(v) = next.and_then(|s| s.parse().ok()) {
                    cfg.max_fps = v;
                    i += 1;
                }
            }
            "--max-size" => {
                if let Some(v) = next.and_then(|s| s.parse().ok()) {
                    cfg.max_size = v;
                    i += 1;
                }
            }
            "--codec" => {
                if let Some(v) = next {
                    cfg.h265 = v.eq_ignore_ascii_case("h265") || v.eq_ignore_ascii_case("hevc");
                    i += 1;
                }
            }
            _ if a.starts_with("--lib=") => cfg.lib = Some(a["--lib=".len()..].to_string()),
            _ => {}
        }
        i += 1;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidmirror_proto::{encode_scroll, Message};

    struct RecInj {
        touches: Vec<(u8, i32, i32)>,
        texts: Vec<String>,
        scrolls: Vec<(i32, i32)>,
    }
    impl Default for RecInj {
        fn default() -> Self {
            Self {
                touches: vec![],
                texts: vec![],
                scrolls: vec![],
            }
        }
    }
    struct RecEnc {
        bitrate: u32,
        paused: bool,
    }
    impl Default for RecEnc {
        fn default() -> Self {
            Self {
                bitrate: 0,
                paused: false,
            }
        }
    }
    impl Inject for RecInj {
        fn touch(&mut self, action: u8, _id: u8, x: i32, y: i32, _p: u16) {
            self.touches.push((action, x, y));
        }
        fn key(&mut self, _action: u8, _keycode: u32, _meta: u32) {}
        fn text(&mut self, s: &str) {
            self.texts.push(s.to_string());
        }
        fn scroll(&mut self, _x: i32, _y: i32, h: i32, v: i32) {
            self.scrolls.push((h, v));
        }
    }
    impl EncoderCtrl for RecEnc {
        fn set_encoding(&mut self, bitrate: u32, _: u32, _: u16) {
            self.bitrate = bitrate;
        }
        fn pause(&mut self, on: bool) {
            self.paused = on;
        }
    }

    #[test]
    fn partial_then_complete() {
        let msg = Message::Touch {
            action: 0,
            pointer_id: 1,
            x: 5,
            y: 6,
            pressure: 1,
        }
        .encode()
        .unwrap();
        let mut d = ControlDemux::new();
        let mut inj = RecInj::default();
        let mut enc = RecEnc::default();
        d.push(&msg[..3], &mut inj, &mut enc).unwrap();
        assert!(inj.touches.is_empty());
        d.push(&msg[3..], &mut inj, &mut enc).unwrap();
        assert_eq!(inj.touches, vec![(0, 5, 6)]);

        d.push(&Message::Text("hi".into()).encode().unwrap(), &mut inj, &mut enc)
            .unwrap();
        assert_eq!(inj.texts, vec!["hi".to_string()]);

        let sc = Message::Scroll {
            x: 0,
            y: 0,
            h: 0,
            v: encode_scroll(-1.0),
        }
        .encode()
        .unwrap();
        d.push(&sc, &mut inj, &mut enc).unwrap();
        assert_eq!(inj.scrolls, vec![(0, -1)]);
        d.push(
            &Message::SetEncoding {
                bitrate: 123,
                max_fps: 30,
                max_size: 0,
            }
            .encode()
            .unwrap(),
            &mut inj,
            &mut enc,
        )
        .unwrap();
        assert_eq!(enc.bitrate, 123);
        d.push(
            &Message::PauseResume { on: true }.encode().unwrap(),
            &mut inj,
            &mut enc,
        )
        .unwrap();
        assert!(enc.paused);
    }

    #[test]
    fn fit_and_args() {
        assert_eq!(fit_max_size(1080, 2400, 720), (324, 720));
        let cfg = parse_server_args(["--bitrate", "100", "--codec", "h265", "--lib=/so"]);
        assert_eq!(cfg.bitrate, 100);
        assert!(cfg.h265);
        assert_eq!(cfg.lib.as_deref(), Some("/so"));
    }
}
