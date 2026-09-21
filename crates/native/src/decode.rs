//! H.264 decode. `openh264` is the portable baseline; H.265 is refused unless a
//! future `ffmpeg` backend is wired up (the flag exists so the host can say so).

use droidmirror_proto::Codec;

#[derive(Clone)]
pub struct RgbaFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub trait Decode {
    fn configure(&mut self, codec: Codec, csd: &[u8]) -> anyhow::Result<()>;
    fn decode(&mut self, nal: &[u8], keyframe: bool) -> anyhow::Result<Option<RgbaFrame>>;
}

pub fn make_decoder() -> Box<dyn Decode + Send> {
    #[cfg(feature = "openh264")]
    {
        Box::new(OpenH264Decoder::new())
    }
    #[cfg(not(feature = "openh264"))]
    {
        Box::new(MissingDecoder)
    }
}

#[cfg(not(feature = "openh264"))]
struct MissingDecoder;

#[cfg(not(feature = "openh264"))]
impl Decode for MissingDecoder {
    fn configure(&mut self, _: Codec, _: &[u8]) -> anyhow::Result<()> {
        anyhow::bail!("built without the openh264 feature")
    }
    fn decode(&mut self, _: &[u8], _: bool) -> anyhow::Result<Option<RgbaFrame>> {
        anyhow::bail!("built without the openh264 feature")
    }
}

#[cfg(feature = "openh264")]
struct OpenH264Decoder {
    inner: Option<openh264::decoder::Decoder>,
    codec: Option<Codec>,
    /// Annex-B SPS/PPS from the last configure. Prepended to an IDR that does
    /// not already carry parameter sets.
    csd: Vec<u8>,
    /// After a bitstream error, ignore slices until the next IDR.
    wait_idr: bool,
    logged_fail: bool,
}

#[cfg(feature = "openh264")]
impl OpenH264Decoder {
    fn new() -> Self {
        Self {
            inner: None,
            codec: None,
            csd: Vec::new(),
            wait_idr: false,
            logged_fail: false,
        }
    }

    fn decoder(&mut self) -> anyhow::Result<&mut openh264::decoder::Decoder> {
        if self.inner.is_none() {
            self.inner = Some(openh264::decoder::Decoder::new()?);
        }
        Ok(self.inner.as_mut().unwrap())
    }
}

#[cfg(feature = "openh264")]
impl Decode for OpenH264Decoder {
    fn configure(&mut self, codec: Codec, csd: &[u8]) -> anyhow::Result<()> {
        if codec != Codec::H264 {
            anyhow::bail!("H.265 decode is not in the default build (openh264 is H.264 only)");
        }
        self.codec = Some(codec);
        self.csd = csd.to_vec();
        self.wait_idr = false;
        self.logged_fail = false;
        self.inner = Some(openh264::decoder::Decoder::new()?);
        if !csd.is_empty() {
            // SPS/PPS alone often returns "no param sets" from OpenH264 because
            // there is no picture. The sets are applied again on the next IDR.
            if self.decoder()?.decode(csd).is_err() {
                self.inner = Some(openh264::decoder::Decoder::new()?);
            }
        }
        Ok(())
    }

    fn decode(&mut self, nal: &[u8], keyframe: bool) -> anyhow::Result<Option<RgbaFrame>> {
        if self.codec == Some(Codec::H265) {
            anyhow::bail!("H.265 frame");
        }
        let idr = keyframe || contains_idr(nal);
        if self.wait_idr && !idr {
            return Ok(None);
        }
        let owned;
        let packet: &[u8] = if idr && !contains_sps(nal) && !self.csd.is_empty() {
            owned = [self.csd.as_slice(), nal].concat();
            &owned
        } else {
            nal
        };
        let step = {
            let dec = self.decoder()?;
            match dec.decode(packet) {
                Ok(Some(yuv)) => {
                    use openh264::formats::YUVSource;
                    let (w, h) = yuv.dimensions();
                    if w == 0 || h == 0 {
                        DecodeStep::Empty
                    } else {
                        let mut rgba = vec![0u8; yuv.rgba8_len()];
                        yuv.write_rgba8(&mut rgba);
                        DecodeStep::Frame(RgbaFrame {
                            width: w as u32,
                            height: h as u32,
                            rgba,
                        })
                    }
                }
                Ok(None) => DecodeStep::Empty,
                Err(e) => DecodeStep::Failed(e.to_string()),
            }
        };
        match step {
            DecodeStep::Frame(frame) => {
                self.wait_idr = false;
                self.logged_fail = false;
                Ok(Some(frame))
            }
            DecodeStep::Empty => Ok(None),
            DecodeStep::Failed(e) => {
                self.inner = None;
                self.wait_idr = true;
                if !self.logged_fail {
                    self.logged_fail = true;
                    log::warn!("decode: {e}");
                }
                Ok(None)
            }
        }
    }
}

enum DecodeStep {
    Frame(RgbaFrame),
    Empty,
    Failed(String),
}

/// NAL unit type 5 is an IDR picture.
fn contains_idr(data: &[u8]) -> bool {
    nal_types(data).any(|t| t == 5)
}

/// NAL unit type 7 is SPS.
fn contains_sps(data: &[u8]) -> bool {
    nal_types(data).any(|t| t == 7)
}

fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    start_codes(data).filter_map(|i| data.get(i).map(|b| b & 0x1f))
}

fn start_codes(data: &[u8]) -> impl Iterator<Item = usize> + '_ {
    (0..data.len().saturating_sub(3)).filter_map(|i| {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            Some(i + 4)
        } else if data[i..].starts_with(&[0, 0, 1]) {
            Some(i + 3)
        } else {
            None
        }
    })
}
