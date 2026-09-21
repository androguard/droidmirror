//! Framework encoder via JNI (`MediaCodec` + hidden `SurfaceControl`).
//! NDK `AMediaCodec` is intentionally not the baseline — shell uid reaches the
//! hidden display APIs through the `app_process` classloader.

use droidmirror_proto::{to_annex_b, Codec};
use jni::objects::{JObject, JString, JValue, GlobalRef};
use jni::JNIEnv;

use super::log_line;
use crate::annexb::csd_annex_b;
use crate::control::EncoderCtrl;

const COLOR_FORMAT_SURFACE: i32 = 0x7F000789;
const CONFIGURE_FLAG_ENCODE: i32 = 1;
const INFO_TRY_AGAIN: i32 = -1;
const INFO_OUTPUT_FORMAT_CHANGED: i32 = -2;
const FLAG_KEY_FRAME: i32 = 1;
const FLAG_CODEC_CONFIG: i32 = 2;

pub struct EncodedAu {
    pub pts_us: u64,
    pub keyframe: bool,
    pub nal: Vec<u8>,
}

pub struct Capture {
    codec: GlobalRef,
    display: GlobalRef,
    info: GlobalRef,
    width: i32,
    height: i32,
    codec_kind: Codec,
    csd: Vec<u8>,
    paused: bool,
    bitrate: u32,
    max_fps: u32,
    params_dirty: bool,
}

impl Capture {
    pub fn start(
        env: &mut JNIEnv,
        width: i32,
        height: i32,
        bitrate: u32,
        fps: u32,
        h265: bool,
    ) -> Result<Self, String> {
        let mime = if h265 { "video/hevc" } else { "video/avc" };
        let codec_kind = if h265 { Codec::H265 } else { Codec::H264 };
        log_line(&format!("encoder {mime} {width}x{height} @{fps}fps {bitrate}bps"));

        let mc_cls = find(env, "android/media/MediaCodec")?;
        let jmime = new_string(env, mime)?;
        let codec = call_static_obj(
            env,
            &mc_cls,
            "createEncoderByType",
            "(Ljava/lang/String;)Landroid/media/MediaCodec;",
            &[JValue::Object(&jmime)],
        )?;

        let fmt_cls = find(env, "android/media/MediaFormat")?;
        let format = call_static_obj(
            env,
            &fmt_cls,
            "createVideoFormat",
            "(Ljava/lang/String;II)Landroid/media/MediaFormat;",
            &[
                JValue::Object(&jmime),
                JValue::Int(width),
                JValue::Int(height),
            ],
        )?;
        put_int(env, &format, "color-format", COLOR_FORMAT_SURFACE)?;
        put_int(env, &format, "bitrate", bitrate as i32)?;
        put_int(env, &format, "frame-rate", fps.max(1) as i32)?;
        put_int(env, &format, "i-frame-interval", 1)?;
        put_int(env, &format, "bitrate-mode", 1)?; // VBR

        call_void(
            env,
            &codec,
            "configure",
            "(Landroid/media/MediaFormat;Landroid/view/Surface;Landroid/media/MediaCrypto;I)V",
            &[
                JValue::Object(&format),
                JValue::Object(&JObject::null()),
                JValue::Object(&JObject::null()),
                JValue::Int(CONFIGURE_FLAG_ENCODE),
            ],
        )?;
        let surface = call_obj(env, &codec, "createInputSurface", "()Landroid/view/Surface;", &[])?;
        let display = create_virtual_display(env, &surface, width, height)?;
        call_void(env, &codec, "start", "()V", &[])?;

        let info_cls = find(env, "android/media/MediaCodec$BufferInfo")?;
        let info = env
            .new_object(info_cls, "()V", &[])
            .map_err(|e| jerr(env, "BufferInfo", e))?;

        Ok(Self {
            codec: global(env, codec)?,
            display: global(env, display)?,
            info: global(env, info)?,
            width,
            height,
            codec_kind,
            csd: Vec::new(),
            paused: false,
            bitrate,
            max_fps: fps,
            params_dirty: false,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec_kind
    }

    /// Block briefly until SPS/PPS (or VPS) shows up, or return what we have.
    pub fn wait_csd(&mut self, env: &mut JNIEnv) -> Vec<u8> {
        for _ in 0..40 {
            let _ = self.poll(env, 50_000);
            if !self.csd.is_empty() {
                break;
            }
        }
        log_line(&format!(
            "csd {} bytes for {}x{}",
            self.csd.len(),
            self.width,
            self.height
        ));
        self.csd.clone()
    }

    pub fn poll(&mut self, env: &mut JNIEnv, timeout_us: i64) -> Option<EncodedAu> {
        if self.paused {
            return None;
        }
        let idx = env
            .call_method(
                &self.codec,
                "dequeueOutputBuffer",
                "(Landroid/media/MediaCodec$BufferInfo;J)I",
                &[JValue::Object(&self.info), JValue::Long(timeout_us)],
            )
            .map_err(|e| log_line(&jerr(env, "dequeue", e)))
            .ok()?
            .i()
            .unwrap_or(INFO_TRY_AGAIN);
        if idx == INFO_TRY_AGAIN {
            return None;
        }
        if idx == INFO_OUTPUT_FORMAT_CHANGED {
            if let Err(e) = self.read_format_csd(env) {
                log_line(&e);
            }
            return None;
        }
        if idx < 0 {
            return None;
        }
        let flags = field_int(env, &self.info, "flags").unwrap_or(0);
        let pts = field_long(env, &self.info, "presentationTimeUs").unwrap_or(0).max(0) as u64;
        let offset = field_int(env, &self.info, "offset").unwrap_or(0);
        let size = field_int(env, &self.info, "size").unwrap_or(0);
        let bytes = if size > 0 {
            copy_output(env, &self.codec, idx, offset, size).unwrap_or_default()
        } else {
            Vec::new()
        };
        let _ = env.call_method(
            &self.codec,
            "releaseOutputBuffer",
            "(IZ)V",
            &[JValue::Int(idx), JValue::Bool(0)],
        );
        if flags & FLAG_CODEC_CONFIG != 0 {
            if !bytes.is_empty() {
                self.csd = to_annex_b(&bytes);
            }
            return None;
        }
        if bytes.is_empty() {
            return None;
        }
        Some(EncodedAu {
            pts_us: pts,
            keyframe: flags & FLAG_KEY_FRAME != 0,
            nal: to_annex_b(&bytes),
        })
    }

    fn read_format_csd(&mut self, env: &mut JNIEnv) -> Result<(), String> {
        let format = call_obj(
            env,
            self.codec.as_obj(),
            "getOutputFormat",
            "()Landroid/media/MediaFormat;",
            &[],
        )?;
        let mut parts = Vec::new();
        for key in ["csd-0", "csd-1", "csd-2"] {
            if let Some(bytes) = format_bytes(env, &format, key) {
                parts.push(bytes);
            }
        }
        if !parts.is_empty() {
            self.csd = csd_annex_b(&parts);
            log_line(&format!("csd {} bytes", self.csd.len()));
        }
        Ok(())
    }

    pub fn release(&self, env: &mut JNIEnv) {
        let _ = call_void(env, &self.codec, "stop", "()V", &[]);
        let _ = call_void(env, &self.codec, "release", "()V", &[]);
        let sc = find(env, "android/view/SurfaceControl");
        if let Ok(sc) = sc {
            let _ = env.call_static_method(
                sc,
                "destroyDisplay",
                "(Landroid/os/IBinder;)V",
                &[JValue::Object(&self.display)],
            );
        }
    }
}

impl EncoderCtrl for Capture {
    fn set_encoding(&mut self, bitrate: u32, max_fps: u32, _max_size: u16) {
        self.bitrate = bitrate;
        self.max_fps = max_fps.max(1);
        self.params_dirty = true;
    }

    fn pause(&mut self, on: bool) {
        self.paused = on;
        log_line(if on { "pause" } else { "resume" });
    }
}

impl Capture {
    pub fn take_params_dirty(&mut self) -> bool {
        let d = self.params_dirty;
        self.params_dirty = false;
        d
    }

    pub fn apply_params(&self, env: &mut JNIEnv) {
        let Ok(cls) = find(env, "android/os/Bundle") else {
            return;
        };
        let Ok(bundle) = env.new_object(&cls, "()V", &[]) else {
            return;
        };
        let _ = put_int(env, &bundle, "video-bitrate", self.bitrate as i32);
        if let Ok(key) = new_string(env, "max-fps-to-encoder") {
            let _ = env.call_method(
                &bundle,
                "putFloat",
                "(Ljava/lang/String;F)V",
                &[
                    JValue::Object(&key),
                    JValue::Float(self.max_fps as f32),
                ],
            );
        }
        let _ = env.call_method(
            &self.codec,
            "setParameters",
            "(Landroid/os/Bundle;)V",
            &[JValue::Object(&bundle)],
        );
    }
}

pub struct DisplayInfo {
    pub width: i32,
    pub height: i32,
    pub rotation: i32,
    pub name: String,
}

pub fn display_info(env: &mut JNIEnv) -> Result<DisplayInfo, String> {
    let dmg = find(env, "android/hardware/display/DisplayManagerGlobal")?;
    let inst = call_static_obj(env, &dmg, "getInstance", "()Landroid/hardware/display/DisplayManagerGlobal;", &[])?;
    let display = call_obj(
        env,
        &inst,
        "getRealDisplay",
        "(I)Landroid/view/Display;",
        &[JValue::Int(0)],
    )?;
    let point_cls = find(env, "android/graphics/Point")?;
    let point = env
        .new_object(point_cls, "()V", &[])
        .map_err(|e| jerr(env, "Point", e))?;
    call_void(
        env,
        &display,
        "getRealSize",
        "(Landroid/graphics/Point;)V",
        &[JValue::Object(&point)],
    )?;
    let width = field_int(env, &point, "x").unwrap_or(1080);
    let height = field_int(env, &point, "y").unwrap_or(1920);
    let rotation = call_int(env, &display, "getRotation", "()I", &[]).unwrap_or(0);
    let name = device_name(env).unwrap_or_else(|_| "Android".into());
    Ok(DisplayInfo {
        width,
        height,
        rotation,
        name,
    })
}

pub fn device_name(env: &mut JNIEnv) -> Result<String, String> {
    let cls = find(env, "android/os/Build")?;
    let model = env
        .get_static_field(cls, "MODEL", "Ljava/lang/String;")
        .map_err(|e| jerr(env, "MODEL", e))?
        .l()
        .map_err(|e| e.to_string())?;
    let s: String = env
        .get_string(&JString::from(model))
        .map_err(|e| e.to_string())?
        .into();
    Ok(s)
}

fn create_virtual_display<'a>(
    env: &mut JNIEnv<'a>,
    surface: &JObject<'a>,
    width: i32,
    height: i32,
) -> Result<JObject<'a>, String> {
    let sc = find(env, "android/view/SurfaceControl")?;
    let name = new_string(env, "droidmirror")?;
    let display = call_static_obj(
        env,
        &sc,
        "createDisplay",
        "(Ljava/lang/String;Z)Landroid/os/IBinder;",
        &[JValue::Object(&name), JValue::Bool(0)],
    )?;

    let layer_stack = current_layer_stack(env).unwrap_or(0);
    let rect_cls = find(env, "android/graphics/Rect")?;
    let rect = env
        .new_object(
            rect_cls,
            "(IIII)V",
            &[
                JValue::Int(0),
                JValue::Int(0),
                JValue::Int(width),
                JValue::Int(height),
            ],
        )
        .map_err(|e| jerr(env, "Rect", e))?;

    call_static_void(env, &sc, "openTransaction", "()V", &[])?;
    let set_surface = call_static_void(
        env,
        &sc,
        "setDisplaySurface",
        "(Landroid/os/IBinder;Landroid/view/Surface;)V",
        &[JValue::Object(&display), JValue::Object(surface)],
    );
    let set_proj = call_static_void(
        env,
        &sc,
        "setDisplayProjection",
        "(Landroid/os/IBinder;ILandroid/graphics/Rect;Landroid/graphics/Rect;)V",
        &[
            JValue::Object(&display),
            JValue::Int(0),
            JValue::Object(&rect),
            JValue::Object(&rect),
        ],
    );
    let set_layer = call_static_void(
        env,
        &sc,
        "setDisplayLayerStack",
        "(Landroid/os/IBinder;I)V",
        &[JValue::Object(&display), JValue::Int(layer_stack)],
    );
    let _ = call_static_void(env, &sc, "closeTransaction", "()V", &[]);
    set_surface?;
    set_proj?;
    set_layer?;
    Ok(display)
}

fn current_layer_stack(env: &mut JNIEnv) -> Result<i32, String> {
    let dmg = find(env, "android/hardware/display/DisplayManagerGlobal")?;
    let inst = call_static_obj(
        env,
        &dmg,
        "getInstance",
        "()Landroid/hardware/display/DisplayManagerGlobal;",
        &[],
    )?;
    let display = call_obj(
        env,
        &inst,
        "getRealDisplay",
        "(I)Landroid/view/Display;",
        &[JValue::Int(0)],
    )?;
    call_int(env, &display, "getLayerStack", "()I", &[])
}

fn format_bytes(env: &mut JNIEnv, format: &JObject, key: &str) -> Option<Vec<u8>> {
    let jkey = new_string(env, key).ok()?;
    let buf = env
        .call_method(
            format,
            "getByteBuffer",
            "(Ljava/lang/String;)Ljava/nio/ByteBuffer;",
            &[JValue::Object(&jkey)],
        )
        .ok()?
        .l()
        .ok()?;
    if buf.is_null() {
        return None;
    }
    let remaining = env.call_method(&buf, "remaining", "()I", &[]).ok()?.i().ok()?;
    if remaining <= 0 {
        return None;
    }
    let arr = env.new_byte_array(remaining).ok()?;
    let _ = env.call_method(
        &buf,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        &[JValue::Object(&arr)],
    );
    env.convert_byte_array(arr).ok()
}

fn copy_output(
    env: &mut JNIEnv,
    codec: &GlobalRef,
    index: i32,
    offset: i32,
    size: i32,
) -> Result<Vec<u8>, String> {
    let buf = call_obj(
        env,
        codec.as_obj(),
        "getOutputBuffer",
        "(I)Ljava/nio/ByteBuffer;",
        &[JValue::Int(index)],
    )?;
    call_obj(
        env,
        &buf,
        "position",
        "(I)Ljava/nio/Buffer;",
        &[JValue::Int(offset)],
    )?;
    call_obj(
        env,
        &buf,
        "limit",
        "(I)Ljava/nio/Buffer;",
        &[JValue::Int(offset + size)],
    )?;
    let arr = env
        .new_byte_array(size)
        .map_err(|e| jerr(env, "byte[]", e))?;
    let _ = env.call_method(
        &buf,
        "get",
        "([B)Ljava/nio/ByteBuffer;",
        &[JValue::Object(&arr)],
    );
    env.convert_byte_array(arr)
        .map_err(|e| jerr(env, "copy", e))
}

fn find<'a>(env: &mut JNIEnv<'a>, name: &str) -> Result<jni::objects::JClass<'a>, String> {
    env.find_class(name).map_err(|e| jerr(env, name, e))
}

fn new_string<'a>(env: &mut JNIEnv<'a>, s: &str) -> Result<JString<'a>, String> {
    env.new_string(s).map_err(|e| jerr(env, "string", e))
}

fn global(env: &mut JNIEnv, obj: JObject) -> Result<GlobalRef, String> {
    env.new_global_ref(obj).map_err(|e| e.to_string())
}

fn call_static_obj<'a>(
    env: &mut JNIEnv<'a>,
    cls: &jni::objects::JClass,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> Result<JObject<'a>, String> {
    env.call_static_method(cls, name, sig, args)
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, name, e))
}

fn call_static_void(
    env: &mut JNIEnv,
    cls: &jni::objects::JClass,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> Result<(), String> {
    env.call_static_method(cls, name, sig, args)
        .map(|_| ())
        .map_err(|e| jerr(env, name, e))
}

fn call_obj<'a, 'o>(
    env: &mut JNIEnv<'a>,
    obj: &JObject<'o>,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> Result<JObject<'a>, String> {
    env.call_method(obj, name, sig, args)
        .and_then(|v| v.l())
        .map_err(|e| jerr(env, name, e))
}

fn call_void(
    env: &mut JNIEnv,
    obj: &JObject,
    name: &str,
    sig: &str,
    args: &[JValue],
) -> Result<(), String> {
    env.call_method(obj, name, sig, args)
        .map(|_| ())
        .map_err(|e| jerr(env, name, e))
}

fn call_int(env: &mut JNIEnv, obj: &JObject, name: &str, sig: &str, args: &[JValue]) -> Result<i32, String> {
    env.call_method(obj, name, sig, args)
        .and_then(|v| v.i())
        .map_err(|e| jerr(env, name, e))
}

fn put_int(env: &mut JNIEnv, format: &JObject, key: &str, value: i32) -> Result<(), String> {
    let jkey = new_string(env, key)?;
    call_void(
        env,
        format,
        "setInteger",
        "(Ljava/lang/String;I)V",
        &[JValue::Object(&jkey), JValue::Int(value)],
    )
}

fn field_int(env: &mut JNIEnv, obj: &JObject, name: &str) -> Result<i32, String> {
    env.get_field(obj, name, "I")
        .and_then(|v| v.i())
        .map_err(|e| e.to_string())
}

fn field_long(env: &mut JNIEnv, obj: &JObject, name: &str) -> Result<i64, String> {
    env.get_field(obj, name, "J")
        .and_then(|v| v.j())
        .map_err(|e| e.to_string())
}

fn jerr(env: &mut JNIEnv, ctx: &str, e: jni::errors::Error) -> String {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    format!("{ctx}: {e}")
}
