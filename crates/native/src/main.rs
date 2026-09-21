mod capture;
mod decode;
mod render;
mod toolbar;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use droidmirror_client::{
    Client, KeyAction, Out, TouchAction, KEY_APP_SWITCH, KEY_BACK, KEY_HOME, KEY_POWER,
    KEY_VOLUME_DOWN, KEY_VOLUME_UP, META_ALT, META_CTRL, META_SHIFT,
};
use droidmirror_proto::Codec;
use droidmirror_transport::{adb_serials, NativeAdb, StreamId};
use tokio::sync::mpsc as async_mpsc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

const REMOTE_DIR: &str = "/data/local/tmp/droidmirror";
/// Two rows: capture (PNG, GIF, MP4) and navigation.
const TOOLBAR_DP: f64 = 80.0;

#[derive(Parser, Debug)]
#[command(name = "droidmirror", about = "Android screen mirror (full Rust)")]
struct Args {
    /// USB serial from `droidmirror` device list. Required when more than one phone is plugged in.
    #[arg(long)]
    serial: Option<String>,
    /// Connect to `adb tcpip` instead of USB, e.g. 192.168.1.20:5555
    #[arg(long)]
    tcp: Option<String>,
    #[arg(long, default_value_t = 8_000_000)]
    bitrate: u32,
    #[arg(long, default_value_t = 60)]
    max_fps: u32,
    /// Scale the longer side down to this many pixels. 0 keeps the device size.
    #[arg(long, default_value_t = 0)]
    max_size: u16,
    #[arg(long, default_value = "h264")]
    codec: String,
    #[arg(long)]
    no_control: bool,
    /// Write the Annex-B elementary stream (ffplay -f h264).
    #[arg(long)]
    record: Option<PathBuf>,
    /// Directory with libdroidmirror_server.so and droidmirror.dex.
    #[arg(long)]
    server_dir: Option<PathBuf>,
    /// Print NALs and exit (no window). Useful while bringing up the device server.
    #[arg(long)]
    dump_nals: Option<PathBuf>,
}

enum HostCmd {
    Touch {
        action: TouchAction,
        x: f32,
        y: f32,
        view_w: f32,
        view_h: f32,
    },
    Key {
        action: KeyAction,
        keycode: u32,
        meta: u32,
    },
    Nav {
        keycode: u32,
        action: KeyAction,
    },
    Text(String),
    Scroll {
        x: f32,
        y: f32,
        view_w: f32,
        view_h: f32,
        h: f32,
        v: f32,
    },
    Pause(bool),
    /// Start or stop muxing the mirror's H.264 into an MP4 on the Desktop.
    RecordMp4(bool),
    Quit,
}

enum DecodedMsg {
    Frame(decode::RgbaFrame),
    Size(u16, u16),
    Status(String),
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    if args.dump_nals.is_some() {
        return rt.block_on(run_dump(args));
    }
    let args2 = ArgsSnapshot::from(&args);
    let live = match rt.block_on(prepare(&args2)) {
        Ok(live) => live,
        Err(e) => {
            log::error!("{e:#}");
            drop(rt);
            std::process::exit(1);
        }
    };
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DecodedMsg>(8);
    let (cmd_tx, cmd_rx) = async_mpsc::unbounded_channel::<HostCmd>();
    let session_task = rt.spawn(async move {
        if let Err(e) = session(live, args2, frame_tx, cmd_rx).await {
            log::error!("{e:#}");
        }
    });
    let event_loop = EventLoop::new()?;
    let mut app = App {
        window: None,
        gpu: None,
        frame_rx,
        cmd_tx,
        cursor: (0.0, 0.0),
        buttons: 0,
        mods: ModifiersState::empty(),
        video: (0, 0),
        no_control: args.no_control,
        toolbar_px: TOOLBAR_DP as f32,
        latest: None,
        gif: None,
        gif_last: None,
        mp4_on: false,
        status: String::new(),
    };
    event_loop.run_app(&mut app)?;
    rt.block_on(async {
        if tokio::time::timeout(Duration::from_secs(3), session_task).await.is_err() {
            log::warn!("mirror session did not exit");
        }
    });
    Ok(())
}

struct ArgsSnapshot {
    serial: Option<String>,
    tcp: Option<String>,
    bitrate: u32,
    max_fps: u32,
    max_size: u16,
    codec: String,
    record: Option<PathBuf>,
    server_dir: Option<PathBuf>,
    dump_nals: Option<PathBuf>,
}

impl From<&Args> for ArgsSnapshot {
    fn from(a: &Args) -> Self {
        Self {
            serial: a.serial.clone(),
            tcp: a.tcp.clone(),
            bitrate: a.bitrate,
            max_fps: a.max_fps,
            max_size: a.max_size,
            codec: a.codec.clone(),
            record: a.record.clone(),
            server_dir: a.server_dir.clone(),
            dump_nals: a.dump_nals.clone(),
        }
    }
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<render::Gpu>,
    frame_rx: Receiver<DecodedMsg>,
    cmd_tx: async_mpsc::UnboundedSender<HostCmd>,
    cursor: (f32, f32),
    buttons: u8,
    mods: ModifiersState,
    video: (u16, u16),
    no_control: bool,
    toolbar_px: f32,
    latest: Option<decode::RgbaFrame>,
    gif: Option<GifRec>,
    gif_last: Option<Instant>,
    mp4_on: bool,
    status: String,
}

struct GifRec {
    tx: SyncSender<capture::GifFrame>,
    join: Option<JoinHandle<()>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("droidmirror")
            .with_inner_size(LogicalSize::new(420.0, 900.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let scale = window.scale_factor() as f32;
        self.toolbar_px = (TOOLBAR_DP as f32) * scale;
        match render::Gpu::new(window.clone(), self.toolbar_px) {
            Ok(gpu) => self.gpu = Some(gpu),
            Err(e) => log::error!("gpu: {e:#}"),
        }
        self.window = Some(window);
        self.paint_bar();
        self.refresh_title();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.stop_gif();
                let _ = self.cmd_tx.send(HostCmd::RecordMp4(false));
                let _ = self.cmd_tx.send(HostCmd::Quit);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
                self.paint_bar();
            }
            WindowEvent::RedrawRequested => {
                while let Ok(msg) = self.frame_rx.try_recv() {
                    match msg {
                        DecodedMsg::Size(w, h) => self.video = (w, h),
                        DecodedMsg::Frame(frame) => {
                            self.video = (frame.width as u16, frame.height as u16);
                            self.push_gif(&frame);
                            if let Some(gpu) = &mut self.gpu {
                                gpu.upload(frame.width, frame.height, &frame.rgba);
                            }
                            self.latest = Some(frame);
                        }
                        DecodedMsg::Status(text) => {
                            if text.starts_with("mp4:") {
                                self.mp4_on = false;
                                self.paint_bar();
                            }
                            self.status = text;
                            self.refresh_title();
                        }
                    }
                }
                if let Some(gpu) = &mut self.gpu {
                    if let Err(e) = gpu.render() {
                        log::warn!("render: {e}");
                    }
                }
            }
            WindowEvent::Occluded(true) => {
                let _ = self.cmd_tx.send(HostCmd::Pause(true));
            }
            WindowEvent::Occluded(false) => {
                let _ = self.cmd_tx.send(HostCmd::Pause(false));
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                if self.buttons != 0 && !self.no_control {
                    self.send_touch(TouchAction::Move);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button != MouseButton::Left {
                    if state == ElementState::Pressed && button == MouseButton::Right && !self.no_control
                    {
                        let _ = self.cmd_tx.send(HostCmd::Nav {
                            keycode: KEY_BACK,
                            action: KeyAction::Down,
                        });
                        let _ = self.cmd_tx.send(HostCmd::Nav {
                            keycode: KEY_BACK,
                            action: KeyAction::Up,
                        });
                    }
                    return;
                }
                let down = state == ElementState::Pressed;
                if down && self.in_toolbar() {
                    self.toolbar_click();
                    return;
                }
                self.buttons = u8::from(down);
                if !self.no_control {
                    self.send_touch(if down { TouchAction::Down } else { TouchAction::Up });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.no_control {
                    return;
                }
                let (h, v) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32 / 40.0, p.y as f32 / 40.0),
                };
                let (vw, vh) = self.view();
                let _ = self.cmd_tx.send(HostCmd::Scroll {
                    x: self.cursor.0,
                    y: self.cursor.1,
                    view_w: vw,
                    view_h: vh,
                    h,
                    v,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed && !event.repeat && self.capture_shortcut(&event) {
                    return;
                }
                if event.state != ElementState::Pressed || self.no_control {
                    if event.state == ElementState::Released {
                        if let Some(code) = android_key(&event.physical_key) {
                            let _ = self.cmd_tx.send(HostCmd::Key {
                                action: KeyAction::Up,
                                keycode: code,
                                meta: meta_bits(self.mods),
                            });
                        }
                    }
                    return;
                }
                self.on_key(&event);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

impl App {
    fn view(&self) -> (f32, f32) {
        let Some(w) = &self.window else {
            return (1.0, 1.0);
        };
        let s = w.inner_size();
        (s.width as f32, (s.height as f32 - self.toolbar_px).max(1.0))
    }

    fn in_toolbar(&self) -> bool {
        let Some(w) = &self.window else {
            return false;
        };
        self.cursor.1 >= w.inner_size().height as f32 - self.toolbar_px
    }

    fn toolbar_click(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let width = window.inner_size().width as f32;
        let top = window.inner_size().height as f32 - self.toolbar_px;
        if self.cursor.1 < top + self.toolbar_px / 2.0 {
            let slot = ((self.cursor.0 / width) * 3.0).clamp(0.0, 2.99) as u32;
            match slot {
                0 => self.screenshot(),
                1 => self.toggle_gif(),
                _ => self.toggle_mp4(),
            }
            return;
        }
        let slot = ((self.cursor.0 / width) * 6.0).clamp(0.0, 5.0) as u32;
        let key = match slot {
            0 => KEY_BACK,
            1 => KEY_HOME,
            2 => KEY_APP_SWITCH,
            3 => KEY_POWER,
            4 => KEY_VOLUME_DOWN,
            _ => KEY_VOLUME_UP,
        };
        let _ = self.cmd_tx.send(HostCmd::Nav {
            keycode: key,
            action: KeyAction::Down,
        });
        let _ = self.cmd_tx.send(HostCmd::Nav {
            keycode: key,
            action: KeyAction::Up,
        });
    }

    fn capture_shortcut(&mut self, event: &winit::event::KeyEvent) -> bool {
        if !(self.mods.super_key() || self.mods.control_key()) || !self.mods.shift_key() {
            return false;
        }
        let Key::Character(text) = &event.logical_key else {
            return false;
        };
        match text.chars().next().map(|c| c.to_ascii_lowercase()) {
            Some('s') => self.screenshot(),
            Some('g') => self.toggle_gif(),
            Some('m') => self.toggle_mp4(),
            _ => return false,
        }
        true
    }

    fn screenshot(&mut self) {
        let Some(frame) = &self.latest else {
            log::warn!("screenshot: no frame yet");
            return;
        };
        let path = capture::output_path("png");
        let width = frame.width;
        let height = frame.height;
        let rgba = frame.rgba.clone();
        self.status = path.display().to_string();
        self.refresh_title();
        std::thread::spawn(move || match capture::save_png(&path, width, height, &rgba) {
            Ok(()) => log::info!("saved {}", path.display()),
            Err(e) => log::error!("screenshot: {e:#}"),
        });
    }

    fn toggle_gif(&mut self) {
        if self.gif.is_some() {
            self.stop_gif();
            return;
        }
        if self.latest.is_none() {
            log::warn!("gif: no frame yet");
            return;
        }
        let path = capture::output_path("gif");
        let (tx, join) = capture::spawn_gif(path.clone());
        self.gif = Some(GifRec {
            tx,
            join: Some(join),
        });
        self.gif_last = None;
        self.status = path.display().to_string();
        self.paint_bar();
        self.refresh_title();
        log::info!("recording {}", path.display());
    }

    fn stop_gif(&mut self) {
        let Some(gif) = self.gif.take() else {
            return;
        };
        drop(gif.tx);
        if let Some(join) = gif.join {
            let _ = join.join();
        }
        self.gif_last = None;
        self.paint_bar();
        self.refresh_title();
    }

    fn push_gif(&mut self, frame: &decode::RgbaFrame) {
        let Some(gif) = &self.gif else {
            return;
        };
        let now = Instant::now();
        let delay_cs = self
            .gif_last
            .map(|t| (now.duration_since(t).as_millis() / 10) as u16)
            .unwrap_or(10);
        if self.gif_last.is_some() && delay_cs < 9 {
            return;
        }
        let (width, height, rgba) = capture::fit_max_edge(frame.width, frame.height, &frame.rgba, 480);
        if gif
            .tx
            .try_send(capture::GifFrame {
                width,
                height,
                rgba,
                delay_cs: delay_cs.max(2),
            })
            .is_ok()
        {
            self.gif_last = Some(now);
        }
    }

    fn toggle_mp4(&mut self) {
        self.mp4_on = !self.mp4_on;
        let _ = self.cmd_tx.send(HostCmd::RecordMp4(self.mp4_on));
        if self.mp4_on {
            self.status.clear();
        }
        self.paint_bar();
        self.refresh_title();
    }

    fn paint_bar(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let height = self.toolbar_px.round().max(2.0) as u32;
        let px = toolbar::paint(
            size.width.max(1),
            height,
            toolbar::ToolbarState {
                gif: self.gif.is_some(),
                mp4: self.mp4_on,
            },
        );
        if let Some(gpu) = &mut self.gpu {
            gpu.upload_toolbar(size.width.max(1), height, &px);
        }
    }

    fn refresh_title(&self) {
        let Some(window) = &self.window else {
            return;
        };
        let rec = match (self.gif.is_some(), self.mp4_on) {
            (true, true) => "recording GIF+MP4 · ",
            (true, false) => "recording GIF · ",
            (false, true) => "recording MP4 · ",
            (false, false) => "",
        };
        let saved = if rec.is_empty() && !self.status.is_empty() {
            format!("{} · ", self.status)
        } else {
            String::new()
        };
        window.set_title(&format!(
            "droidmirror  ·  {rec}{saved}PNG  GIF  MP4  ·  Back Home Apps Power Vol- Vol+"
        ));
    }

    fn send_touch(&self, action: TouchAction) {
        let (vw, vh) = self.view();
        let _ = self.cmd_tx.send(HostCmd::Touch {
            action,
            x: self.cursor.0,
            y: self.cursor.1,
            view_w: vw,
            view_h: vh,
        });
    }

    fn on_key(&self, event: &winit::event::KeyEvent) {
        let ctrl = self.mods.control_key();
        if ctrl && event.logical_key == Key::Character("v".into()) {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    let _ = self.cmd_tx.send(HostCmd::Text(text));
                    return;
                }
            }
        }
        if event.physical_key == PhysicalKey::Code(KeyCode::Escape) {
            let _ = self.cmd_tx.send(HostCmd::Nav {
                keycode: KEY_BACK,
                action: KeyAction::Down,
            });
            let _ = self.cmd_tx.send(HostCmd::Nav {
                keycode: KEY_BACK,
                action: KeyAction::Up,
            });
            return;
        }
        if ctrl && event.logical_key == Key::Character("h".into()) {
            let _ = self.cmd_tx.send(HostCmd::Nav {
                keycode: KEY_HOME,
                action: KeyAction::Down,
            });
            return;
        }
        if ctrl && event.logical_key == Key::Character("s".into()) {
            let _ = self.cmd_tx.send(HostCmd::Nav {
                keycode: KEY_APP_SWITCH,
                action: KeyAction::Down,
            });
            return;
        }
        if let Key::Named(NamedKey::AudioVolumeUp) = event.logical_key {
            let _ = self.cmd_tx.send(HostCmd::Nav {
                keycode: KEY_VOLUME_UP,
                action: KeyAction::Down,
            });
            return;
        }
        if !ctrl && !self.mods.alt_key() {
            if let Some(text) = event.text.as_deref() {
                if text.chars().all(|c| !c.is_control()) {
                    let _ = self.cmd_tx.send(HostCmd::Text(text.to_string()));
                    return;
                }
            }
        }
        if let Some(code) = android_key(&event.physical_key) {
            let _ = self.cmd_tx.send(HostCmd::Key {
                action: KeyAction::Down,
                keycode: code,
                meta: meta_bits(self.mods),
            });
        }
    }
}

fn meta_bits(m: ModifiersState) -> u32 {
    let mut b = 0;
    if m.shift_key() {
        b |= META_SHIFT;
    }
    if m.alt_key() {
        b |= META_ALT;
    }
    if m.control_key() {
        b |= META_CTRL;
    }
    b
}

fn android_key(key: &PhysicalKey) -> Option<u32> {
    let PhysicalKey::Code(code) = key else {
        return None;
    };
    Some(match code {
        KeyCode::KeyA => 29,
        KeyCode::KeyB => 30,
        KeyCode::KeyC => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyE => 33,
        KeyCode::KeyF => 34,
        KeyCode::KeyG => 35,
        KeyCode::KeyH => 36,
        KeyCode::KeyI => 37,
        KeyCode::KeyJ => 38,
        KeyCode::KeyK => 39,
        KeyCode::KeyL => 40,
        KeyCode::KeyM => 41,
        KeyCode::KeyN => 42,
        KeyCode::KeyO => 43,
        KeyCode::KeyP => 44,
        KeyCode::KeyQ => 45,
        KeyCode::KeyR => 46,
        KeyCode::KeyS => 47,
        KeyCode::KeyT => 48,
        KeyCode::KeyU => 49,
        KeyCode::KeyV => 50,
        KeyCode::KeyW => 51,
        KeyCode::KeyX => 52,
        KeyCode::KeyY => 53,
        KeyCode::KeyZ => 54,
        KeyCode::Digit0 => 7,
        KeyCode::Digit1 => 8,
        KeyCode::Digit2 => 9,
        KeyCode::Digit3 => 10,
        KeyCode::Digit4 => 11,
        KeyCode::Digit5 => 12,
        KeyCode::Digit6 => 13,
        KeyCode::Digit7 => 14,
        KeyCode::Digit8 => 15,
        KeyCode::Digit9 => 16,
        KeyCode::Enter => 66,
        KeyCode::Backspace => 67,
        KeyCode::Tab => 61,
        KeyCode::Space => 62,
        KeyCode::ArrowUp => 19,
        KeyCode::ArrowDown => 20,
        KeyCode::ArrowLeft => 21,
        KeyCode::ArrowRight => 22,
        KeyCode::Escape => 111,
        _ => return None,
    })
}

async fn run_dump(args: Args) -> anyhow::Result<()> {
    let snap = ArgsSnapshot::from(&args);
    let live = prepare(&snap).await?;
    session(live, snap, mpsc::sync_channel(8).0, async_mpsc::unbounded_channel().1).await
}

struct Live {
    adb: Arc<NativeAdb>,
    video: StreamId,
}

/// Connect, push the server, and open the mirror socket. Failure here exits
/// before the window is created.
async fn prepare(args: &ArgsSnapshot) -> anyhow::Result<Live> {
    let adb = Arc::new(connect(args).await?);
    log::info!("adb {}", adb.banner());
    deploy(&adb, args).await?;
    let launch = launch_command(args);
    log::info!("launch {launch}");
    let shell = adb.open(&format!("shell:{launch}")).await?;
    let log_adb = Arc::clone(&adb);
    tokio::spawn(async move {
        loop {
            match log_adb.read_stream(shell).await {
                Ok(b) if b.is_empty() => break,
                Ok(b) => eprint!("{}", String::from_utf8_lossy(&b)),
                Err(_) => break,
            }
        }
    });
    let video = open_retry(&adb, "localabstract:droidmirror", 40).await?;
    Ok(Live { adb, video })
}

async fn session(
    live: Live,
    args: ArgsSnapshot,
    frame_tx: SyncSender<DecodedMsg>,
    mut cmd_rx: async_mpsc::UnboundedReceiver<HostCmd>,
) -> anyhow::Result<()> {
    let Live { adb, video } = live;
    let mut client = Client::new();
    let mut record = if let Some(path) = args.record.as_ref().or(args.dump_nals.as_ref()) {
        Some(std::fs::File::create(path).with_context(|| format!("record {}", path.display()))?)
    } else {
        None
    };
    let (nal_tx, nal_rx) = mpsc::sync_channel::<NalJob>(8);
    if args.dump_nals.is_none() {
        let frame_tx2 = frame_tx.clone();
        std::thread::spawn(move || decode_thread(nal_rx, frame_tx2));
    }
    let mut video_cfg: Option<VideoCfg> = None;
    let mut mp4: Option<capture::Mp4Rec> = None;
    let mut mp4_on = false;
    let mut stream_err = None;
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    HostCmd::Quit => break,
                    HostCmd::RecordMp4(on) => {
                        mp4_on = on;
                        if on {
                            if mp4.is_none() {
                                if let Some(cfg) = &video_cfg {
                                    mp4 = open_mp4(cfg, &frame_tx);
                                    if mp4.is_none() {
                                        mp4_on = false;
                                    }
                                } else {
                                    log::info!("mp4: waiting for video");
                                }
                            }
                        } else {
                            finish_mp4(&mut mp4, &frame_tx);
                        }
                    }
                    other => {
                        let bytes = encode_cmd(&client, other);
                        if !bytes.is_empty() {
                            adb.write_stream(video, &bytes).await?;
                        }
                    }
                }
            }
            chunk = adb.read_stream(video) => {
                let chunk = chunk?;
                if chunk.is_empty() {
                    stream_err = Some(anyhow::anyhow!("mirror stream closed"));
                    break;
                }
                for ev in client.on_bytes(&chunk) {
                    match ev {
                        Out::DeviceName(n) => log::info!("device {n}"),
                        Out::Configure { codec, csd, width, height } => {
                            log::info!("configure {codec:?} {width}x{height} csd {}", csd.len());
                            let changed = video_cfg.as_ref().is_some_and(|c| {
                                c.width != width || c.height != height || c.codec != codec
                            });
                            if changed {
                                finish_mp4(&mut mp4, &frame_tx);
                            }
                            video_cfg = Some(VideoCfg {
                                codec,
                                csd: csd.clone(),
                                width,
                                height,
                            });
                            if mp4_on && mp4.is_none() {
                                mp4 = open_mp4(video_cfg.as_ref().unwrap(), &frame_tx);
                                if mp4.is_none() {
                                    mp4_on = false;
                                }
                            }
                            let _ = frame_tx.try_send(DecodedMsg::Size(width, height));
                            enqueue(
                                &nal_tx,
                                NalJob::Configure { codec, csd },
                                true,
                            );
                        }
                        Out::Frame { nal, keyframe, pts_us } => {
                            if let Some(f) = &mut record {
                                use std::io::Write;
                                if !nal.starts_with(&[0, 0, 0, 1]) && !nal.starts_with(&[0, 0, 1]) {
                                    let _ = f.write_all(&[0, 0, 0, 1]);
                                }
                                let _ = f.write_all(&nal);
                            }
                            if let Some(rec) = &mut mp4 {
                                if let Err(e) = rec.push(pts_us, keyframe, &nal) {
                                    log::error!("mp4: {e:#}");
                                }
                            }
                            if args.dump_nals.is_some() {
                                continue;
                            }
                            if !enqueue(&nal_tx, NalJob::Frame { nal, keyframe }, keyframe) {
                                client.request_keyframe_skip();
                            }
                        }
                        Out::Error(e) => log::error!("protocol: {e}"),
                    }
                }
            }
        }
    }
    finish_mp4(&mut mp4, &frame_tx);
    let _ = adb.close_stream(video).await;
    if let Some(e) = stream_err {
        return Err(e);
    }
    Ok(())
}

struct VideoCfg {
    codec: Codec,
    csd: Vec<u8>,
    width: u16,
    height: u16,
}

fn open_mp4(cfg: &VideoCfg, frame_tx: &SyncSender<DecodedMsg>) -> Option<capture::Mp4Rec> {
    if cfg.codec != Codec::H264 {
        let msg = "mp4: capture needs H.264".to_string();
        log::error!("{msg}");
        let _ = frame_tx.try_send(DecodedMsg::Status(msg));
        return None;
    }
    let path = capture::output_path("mp4");
    match capture::Mp4Rec::start(path, cfg.width, cfg.height, &cfg.csd) {
        Ok(rec) => {
            log::info!("recording {}", rec.path().display());
            Some(rec)
        }
        Err(e) => {
            let msg = format!("mp4: {e:#}");
            log::error!("{msg}");
            let _ = frame_tx.try_send(DecodedMsg::Status(msg));
            None
        }
    }
}

fn finish_mp4(mp4: &mut Option<capture::Mp4Rec>, frame_tx: &SyncSender<DecodedMsg>) {
    let Some(rec) = mp4.take() else {
        return;
    };
    match rec.finish() {
        Ok(path) => {
            log::info!("saved {}", path.display());
            let _ = frame_tx.try_send(DecodedMsg::Status(path.display().to_string()));
        }
        Err(e) => {
            let msg = format!("mp4: {e:#}");
            log::error!("{msg}");
            let _ = frame_tx.try_send(DecodedMsg::Status(msg));
        }
    }
}

enum NalJob {
    Configure { codec: Codec, csd: Vec<u8> },
    Frame { nal: Vec<u8>, keyframe: bool },
}

/// Queue a NAL for the decoder. Codec config and IDRs wait briefly instead of
/// being dropped: OpenH264 cannot resync without them.
fn enqueue(tx: &SyncSender<NalJob>, job: NalJob, important: bool) -> bool {
    match tx.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(job)) if important => tx.send(job).is_ok(),
        Err(_) => false,
    }
}

fn decode_thread(rx: Receiver<NalJob>, tx: SyncSender<DecodedMsg>) {
    let mut dec = decode::make_decoder();
    while let Ok(job) = rx.recv() {
        match job {
            NalJob::Configure { codec, csd } => {
                if let Err(e) = dec.configure(codec, &csd) {
                    log::error!("decoder configure: {e:#}");
                }
            }
            NalJob::Frame { nal, keyframe } => match dec.decode(&nal, keyframe) {
                Ok(Some(frame)) => {
                    if tx.send(DecodedMsg::Frame(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => log::warn!("decode: {e}"),
            },
        }
    }
}

fn encode_cmd(client: &Client, cmd: HostCmd) -> Vec<u8> {
    match cmd {
        HostCmd::Touch {
            action,
            x,
            y,
            view_w,
            view_h,
        } => client.touch(action, 0, x, y, view_w, view_h, 1.0),
        HostCmd::Key {
            action,
            keycode,
            meta,
        } => client.key(action, keycode, meta),
        HostCmd::Nav { keycode, action } => client.nav(keycode, action),
        HostCmd::Text(s) => client.text(&s),
        HostCmd::Scroll {
            x,
            y,
            view_w,
            view_h,
            h,
            v,
        } => client.scroll(x, y, view_w, view_h, h, v),
        HostCmd::Pause(on) => client.pause(on),
        HostCmd::RecordMp4(_) | HostCmd::Quit => Vec::new(),
    }
}

async fn connect(args: &ArgsSnapshot) -> anyhow::Result<NativeAdb> {
    if let Some(addr) = &args.tcp {
        return Ok(NativeAdb::connect_tcp(addr).await?);
    }
    match NativeAdb::connect_usb(args.serial.as_deref()).await {
        Ok(adb) => Ok(adb),
        Err(e) => {
            if let Ok(list) = adb_serials() {
                log::error!("USB devices: {}", list.join(", "));
            }
            Err(e.into())
        }
    }
}

async fn deploy(adb: &NativeAdb, args: &ArgsSnapshot) -> anyhow::Result<()> {
    let dir = args.server_dir.clone().unwrap_or_else(default_server_dir);
    let so = std::fs::read(dir.join("libdroidmirror_server.so"))
        .with_context(|| format!("missing libdroidmirror_server.so in {}", dir.display()))?;
    let dex = std::fs::read(dir.join("droidmirror.dex"))
        .with_context(|| format!("missing droidmirror.dex in {}", dir.display()))?;
    adb.shell_cmd(&format!("mkdir -p {REMOTE_DIR}")).await?;
    adb.push(&so, &format!("{REMOTE_DIR}/libdroidmirror_server.so")).await?;
    adb.push(&dex, &format!("{REMOTE_DIR}/droidmirror.dex")).await?;
    adb.shell_cmd(&format!("chmod 755 {REMOTE_DIR}/libdroidmirror_server.so"))
        .await?;
    Ok(())
}

fn launch_command(args: &ArgsSnapshot) -> String {
    let codec = if args.codec.eq_ignore_ascii_case("h265") || args.codec.eq_ignore_ascii_case("hevc") {
        "h265"
    } else {
        "h264"
    };
    format!(
        "CLASSPATH={REMOTE_DIR}/droidmirror.dex exec app_process / com.droidmirror.Server --bitrate {} --max-fps {} --max-size {} --codec {codec} --lib={REMOTE_DIR}/libdroidmirror_server.so",
        args.bitrate, args.max_fps, args.max_size
    )
}

async fn open_retry(adb: &NativeAdb, dest: &str, attempts: u32) -> anyhow::Result<StreamId> {
    let mut last = None;
    for i in 0..attempts {
        match adb.open(dest).await {
            Ok(id) => return Ok(id),
            Err(e) => {
                last = Some(e);
                if i == 0 || i % 5 == 0 {
                    log::info!("waiting for {dest} ({}/{})", i + 1, attempts);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    Err(last
        .map(|e| anyhow::anyhow!(e))
        .unwrap_or_else(|| anyhow::anyhow!("stream did not open")))
}

fn default_server_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let next = dir.join("dist/android-arm64");
            if next.join("droidmirror.dex").exists() {
                return next;
            }
            let up = dir.join("../dist/android-arm64");
            if up.join("droidmirror.dex").exists() {
                return up;
            }
        }
    }
    PathBuf::from("dist/android-arm64")
}
