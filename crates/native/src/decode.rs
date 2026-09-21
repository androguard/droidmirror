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
    fn decode(&mut self, nal: &[u8]) -> anyhow::Result<Option<RgbaFrame>>;
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
    fn decode(&mut self, _: &[u8]) -> anyhow::Result<Option<RgbaFrame>> {
        anyhow::bail!("built without the openh264 feature")
    }
}

#[cfg(feature = "openh264")]
struct OpenH264Decoder {
    inner: Option<openh264::decoder::Decoder>,
    codec: Option<Codec>,
}

#[cfg(feature = "openh264")]
impl OpenH264Decoder {
    fn new() -> Self {
        Self {
            inner: None,
            codec: None,
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
        self.inner = Some(openh264::decoder::Decoder::new()?);
        if !csd.is_empty() {
            let _ = self.decoder()?.decode(csd)?;
        }
        Ok(())
    }

    fn decode(&mut self, nal: &[u8]) -> anyhow::Result<Option<RgbaFrame>> {
        if self.codec == Some(Codec::H265) {
            anyhow::bail!("H.265 frame");
        }
        let yuv = self.decoder()?.decode(nal)?;
        let Some(yuv) = yuv else {
            return Ok(None);
        };
        use openh264::formats::YUVSource;
        let (w, h) = yuv.dimensions();
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let mut rgba = vec![0u8; yuv.rgba8_len()];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(RgbaFrame {
            width: w as u32,
            height: h as u32,
            rgba,
        }))
    }
}
