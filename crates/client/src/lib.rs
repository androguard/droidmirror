//! Sans-IO mirror core.
//!
//! Bytes in, events out. Control calls return bytes the host must write.
//! No transport, decoder, windowing, threads, async, or `wasm-bindgen`.

use droidmirror_proto::{
    decode_scroll, encode_handshake, encode_pressure, encode_scroll, try_decode_frame,
    try_decode_handshake, Codec, Handshake, Message, ProtoError, CAP_AUDIO, CAP_H265, CAP_UINPUT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchAction {
    Down = 0,
    Up = 1,
    Move = 2,
    Cancel = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Down = 0,
    Up = 1,
}

/// Android meta-state bits carried in [`Client::key`].
pub const META_SHIFT: u32 = 0x1;
pub const META_ALT: u32 = 0x2;
pub const META_CTRL: u32 = 0x1000;
pub const META_META: u32 = 0x10000;

/// Common navigation keycodes (Android `KeyEvent`).
pub const KEY_HOME: u32 = 3;
pub const KEY_BACK: u32 = 4;
pub const KEY_VOLUME_UP: u32 = 24;
pub const KEY_VOLUME_DOWN: u32 = 25;
pub const KEY_POWER: u32 = 26;
pub const KEY_APP_SWITCH: u32 = 187;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Out {
    /// Decoder must be (re)configured.
    Configure {
        codec: Codec,
        csd: Vec<u8>,
        width: u16,
        height: u16,
    },
    Frame {
        pts_us: u64,
        keyframe: bool,
        nal: Vec<u8>,
    },
    DeviceName(String),
    Error(String),
}

#[derive(Debug)]
pub struct Client {
    buf: Vec<u8>,
    handshake: bool,
    fatal: bool,
    width: u16,
    height: u16,
    /// Quarter-turns clockwise of the *displayed* frame relative to the buffer.
    /// Zero when `Configure` already swapped `width`/`height` to match the view.
    rotation: u8,
    codec: Option<Codec>,
    csd: Vec<u8>,
    device_name: String,
    caps: u8,
    skip_until_key: bool,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            handshake: false,
            fatal: false,
            width: 0,
            height: 0,
            rotation: 0,
            codec: None,
            csd: Vec::new(),
            device_name: String::new(),
            caps: 0,
            skip_until_key: false,
        }
    }

    pub fn video_size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn rotation(&self) -> u8 {
        self.rotation
    }

    /// Extra display rotation, in clockwise quarter-turns, when the host rotates
    /// the view without the frame buffer itself being rotated.
    pub fn set_rotation(&mut self, quarters_clockwise: u8) {
        self.rotation = quarters_clockwise & 3;
    }

    pub fn codec(&self) -> Option<Codec> {
        self.codec
    }

    pub fn csd(&self) -> &[u8] {
        &self.csd
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn caps(&self) -> u8 {
        self.caps
    }

    pub fn has_uinput(&self) -> bool {
        self.caps & CAP_UINPUT != 0
    }

    pub fn has_h265(&self) -> bool {
        self.caps & CAP_H265 != 0
    }

    pub fn has_audio(&self) -> bool {
        self.caps & CAP_AUDIO != 0
    }

    /// Drop non-keyframes until the next IDR. Called when the decoder queue is behind.
    pub fn request_keyframe_skip(&mut self) {
        self.skip_until_key = true;
    }

    pub fn on_bytes(&mut self, buf: &[u8]) -> Vec<Out> {
        if self.fatal {
            return Vec::new();
        }
        self.buf.extend_from_slice(buf);
        let mut out = Vec::new();
        if !self.handshake {
            match try_decode_handshake(&self.buf) {
                Ok(None) => return out,
                Ok(Some((h, n))) => {
                    self.buf.drain(..n);
                    self.apply_handshake(h, &mut out);
                }
                Err(e) => {
                    self.fail(e, &mut out);
                    return out;
                }
            }
        }
        loop {
            match try_decode_frame(&self.buf) {
                Ok(None) => break,
                Ok(Some((msg, n))) => {
                    self.buf.drain(..n);
                    self.apply_message(msg, &mut out);
                }
                Err(e) => {
                    self.fail(e, &mut out);
                    break;
                }
            }
        }
        out
    }

    fn fail(&mut self, e: ProtoError, out: &mut Vec<Out>) {
        self.fatal = true;
        self.buf.clear();
        out.push(Out::Error(e.to_string()));
    }

    fn apply_handshake(&mut self, h: Handshake, out: &mut Vec<Out>) {
        self.handshake = true;
        self.width = h.width;
        self.height = h.height;
        self.codec = Some(h.codec);
        self.csd = h.csd.clone();
        self.device_name = h.device_name.clone();
        self.caps = h.caps;
        out.push(Out::DeviceName(h.device_name));
        out.push(Out::Configure {
            codec: h.codec,
            csd: h.csd,
            width: h.width,
            height: h.height,
        });
    }

    fn apply_message(&mut self, msg: Message, out: &mut Vec<Out>) {
        match msg {
            Message::Configure {
                codec,
                width,
                height,
                csd,
            } => {
                self.codec = Some(codec);
                self.width = width;
                self.height = height;
                if !csd.is_empty() {
                    self.csd = csd.clone();
                }
                out.push(Out::Configure {
                    codec,
                    csd,
                    width,
                    height,
                });
            }
            Message::VideoFrame {
                pts_us,
                keyframe,
                nal,
            } => {
                if self.skip_until_key && !keyframe {
                    return;
                }
                if keyframe {
                    self.skip_until_key = false;
                }
                out.push(Out::Frame {
                    pts_us,
                    keyframe,
                    nal,
                });
            }
            Message::Reserved { .. } => {}
            other => {
                out.push(Out::Error(format!(
                    "unexpected client-bound message 0x{:02x}",
                    other.tag()
                )));
            }
        }
    }

    pub fn touch(
        &self,
        action: TouchAction,
        id: u8,
        css_x: f32,
        css_y: f32,
        view_w: f32,
        view_h: f32,
        pressure: f32,
    ) -> Vec<u8> {
        let Some((x, y)) = map_to_device(
            css_x,
            css_y,
            view_w,
            view_h,
            self.width,
            self.height,
            self.rotation,
        ) else {
            return Vec::new();
        };
        encode(&Message::Touch {
            action: action as u8,
            pointer_id: id,
            x,
            y,
            pressure: encode_pressure(pressure),
        })
    }

    pub fn key(&self, action: KeyAction, keycode: u32, meta: u32) -> Vec<u8> {
        encode(&Message::Key {
            action: action as u8,
            keycode,
            meta,
        })
    }

    /// BACK / HOME / RECENTS / POWER / volume. Same actions as [`KeyAction`].
    pub fn nav(&self, keycode: u32, action: KeyAction) -> Vec<u8> {
        encode(&Message::KeySyn {
            keycode,
            action: action as u8,
        })
    }

    pub fn text(&self, s: &str) -> Vec<u8> {
        encode(&Message::Text(s.to_string()))
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
        let Some((x, y)) = map_to_device(
            css_x,
            css_y,
            view_w,
            view_h,
            self.width,
            self.height,
            self.rotation,
        ) else {
            return Vec::new();
        };
        encode(&Message::Scroll {
            x,
            y,
            h: encode_scroll(hscroll),
            v: encode_scroll(vscroll),
        })
    }

    pub fn set_encoding(&self, bitrate: u32, max_fps: u32, max_size: u16) -> Vec<u8> {
        encode(&Message::SetEncoding {
            bitrate,
            max_fps,
            max_size,
        })
    }

    pub fn pause(&self, on: bool) -> Vec<u8> {
        encode(&Message::PauseResume { on })
    }
}

fn encode(msg: &Message) -> Vec<u8> {
    msg.encode().unwrap_or_default()
}

/// Letterboxed `css` point → device pixel inside the current frame.
///
/// `rotation` is clockwise quarter-turns of the *display* relative to the
/// frame buffer. Use `0` when `width`/`height` already match the oriented video.
pub fn map_to_device(
    css_x: f32,
    css_y: f32,
    view_w: f32,
    view_h: f32,
    frame_w: u16,
    frame_h: u16,
    rotation: u8,
) -> Option<(i32, i32)> {
    if view_w <= 0.0 || view_h <= 0.0 || frame_w == 0 || frame_h == 0 {
        return None;
    }
    let fw = frame_w as f32;
    let fh = frame_h as f32;
    let (disp_w_px, disp_h_px) = if rotation & 1 == 1 {
        (fh, fw)
    } else {
        (fw, fh)
    };
    let view_aspect = view_w / view_h;
    let frame_aspect = disp_w_px / disp_h_px;
    let (box_w, box_h, off_x, off_y) = if view_aspect > frame_aspect {
        let box_h = view_h;
        let box_w = view_h * frame_aspect;
        (box_w, box_h, (view_w - box_w) * 0.5, 0.0)
    } else {
        let box_w = view_w;
        let box_h = view_w / frame_aspect;
        (box_w, box_h, 0.0, (view_h - box_h) * 0.5)
    };
    if box_w <= 0.0 || box_h <= 0.0 {
        return None;
    }
    let nx = ((css_x - off_x) / box_w).clamp(0.0, 1.0);
    let ny = ((css_y - off_y) / box_h).clamp(0.0, 1.0);
    let (nx, ny) = apply_rotation(nx, ny, rotation);
    let x = (nx * fw).round() as i32;
    let y = (ny * fh).round() as i32;
    Some((
        x.clamp(0, frame_w as i32 - 1),
        y.clamp(0, frame_h as i32 - 1),
    ))
}

fn apply_rotation(nx: f32, ny: f32, quarters: u8) -> (f32, f32) {
    match quarters & 3 {
        0 => (nx, ny),
        1 => (ny, 1.0 - nx),
        2 => (1.0 - nx, 1.0 - ny),
        _ => (1.0 - ny, nx),
    }
}

/// Build a handshake the tests (and a fake server) can feed to [`Client::on_bytes`].
pub fn handshake_bytes(h: &Handshake) -> Vec<u8> {
    encode_handshake(h).expect("handshake")
}

pub fn scroll_float(v: i32) -> f32 {
    decode_scroll(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use droidmirror_proto::{try_decode_frame, Handshake, CAP_UINPUT, VERSION};

    fn hs() -> Handshake {
        Handshake {
            version: VERSION,
            codec: Codec::H264,
            width: 100,
            height: 200,
            csd: vec![0, 0, 0, 1, 0x67],
            device_name: "dev".into(),
            caps: CAP_UINPUT,
        }
    }

    fn primed() -> Client {
        let mut c = Client::new();
        let out = c.on_bytes(&handshake_bytes(&hs()));
        assert!(matches!(out[0], Out::DeviceName(_)));
        assert!(matches!(out[1], Out::Configure { width: 100, height: 200, .. }));
        c
    }

    #[test]
    fn chunked_handshake_then_frames_and_skip() {
        let mut c = Client::new();
        let mut bytes = handshake_bytes(&hs());
        let delta = Message::VideoFrame {
            pts_us: 1,
            keyframe: false,
            nal: vec![0x41],
        }
        .encode()
        .unwrap();
        let idr = Message::VideoFrame {
            pts_us: 2,
            keyframe: true,
            nal: vec![0x65],
        }
        .encode()
        .unwrap();
        bytes.extend_from_slice(&delta);
        bytes.extend_from_slice(&idr);

        assert!(c.on_bytes(&bytes[..3]).is_empty());
        let mut got_name = false;
        let mut frames = 0;
        for chunk in bytes[3..].chunks(5) {
            for ev in c.on_bytes(chunk) {
                match ev {
                    Out::DeviceName(_) => got_name = true,
                    Out::Frame { .. } => frames += 1,
                    _ => {}
                }
            }
        }
        assert!(got_name);
        assert_eq!(frames, 2);

        c.request_keyframe_skip();
        let dropped = c.on_bytes(
            &Message::VideoFrame {
                pts_us: 3,
                keyframe: false,
                nal: vec![9],
            }
            .encode()
            .unwrap(),
        );
        assert!(dropped.iter().all(|o| !matches!(o, Out::Frame { .. })));
        let kept = c.on_bytes(
            &Message::VideoFrame {
                pts_us: 4,
                keyframe: true,
                nal: vec![8],
            }
            .encode()
            .unwrap(),
        );
        assert!(matches!(
            kept[0],
            Out::Frame {
                keyframe: true,
                pts_us: 4,
                ..
            }
        ));
        let _ = frames;
    }

    #[test]
    fn configure_updates_size_used_by_touch() {
        let mut c = primed();
        let msg = Message::Configure {
            codec: Codec::H264,
            width: 200,
            height: 100,
            csd: vec![1],
        };
        let out = c.on_bytes(&msg.encode().unwrap());
        assert!(matches!(
            out[0],
            Out::Configure {
                width: 200,
                height: 100,
                ..
            }
        ));
        let bytes = c.touch(TouchAction::Down, 0, 0.0, 0.0, 200.0, 100.0, 1.0);
        let (m, _) = try_decode_frame(&bytes).unwrap().unwrap();
        match m {
            Message::Touch { x, y, pressure, .. } => {
                assert_eq!((x, y), (0, 0));
                assert_eq!(pressure, 65535);
            }
            _ => panic!("touch"),
        }
    }

    #[test]
    fn letterbox_and_pillarbox() {
        // Portrait frame in a wide view: pillarbox. Center of the view is the frame center.
        let (x, y) = map_to_device(200.0, 50.0, 400.0, 100.0, 100, 200, 0).unwrap();
        assert_eq!((x, y), (50, 100));
        // Point in the left bar clamps to x = 0.
        let (x, _) = map_to_device(0.0, 50.0, 400.0, 100.0, 100, 200, 0).unwrap();
        assert_eq!(x, 0);
        // Landscape frame in a tall view: letterbox. Center maps to center.
        let (x, y) = map_to_device(50.0, 200.0, 100.0, 400.0, 200, 100, 0).unwrap();
        assert_eq!((x, y), (100, 50));
    }

    #[test]
    fn rotation_90_maps_display_top_right_to_origin() {
        // Buffer 100x200, displayed rotated 90 CW so the view aspect is 200x100.
        // Display top-right is buffer (0, 0).
        let (x, y) = map_to_device(200.0, 0.0, 200.0, 100.0, 100, 200, 1).unwrap();
        assert_eq!((x, y), (0, 0));
    }

    #[test]
    fn control_messages_decode() {
        let c = primed();
        let key = c.key(KeyAction::Down, 66, META_CTRL);
        assert!(matches!(
            try_decode_frame(&key).unwrap().unwrap().0,
            Message::Key {
                keycode: 66,
                meta: META_CTRL,
                ..
            }
        ));
        let text = c.text("ok");
        assert!(matches!(
            try_decode_frame(&text).unwrap().unwrap().0,
            Message::Text(s) if s == "ok"
        ));
        let sc = c.scroll(50.0, 100.0, 100.0, 200.0, -1.5, 0.25);
        match try_decode_frame(&sc).unwrap().unwrap().0 {
            Message::Scroll { x, y, h, v } => {
                assert_eq!((x, y), (50, 100));
                assert_eq!(scroll_float(h), -1.5);
                assert_eq!(scroll_float(v), 0.25);
            }
            _ => panic!("scroll"),
        }
        let enc = c.set_encoding(1000, 30, 720);
        assert!(matches!(
            try_decode_frame(&enc).unwrap().unwrap().0,
            Message::SetEncoding {
                bitrate: 1000,
                max_fps: 30,
                max_size: 720,
            }
        ));
        let pause = c.pause(true);
        assert!(matches!(
            try_decode_frame(&pause).unwrap().unwrap().0,
            Message::PauseResume { on: true }
        ));
        let nav = c.nav(KEY_BACK, KeyAction::Down);
        assert!(matches!(
            try_decode_frame(&nav).unwrap().unwrap().0,
            Message::KeySyn { keycode: KEY_BACK, .. }
        ));
    }

    #[test]
    fn degenerate_view_sends_nothing() {
        let c = primed();
        assert!(c
            .touch(TouchAction::Down, 0, 1.0, 1.0, 0.0, 10.0, 1.0)
            .is_empty());
    }

    #[test]
    fn bad_magic_is_fatal() {
        let mut c = Client::new();
        let out = c.on_bytes(b"XXXX more");
        assert!(matches!(out[0], Out::Error(_)));
        assert!(c.on_bytes(b"DMIR").is_empty());
    }
}
