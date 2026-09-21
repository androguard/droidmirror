//! wasm-bindgen face of [`droidmirror_client`]. No transport and no decoder:
//! the host feeds bytes and hands `Frame` NALs to a decoder.

use droidmirror_client::{Client, KeyAction, TouchAction};
use droidmirror_proto::Codec;
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

#[cfg_attr(feature = "wasm-exports", wasm_bindgen)]
pub struct MirrorClient {
    inner: Client,
}

#[cfg_attr(feature = "wasm-exports", wasm_bindgen)]
impl MirrorClient {
    #[cfg_attr(feature = "wasm-exports", wasm_bindgen(constructor))]
    pub fn new() -> Self {
        Self {
            inner: Client::new(),
        }
    }

    /// Demux a chunk from `adb.readStream`. Returns an array of
    /// `{kind, ...}` objects: `configure`, `frame`, `name`, `error`.
    pub fn on_bytes(&mut self, buf: &[u8]) -> JsValue {
        let outs = self.inner.on_bytes(buf);
        let arr = Array::new();
        for ev in outs {
            let obj = Object::new();
            match ev {
                droidmirror_client::Out::Configure {
                    codec,
                    csd,
                    width,
                    height,
                } => {
                    set(&obj, "kind", &JsValue::from_str("configure"));
                    set(&obj, "codec", &JsValue::from_str(webcodecs(codec)));
                    set(&obj, "codecId", &JsValue::from(codec as u8));
                    set(&obj, "width", &JsValue::from(width));
                    set(&obj, "height", &JsValue::from(height));
                    set(&obj, "csd", &Uint8Array::from(csd.as_slice()).into());
                }
                droidmirror_client::Out::Frame {
                    pts_us,
                    keyframe,
                    nal,
                } => {
                    set(&obj, "kind", &JsValue::from_str("frame"));
                    set(&obj, "pts", &JsValue::from(pts_us as f64));
                    set(&obj, "keyframe", &JsValue::from(keyframe));
                    set(&obj, "nal", &Uint8Array::from(nal.as_slice()).into());
                }
                droidmirror_client::Out::DeviceName(name) => {
                    set(&obj, "kind", &JsValue::from_str("name"));
                    set(&obj, "name", &JsValue::from_str(&name));
                }
                droidmirror_client::Out::Error(message) => {
                    set(&obj, "kind", &JsValue::from_str("error"));
                    set(&obj, "message", &JsValue::from_str(&message));
                }
            }
            arr.push(&obj);
        }
        arr.into()
    }

    pub fn touch(
        &self,
        action: u8,
        id: u8,
        css_x: f32,
        css_y: f32,
        view_w: f32,
        view_h: f32,
        pressure: f32,
    ) -> Vec<u8> {
        self.inner.touch(touch_action(action), id, css_x, css_y, view_w, view_h, pressure)
    }

    pub fn key(&self, action: u8, keycode: u32, meta: u32) -> Vec<u8> {
        self.inner.key(key_action(action), keycode, meta)
    }

    /// BACK / HOME / RECENTS / POWER / volume (`0x15` on the wire).
    pub fn nav(&self, keycode: u32, action: u8) -> Vec<u8> {
        self.inner.nav(keycode, key_action(action))
    }

    pub fn text(&self, s: &str) -> Vec<u8> {
        self.inner.text(s)
    }

    pub fn scroll(
        &self,
        css_x: f32,
        css_y: f32,
        view_w: f32,
        view_h: f32,
        hscroll: f32,
        vscroll: f32,
    ) -> Vec<u8> {
        self.inner
            .scroll(css_x, css_y, view_w, view_h, hscroll, vscroll)
    }

    pub fn set_encoding(&self, bitrate: u32, max_fps: u32, max_size: u16) -> Vec<u8> {
        self.inner.set_encoding(bitrate, max_fps, max_size)
    }

    pub fn pause(&self, on: bool) -> Vec<u8> {
        self.inner.pause(on)
    }

    pub fn request_keyframe_skip(&mut self) {
        self.inner.request_keyframe_skip();
    }

    pub fn caps(&self) -> u8 {
        self.inner.caps()
    }

    pub fn width(&self) -> u16 {
        self.inner.video_size().0
    }

    pub fn height(&self) -> u16 {
        self.inner.video_size().1
    }
}

fn set(obj: &Object, key: &str, value: &JsValue) {
    let _ = Reflect::set(obj, &JsValue::from_str(key), value);
}

fn webcodecs(codec: Codec) -> &'static str {
    codec.webcodecs_id()
}

fn touch_action(v: u8) -> TouchAction {
    match v {
        0 => TouchAction::Down,
        1 => TouchAction::Up,
        2 => TouchAction::Move,
        _ => TouchAction::Cancel,
    }
}

fn key_action(v: u8) -> KeyAction {
    if v == 0 {
        KeyAction::Down
    } else {
        KeyAction::Up
    }
}
