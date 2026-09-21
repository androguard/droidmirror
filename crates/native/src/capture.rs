//! PNG screenshot, GIF, and MP4 of the mirror.
//!
//! PNG and GIF come from decoded RGBA. MP4 muxes the H.264 access units the
//! phone already encoded, so it does not re-encode the picture.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use bytes::Bytes;
use mp4::{AvcConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};

pub fn capture_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let desk = PathBuf::from(home).join("Desktop");
        if desk.is_dir() {
            return desk;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn output_path(ext: &str) -> PathBuf {
    capture_dir().join(format!("droidmirror-{}.{ext}", utc_stamp()))
}

pub fn save_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<()> {
    if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
        anyhow::bail!("empty frame");
    }
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .context("png header")?
        .write_image_data(&rgba[..(width * height * 4) as usize])
        .context("png pixels")?;
    Ok(())
}

/// Longest side becomes `max_edge` pixels. Smaller frames are copied as-is.
pub fn fit_max_edge(width: u32, height: u32, rgba: &[u8], max_edge: u32) -> (u32, u32, Vec<u8>) {
    let long = width.max(height).max(1);
    if long <= max_edge || width == 0 || height == 0 {
        return (width, height, rgba.to_vec());
    }
    let scale = max_edge as f32 / long as f32;
    let nw = ((width as f32 * scale).round() as u32).max(1);
    let nh = ((height as f32 * scale).round() as u32).max(1);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let sy = (y * height / nh).min(height - 1);
        for x in 0..nw {
            let sx = (x * width / nw).min(width - 1);
            let si = ((sy * width + sx) * 4) as usize;
            let di = ((y * nw + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&rgba[si..si + 4]);
        }
    }
    (nw, nh, out)
}

pub struct GifFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Hundredths of a second, matching the GIF delay field.
    pub delay_cs: u16,
}

pub fn spawn_gif(path: PathBuf) -> (SyncSender<GifFrame>, JoinHandle<()>) {
    let (tx, rx) = mpsc::sync_channel::<GifFrame>(4);
    let join = std::thread::spawn(move || gif_worker(path, rx));
    (tx, join)
}

fn gif_worker(path: PathBuf, rx: Receiver<GifFrame>) {
    let Ok(first) = rx.recv() else {
        log::info!("gif: no frames");
        return;
    };
    if let Err(e) = write_gif(&path, first, &rx) {
        log::error!("gif: {e:#}");
        let _ = std::fs::remove_file(&path);
        return;
    }
    log::info!("saved {}", path.display());
}

fn write_gif(path: &Path, first: GifFrame, rx: &Receiver<GifFrame>) -> anyhow::Result<()> {
    let w = u16::try_from(first.width).context("gif width")?;
    let h = u16::try_from(first.height).context("gif height")?;
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = gif::Encoder::new(BufWriter::new(file), w, h, &[]).context("gif header")?;
    encoder.set_repeat(gif::Repeat::Infinite).context("gif loop")?;
    write_gif_frame(&mut encoder, first)?;
    while let Ok(frame) = rx.recv() {
        if frame.width != w as u32 || frame.height != h as u32 {
            continue;
        }
        write_gif_frame(&mut encoder, frame)?;
    }
    Ok(())
}

fn write_gif_frame<W: Write>(encoder: &mut gif::Encoder<W>, mut frame: GifFrame) -> anyhow::Result<()> {
    let mut image = gif::Frame::from_rgba_speed(frame.width as u16, frame.height as u16, &mut frame.rgba, 10);
    image.delay = frame.delay_cs.max(2);
    encoder.write_frame(&image).context("gif frame")?;
    Ok(())
}

/// Muxes Annex-B H.264 into an MP4. Samples start at the first IDR.
pub struct Mp4Rec {
    writer: Mp4Writer<BufWriter<File>>,
    path: PathBuf,
    samples: u32,
    wait_idr: bool,
    pending: Option<PendingSample>,
    logged_wait: bool,
}

struct PendingSample {
    pts_us: u64,
    keyframe: bool,
    bytes: Vec<u8>,
}

impl Mp4Rec {
    pub fn start(path: PathBuf, width: u16, height: u16, csd: &[u8]) -> anyhow::Result<Self> {
        let (sps, pps) = sps_pps(csd).context("MP4 capture needs H.264 SPS and PPS")?;
        if width == 0 || height == 0 {
            anyhow::bail!("video size is not known yet");
        }
        let file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
        let config = Mp4Config {
            major_brand: "isom".parse().expect("fourcc"),
            minor_version: 512,
            compatible_brands: vec![
                "isom".parse().expect("fourcc"),
                "iso2".parse().expect("fourcc"),
                "avc1".parse().expect("fourcc"),
                "mp41".parse().expect("fourcc"),
            ],
            timescale: 1000,
        };
        let mut writer = Mp4Writer::write_start(BufWriter::new(file), &config).context("mp4 header")?;
        writer
            .add_track(&TrackConfig::from(AvcConfig {
                width,
                height,
                seq_param_set: sps,
                pic_param_set: pps,
            }))
            .context("mp4 track")?;
        Ok(Self {
            writer,
            path,
            samples: 0,
            wait_idr: true,
            pending: None,
            logged_wait: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn push(&mut self, pts_us: u64, keyframe: bool, nal: &[u8]) -> anyhow::Result<()> {
        let idr = keyframe || nals(nal).iter().any(|n| nal_type(n) == 5);
        if self.wait_idr && !idr {
            if !self.logged_wait {
                self.logged_wait = true;
                log::info!("mp4: waiting for a keyframe");
            }
            return Ok(());
        }
        let bytes = avcc_sample(nal);
        if bytes.is_empty() {
            return Ok(());
        }
        self.wait_idr = false;
        if let Some(prev) = self.pending.take() {
            let dur_ms = pts_us.saturating_sub(prev.pts_us) / 1000;
            self.write_sample(prev.pts_us, dur_ms, prev.keyframe, prev.bytes)?;
        }
        self.pending = Some(PendingSample {
            pts_us,
            keyframe: idr,
            bytes,
        });
        Ok(())
    }

    pub fn finish(mut self) -> anyhow::Result<PathBuf> {
        if let Some(prev) = self.pending.take() {
            self.write_sample(prev.pts_us, 33, prev.keyframe, prev.bytes)?;
        }
        let path = self.path.clone();
        self.writer.write_end().context("mp4 trailer")?;
        if self.samples == 0 {
            let _ = std::fs::remove_file(&path);
            anyhow::bail!("no frames were recorded");
        }
        Ok(path)
    }

    fn write_sample(&mut self, pts_us: u64, dur_ms: u64, keyframe: bool, bytes: Vec<u8>) -> anyhow::Result<()> {
        let sample = Mp4Sample {
            start_time: pts_us / 1000,
            duration: dur_ms.clamp(1, 10_000) as u32,
            rendering_offset: 0,
            is_sync: keyframe,
            bytes: Bytes::from(bytes),
        };
        self.writer.write_sample(1, &sample).context("mp4 sample")?;
        self.samples += 1;
        Ok(())
    }
}

fn sps_pps(csd: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut sps = None;
    let mut pps = None;
    for nal in nals(csd) {
        match nal_type(nal) {
            7 if sps.is_none() => sps = Some(nal.to_vec()),
            8 if pps.is_none() => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    Some((sps?, pps?))
}

/// Length-prefixed VCL NALs. Parameter sets stay in the avcC box.
fn avcc_sample(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals(annexb) {
        match nal_type(nal) {
            6 | 7 | 8 | 9 => continue,
            _ => {}
        }
        let len = nal.len() as u32;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn nal_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1f
}

fn nals(data: &[u8]) -> Vec<&[u8]> {
    let starts = start_codes(data);
    let mut out = Vec::new();
    for (i, start) in starts.iter().enumerate() {
        let to = starts
            .get(i + 1)
            .map(|next| next.at.saturating_sub(next.sc))
            .unwrap_or(data.len());
        if to > start.at {
            out.push(&data[start.at..to]);
        }
    }
    out
}

struct Start {
    at: usize,
    sc: usize,
}

fn start_codes(data: &[u8]) -> Vec<Start> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            starts.push(Start { at: i + 4, sc: 4 });
            i += 4;
        } else if data[i..].starts_with(&[0, 0, 1]) {
            starts.push(Start { at: i + 3, sc: 3 });
            i += 3;
        } else {
            i += 1;
        }
    }
    if starts.is_empty() {
        starts.push(Start { at: 0, sc: 0 });
    }
    starts
}

fn utc_stamp() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let secs = (ms / 1000) as u64;
    let millis = (ms % 1000) as u32;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}-{millis:03}",
        h = tod / 3600,
        m = (tod % 3600) / 60,
        s = tod % 60
    )
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_and_builds_avcc() {
        let buf = [
            0, 0, 0, 1, 0x67, 0xaa, 0, 0, 1, 0x68, 0xbb, 0, 0, 0, 1, 0x65, 0xcc,
        ];
        let parts = nals(&buf);
        assert_eq!(parts, vec![&[0x67, 0xaa][..], &[0x68, 0xbb][..], &[0x65, 0xcc][..]]);
        let sample = avcc_sample(&buf);
        assert_eq!(sample, vec![0, 0, 0, 2, 0x65, 0xcc]);
    }

    #[test]
    fn png_and_gif_roundtrip_a_pixel() {
        let dir = std::env::temp_dir();
        let png_path = dir.join(format!("droidmirror-test-{}.png", std::process::id()));
        save_png(&png_path, 1, 1, &[10, 20, 30, 255]).unwrap();
        let bytes = std::fs::read(&png_path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let _ = std::fs::remove_file(&png_path);

        let gif_path = dir.join(format!("droidmirror-test-{}.gif", std::process::id()));
        let (tx, rx) = mpsc::sync_channel(1);
        drop(tx);
        write_gif(
            &gif_path,
            GifFrame {
                width: 2,
                height: 2,
                rgba: vec![255, 0, 0, 255, 255, 0, 0, 255, 0, 0, 255, 255, 0, 0, 255, 255],
                delay_cs: 10,
            },
            &rx,
        )
        .unwrap();
        let mut dec = gif::DecodeOptions::new();
        dec.set_color_output(gif::ColorOutput::RGBA);
        let file = File::open(&gif_path).unwrap();
        let mut reader = dec.read_info(file).unwrap();
        let frame = reader.read_next_frame().unwrap().unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        let _ = std::fs::remove_file(&gif_path);
    }

    #[test]
    fn mp4_muxes_one_idr() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("droidmirror-test-{}.mp4", std::process::id()));
        let csd = [0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1e, 0, 0, 0, 1, 0x68, 0xce, 0x06, 0xe2];
        let mut rec = Mp4Rec::start(path.clone(), 16, 16, &csd).unwrap();
        let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
        rec.push(0, true, &idr).unwrap();
        rec.push(33_000, false, &[0, 0, 0, 1, 0x41, 0x9a, 0x00]).unwrap();
        let saved = rec.finish().unwrap();
        let file = File::open(&saved).unwrap();
        let size = file.metadata().unwrap().len();
        let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();
        assert_eq!(reader.tracks().len(), 1);
        let _ = std::fs::remove_file(&saved);
    }

    #[test]
    fn fit_shrinks_the_long_side() {
        let rgba = vec![0u8; 100 * 200 * 4];
        let (w, h, out) = fit_max_edge(100, 200, &rgba, 50);
        assert_eq!((w, h), (25, 50));
        assert_eq!(out.len(), 25 * 50 * 4);
    }
}
