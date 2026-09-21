//! Wire protocol shared by the device server and both hosts.
//!
//! Handshake (once, server → client) is a raw prefix. Everything after it is
//! framed like `goauld-proto`: `[u32 LE total_len][u8 type][payload]`, where
//! `total_len` counts the type byte plus the payload.
//!
//! No I/O, no threads, no async. Safe to compile for `wasm32`.

use byteorder::{ByteOrder, LittleEndian};
use thiserror::Error;

pub const MAGIC: &[u8; 4] = b"DMIR";
pub const VERSION: u8 = 1;

/// Largest framed message we will accept (video access units included).
pub const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024;
pub const MAX_CSD_LEN: usize = 64 * 1024;
pub const MAX_NAME_LEN: usize = 256;
pub const MAX_TEXT_LEN: usize = 4096;

/// Scroll components are 16.16 fixed-point on the wire.
pub const SCROLL_SCALE: f32 = 65536.0;

pub const FLAG_KEYFRAME: u8 = 0x01;

pub const CAP_UINPUT: u8 = 1 << 0;
pub const CAP_H265: u8 = 1 << 1;
pub const CAP_AUDIO: u8 = 1 << 2;

pub const TAG_VIDEO_FRAME: u8 = 0x01;
pub const TAG_CONFIGURE: u8 = 0x02;
pub const TAG_TOUCH: u8 = 0x10;
pub const TAG_KEY: u8 = 0x11;
pub const TAG_TEXT: u8 = 0x12;
pub const TAG_SCROLL: u8 = 0x13;
pub const TAG_SET_ENCODING: u8 = 0x14;
pub const TAG_KEY_SYN: u8 = 0x15;
pub const TAG_AUDIO: u8 = 0x20;
pub const TAG_PAUSE: u8 = 0x7F;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264 = 1,
    H265 = 2,
}

impl Codec {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::H264),
            2 => Some(Self::H265),
            _ => None,
        }
    }

    /// WebCodecs `codec` string. H.264 is constrained baseline / high-compatible
    /// (`avc1.42E01E` is a widely accepted default when the exact profile is unknown).
    pub fn webcodecs_id(self) -> &'static str {
        match self {
            Self::H264 => "avc1.42E01E",
            Self::H265 => "hev1.1.6.L93.B0",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub version: u8,
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub csd: Vec<u8>,
    pub device_name: String,
    pub caps: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    VideoFrame {
        pts_us: u64,
        keyframe: bool,
        nal: Vec<u8>,
    },
    Configure {
        codec: Codec,
        width: u16,
        height: u16,
        csd: Vec<u8>,
    },
    Touch {
        action: u8,
        pointer_id: u8,
        x: i32,
        y: i32,
        pressure: u16,
    },
    Key {
        action: u8,
        keycode: u32,
        meta: u32,
    },
    Text(String),
    Scroll {
        x: i32,
        y: i32,
        h: i32,
        v: i32,
    },
    SetEncoding {
        bitrate: u32,
        max_fps: u32,
        max_size: u16,
    },
    /// Navigation keys (BACK / HOME / RECENTS / POWER / volume).
    KeySyn {
        keycode: u32,
        action: u8,
    },
    PauseResume {
        on: bool,
    },
    /// `0x20` audio, or any tag this build does not interpret. Payload is kept
    /// so a peer can round-trip it; hosts ignore it.
    Reserved {
        tag: u8,
        payload: Vec<u8>,
    },
}

impl Message {
    pub fn tag(&self) -> u8 {
        match self {
            Self::VideoFrame { .. } => TAG_VIDEO_FRAME,
            Self::Configure { .. } => TAG_CONFIGURE,
            Self::Touch { .. } => TAG_TOUCH,
            Self::Key { .. } => TAG_KEY,
            Self::Text(_) => TAG_TEXT,
            Self::Scroll { .. } => TAG_SCROLL,
            Self::SetEncoding { .. } => TAG_SET_ENCODING,
            Self::KeySyn { .. } => TAG_KEY_SYN,
            Self::PauseResume { .. } => TAG_PAUSE,
            Self::Reserved { tag, .. } => *tag,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let payload = self.encode_payload()?;
        encode_frame(self.tag(), &payload)
    }

    fn encode_payload(&self) -> Result<Vec<u8>, ProtoError> {
        match self {
            Self::VideoFrame {
                pts_us,
                keyframe,
                nal,
            } => {
                if nal.len() > MAX_FRAME_LEN as usize {
                    return Err(ProtoError::BadLength(nal.len() as u32));
                }
                let mut p = Vec::with_capacity(8 + 1 + 4 + nal.len());
                p.extend_from_slice(&pts_us.to_le_bytes());
                p.push(if *keyframe { FLAG_KEYFRAME } else { 0 });
                p.extend_from_slice(&(nal.len() as u32).to_le_bytes());
                p.extend_from_slice(nal);
                Ok(p)
            }
            Self::Configure {
                codec,
                width,
                height,
                csd,
            } => encode_config(*codec, *width, *height, csd),
            Self::Touch {
                action,
                pointer_id,
                x,
                y,
                pressure,
            } => {
                let mut p = Vec::with_capacity(12);
                p.push(*action);
                p.push(*pointer_id);
                p.extend_from_slice(&x.to_le_bytes());
                p.extend_from_slice(&y.to_le_bytes());
                p.extend_from_slice(&pressure.to_le_bytes());
                Ok(p)
            }
            Self::Key {
                action,
                keycode,
                meta,
            } => {
                let mut p = Vec::with_capacity(9);
                p.push(*action);
                p.extend_from_slice(&keycode.to_le_bytes());
                p.extend_from_slice(&meta.to_le_bytes());
                Ok(p)
            }
            Self::Text(s) => {
                let b = s.as_bytes();
                if b.len() > u16::MAX as usize {
                    return Err(ProtoError::BadLength(b.len() as u32));
                }
                let mut p = Vec::with_capacity(2 + b.len());
                p.extend_from_slice(&(b.len() as u16).to_le_bytes());
                p.extend_from_slice(b);
                Ok(p)
            }
            Self::Scroll { x, y, h, v } => {
                let mut p = Vec::with_capacity(16);
                p.extend_from_slice(&x.to_le_bytes());
                p.extend_from_slice(&y.to_le_bytes());
                p.extend_from_slice(&h.to_le_bytes());
                p.extend_from_slice(&v.to_le_bytes());
                Ok(p)
            }
            Self::SetEncoding {
                bitrate,
                max_fps,
                max_size,
            } => {
                let mut p = Vec::with_capacity(10);
                p.extend_from_slice(&bitrate.to_le_bytes());
                p.extend_from_slice(&max_fps.to_le_bytes());
                p.extend_from_slice(&max_size.to_le_bytes());
                Ok(p)
            }
            Self::KeySyn { keycode, action } => {
                let mut p = Vec::with_capacity(5);
                p.extend_from_slice(&keycode.to_le_bytes());
                p.push(*action);
                Ok(p)
            }
            Self::PauseResume { on } => Ok(vec![u8::from(*on)]),
            Self::Reserved { payload, .. } => Ok(payload.clone()),
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported version {0}")]
    BadVersion(u8),
    #[error("unknown codec {0}")]
    BadCodec(u8),
    #[error("truncated: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("invalid length {0}")]
    BadLength(u32),
    #[error("malformed payload for tag 0x{0:02x}")]
    Malformed(u8),
    #[error("text is not utf-8")]
    BadUtf8,
}

pub fn encode_pressure(pressure: f32) -> u16 {
    (pressure.clamp(0.0, 1.0) * 65535.0).round() as u16
}

pub fn decode_pressure(pressure: u16) -> f32 {
    pressure as f32 / 65535.0
}

pub fn encode_scroll(v: f32) -> i32 {
    (v * SCROLL_SCALE).round() as i32
}

pub fn decode_scroll(v: i32) -> f32 {
    v as f32 / SCROLL_SCALE
}

pub fn encode_handshake(h: &Handshake) -> Result<Vec<u8>, ProtoError> {
    if h.csd.len() > MAX_CSD_LEN || h.csd.len() > u16::MAX as usize {
        return Err(ProtoError::BadLength(h.csd.len() as u32));
    }
    let name = h.device_name.as_bytes();
    if name.len() > MAX_NAME_LEN || name.len() > u16::MAX as usize {
        return Err(ProtoError::BadLength(name.len() as u32));
    }
    let mut out = Vec::with_capacity(13 + h.csd.len() + name.len());
    out.extend_from_slice(MAGIC);
    out.push(h.version);
    out.push(h.codec as u8);
    out.extend_from_slice(&h.width.to_le_bytes());
    out.extend_from_slice(&h.height.to_le_bytes());
    out.extend_from_slice(&(h.csd.len() as u16).to_le_bytes());
    out.extend_from_slice(&h.csd);
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(name);
    out.push(h.caps);
    Ok(out)
}

/// `Ok(None)` means `buf` does not yet contain a full handshake.
pub fn try_decode_handshake(buf: &[u8]) -> Result<Option<(Handshake, usize)>, ProtoError> {
    if buf.len() >= 4 && &buf[0..4] != MAGIC {
        return Err(ProtoError::BadMagic);
    }
    if buf.len() < 12 {
        return Ok(None);
    }
    let version = buf[4];
    if version != VERSION {
        return Err(ProtoError::BadVersion(version));
    }
    let codec = Codec::from_u8(buf[5]).ok_or(ProtoError::BadCodec(buf[5]))?;
    let width = LittleEndian::read_u16(&buf[6..8]);
    let height = LittleEndian::read_u16(&buf[8..10]);
    let csd_len = LittleEndian::read_u16(&buf[10..12]) as usize;
    if csd_len > MAX_CSD_LEN {
        return Err(ProtoError::BadLength(csd_len as u32));
    }
    let name_len_at = 12 + csd_len;
    if buf.len() < name_len_at + 2 {
        return Ok(None);
    }
    let name_len = LittleEndian::read_u16(&buf[name_len_at..name_len_at + 2]) as usize;
    if name_len > MAX_NAME_LEN {
        return Err(ProtoError::BadLength(name_len as u32));
    }
    let total = name_len_at + 2 + name_len + 1;
    if buf.len() < total {
        return Ok(None);
    }
    let csd = buf[12..12 + csd_len].to_vec();
    let name_bytes = &buf[name_len_at + 2..name_len_at + 2 + name_len];
    let device_name = String::from_utf8(name_bytes.to_vec()).map_err(|_| ProtoError::BadUtf8)?;
    let caps = buf[total - 1];
    Ok(Some((
        Handshake {
            version,
            codec,
            width,
            height,
            csd,
            device_name,
            caps,
        },
        total,
    )))
}

pub fn encode_frame(tag: u8, payload: &[u8]) -> Result<Vec<u8>, ProtoError> {
    let total_len = 1u32
        .checked_add(payload.len() as u32)
        .ok_or(ProtoError::BadLength(u32::MAX))?;
    if total_len > MAX_FRAME_LEN {
        return Err(ProtoError::BadLength(total_len));
    }
    let mut out = Vec::with_capacity(4 + total_len as usize);
    out.extend_from_slice(&total_len.to_le_bytes());
    out.push(tag);
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn try_decode_frame(buf: &[u8]) -> Result<Option<(Message, usize)>, ProtoError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let total_len = LittleEndian::read_u32(&buf[0..4]);
    if total_len < 1 || total_len > MAX_FRAME_LEN {
        return Err(ProtoError::BadLength(total_len));
    }
    let total = total_len as usize;
    if buf.len() < 4 + total {
        return Ok(None);
    }
    let tag = buf[4];
    let payload = &buf[5..4 + total];
    let msg = decode_payload(tag, payload)?;
    Ok(Some((msg, 4 + total)))
}

fn decode_payload(tag: u8, payload: &[u8]) -> Result<Message, ProtoError> {
    match tag {
        TAG_VIDEO_FRAME => {
            if payload.len() < 13 {
                return Err(ProtoError::Malformed(tag));
            }
            let pts_us = u64::from_le_bytes(payload[0..8].try_into().unwrap());
            let flags = payload[8];
            let nal_len = u32::from_le_bytes(payload[9..13].try_into().unwrap()) as usize;
            if payload.len() != 13 + nal_len {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::VideoFrame {
                pts_us,
                keyframe: flags & FLAG_KEYFRAME != 0,
                nal: payload[13..].to_vec(),
            })
        }
        TAG_CONFIGURE => {
            if payload.len() < 7 {
                return Err(ProtoError::Malformed(tag));
            }
            let codec = Codec::from_u8(payload[0]).ok_or(ProtoError::BadCodec(payload[0]))?;
            let width = u16::from_le_bytes(payload[1..3].try_into().unwrap());
            let height = u16::from_le_bytes(payload[3..5].try_into().unwrap());
            let csd_len = u16::from_le_bytes(payload[5..7].try_into().unwrap()) as usize;
            if csd_len > MAX_CSD_LEN || payload.len() != 7 + csd_len {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::Configure {
                codec,
                width,
                height,
                csd: payload[7..].to_vec(),
            })
        }
        TAG_TOUCH => {
            if payload.len() != 12 {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::Touch {
                action: payload[0],
                pointer_id: payload[1],
                x: i32::from_le_bytes(payload[2..6].try_into().unwrap()),
                y: i32::from_le_bytes(payload[6..10].try_into().unwrap()),
                pressure: u16::from_le_bytes(payload[10..12].try_into().unwrap()),
            })
        }
        TAG_KEY => {
            if payload.len() != 9 {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::Key {
                action: payload[0],
                keycode: u32::from_le_bytes(payload[1..5].try_into().unwrap()),
                meta: u32::from_le_bytes(payload[5..9].try_into().unwrap()),
            })
        }
        TAG_TEXT => {
            if payload.len() < 2 {
                return Err(ProtoError::Malformed(tag));
            }
            let n = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
            if payload.len() != 2 + n {
                return Err(ProtoError::Malformed(tag));
            }
            let s = std::str::from_utf8(&payload[2..]).map_err(|_| ProtoError::BadUtf8)?;
            Ok(Message::Text(s.to_string()))
        }
        TAG_SCROLL => {
            if payload.len() != 16 {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::Scroll {
                x: i32::from_le_bytes(payload[0..4].try_into().unwrap()),
                y: i32::from_le_bytes(payload[4..8].try_into().unwrap()),
                h: i32::from_le_bytes(payload[8..12].try_into().unwrap()),
                v: i32::from_le_bytes(payload[12..16].try_into().unwrap()),
            })
        }
        TAG_SET_ENCODING => {
            if payload.len() != 10 {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::SetEncoding {
                bitrate: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
                max_fps: u32::from_le_bytes(payload[4..8].try_into().unwrap()),
                max_size: u16::from_le_bytes(payload[8..10].try_into().unwrap()),
            })
        }
        TAG_KEY_SYN => {
            if payload.len() != 5 {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::KeySyn {
                keycode: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
                action: payload[4],
            })
        }
        TAG_PAUSE => {
            if payload.len() != 1 {
                return Err(ProtoError::Malformed(tag));
            }
            Ok(Message::PauseResume { on: payload[0] != 0 })
        }
        tag => Ok(Message::Reserved {
            tag,
            payload: payload.to_vec(),
        }),
    }
}

fn encode_config(codec: Codec, width: u16, height: u16, csd: &[u8]) -> Result<Vec<u8>, ProtoError> {
    if csd.len() > MAX_CSD_LEN || csd.len() > u16::MAX as usize {
        return Err(ProtoError::BadLength(csd.len() as u32));
    }
    let mut p = Vec::with_capacity(7 + csd.len());
    p.push(codec as u8);
    p.extend_from_slice(&width.to_le_bytes());
    p.extend_from_slice(&height.to_le_bytes());
    p.extend_from_slice(&(csd.len() as u16).to_le_bytes());
    p.extend_from_slice(csd);
    Ok(p)
}

/// Convert length-prefixed AVCC NALs (or raw SPS/PPS) into Annex-B.
/// Data that already starts with a start code is returned unchanged.
pub fn to_annex_b(data: &[u8]) -> Vec<u8> {
    if data.is_empty() || has_start_code(data) {
        return data.to_vec();
    }
    // AVCC: repeated [u32 length][nal]. Reject if the lengths don't cover the buffer.
    if looks_like_avcc(data) {
        let mut out = Vec::with_capacity(data.len() + 16);
        let mut i = 0;
        while i + 4 <= data.len() {
            let n = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            if n == 0 || i + n > data.len() {
                break;
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&data[i..i + n]);
            i += n;
        }
        if i == data.len() && !out.is_empty() {
            return out;
        }
    }
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(data);
    out
}

fn has_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

fn looks_like_avcc(data: &[u8]) -> bool {
    if data.len() < 5 {
        return false;
    }
    let n = u32::from_be_bytes(data[0..4].try_into().unwrap()) as usize;
    n > 0 && n < data.len() && 4 + n <= data.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_handshake() -> Handshake {
        Handshake {
            version: VERSION,
            codec: Codec::H264,
            width: 1080,
            height: 2400,
            csd: vec![0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce],
            device_name: "Pixel".into(),
            caps: CAP_UINPUT | CAP_H265,
        }
    }

    fn roundtrip(msg: Message) {
        let bytes = msg.encode().unwrap();
        assert!(try_decode_frame(&bytes[..bytes.len() - 1]).unwrap().is_none());
        let (decoded, n) = try_decode_frame(&bytes).unwrap().unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(decoded, msg);
    }

    #[test]
    fn handshake_roundtrip_and_partial() {
        let h = sample_handshake();
        let bytes = encode_handshake(&h).unwrap();
        assert!(try_decode_handshake(&bytes[..4]).unwrap().is_none());
        assert!(try_decode_handshake(&bytes[..bytes.len() - 1])
            .unwrap()
            .is_none());
        let (decoded, n) = try_decode_handshake(&bytes).unwrap().unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(decoded, h);
    }

    #[test]
    fn handshake_rejects_bad_magic() {
        let mut bytes = encode_handshake(&sample_handshake()).unwrap();
        bytes[0] = b'X';
        assert!(matches!(
            try_decode_handshake(&bytes),
            Err(ProtoError::BadMagic)
        ));
    }

    #[test]
    fn every_message_roundtrips() {
        roundtrip(Message::VideoFrame {
            pts_us: 42,
            keyframe: true,
            nal: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
        });
        roundtrip(Message::VideoFrame {
            pts_us: 0,
            keyframe: false,
            nal: vec![0, 0, 1, 0x41],
        });
        roundtrip(Message::Configure {
            codec: Codec::H265,
            width: 720,
            height: 1280,
            csd: vec![1, 2, 3, 4],
        });
        roundtrip(Message::Touch {
            action: 0,
            pointer_id: 3,
            x: 10,
            y: -4,
            pressure: 1000,
        });
        roundtrip(Message::Key {
            action: 1,
            keycode: 66,
            meta: 0x1000,
        });
        roundtrip(Message::Text("héllo".into()));
        roundtrip(Message::Scroll {
            x: 1,
            y: 2,
            h: encode_scroll(-1.5),
            v: encode_scroll(0.25),
        });
        roundtrip(Message::SetEncoding {
            bitrate: 8_000_000,
            max_fps: 60,
            max_size: 1080,
        });
        roundtrip(Message::KeySyn {
            keycode: 4,
            action: 0,
        });
        roundtrip(Message::PauseResume { on: true });
        roundtrip(Message::Reserved {
            tag: TAG_AUDIO,
            payload: vec![9, 9],
        });
    }

    #[test]
    fn scroll_fixed_point() {
        assert_eq!(decode_scroll(encode_scroll(-1.5)), -1.5);
        assert_eq!(decode_scroll(encode_scroll(0.25)), 0.25);
    }

    #[test]
    fn annex_b_passthrough_and_avcc() {
        let annex = [0u8, 0, 0, 1, 0x65, 9];
        assert_eq!(to_annex_b(&annex), annex);
        let avcc = {
            let nal = [0x65u8, 9, 8];
            let mut v = (nal.len() as u32).to_be_bytes().to_vec();
            v.extend_from_slice(&nal);
            v
        };
        assert_eq!(to_annex_b(&avcc), vec![0, 0, 0, 1, 0x65, 9, 8]);
        assert_eq!(to_annex_b(&[0x67, 0x42]), vec![0, 0, 0, 1, 0x67, 0x42]);
    }

    #[test]
    fn concatenated_handshake_and_frame() {
        let mut buf = encode_handshake(&sample_handshake()).unwrap();
        let frame = Message::VideoFrame {
            pts_us: 1,
            keyframe: true,
            nal: vec![1, 2],
        }
        .encode()
        .unwrap();
        buf.extend_from_slice(&frame);
        let (_, hn) = try_decode_handshake(&buf).unwrap().unwrap();
        let (msg, fn_) = try_decode_frame(&buf[hn..]).unwrap().unwrap();
        assert_eq!(hn + fn_, buf.len());
        assert!(matches!(msg, Message::VideoFrame { keyframe: true, .. }));
    }
}
