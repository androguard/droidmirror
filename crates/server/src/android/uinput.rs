use std::os::fd::RawFd;
use std::process::Command;

use crate::inject::{key_events, scroll_events, TouchSlots, ACTION_DOWN};
use crate::keys::{android_to_linux, ascii_to_linux, KEY_LEFTSHIFT};
use crate::control::Inject;

use super::log_line;

const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTALT: u16 = 56;
const META_SHIFT: u32 = 0x1;
const META_ALT: u32 = 0x2;
const META_CTRL: u32 = 0x1000;

pub struct UInput {
    fd: RawFd,
    slots: TouchSlots,
}

impl UInput {
    pub fn open(width: i32, height: i32) -> Result<Self, String> {
        let fd = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(format!("open /dev/uinput: {}", std::io::Error::last_os_error()));
        }
        let dev = Self { fd, slots: TouchSlots::default() };
        let span = width.max(height).max(4096);
        if let Err(e) = dev.setup(span, span) {
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(dev)
    }

    fn setup(&self, width: i32, height: i32) -> Result<(), String> {
        self.ioctl_int(ioc(1, 100, 4), EV_KEY)?;
        self.ioctl_int(ioc(1, 100, 4), EV_ABS)?;
        self.ioctl_int(ioc(1, 100, 4), EV_REL)?;
        // Touchscreen, not a touchpad. Without this Android ignores the clicks.
        let _ = self.ioctl_int(ioc(1, 110, 4), 1);
        self.ioctl_int(ioc(1, 101, 4), BTN_TOUCH as i32)?;
        self.ioctl_int(ioc(1, 101, 4), 0x145)?; // BTN_TOOL_FINGER
        for code in [
            ABS_MT_SLOT,
            ABS_MT_TRACKING_ID,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_PRESSURE,
            ABS_MT_TOUCH_MAJOR,
        ] {
            self.ioctl_int(ioc(1, 103, 4), code as i32)?;
        }
        self.ioctl_int(ioc(1, 102, 4), REL_WHEEL as i32)?;
        self.ioctl_int(ioc(1, 102, 4), REL_HWHEEL as i32)?;
        // A reasonable set of keys so text and nav work.
        for code in 1..256 {
            let _ = self.ioctl_int(ioc(1, 101, 4), code);
        }

        self.abs_setup(ABS_MT_SLOT, 0, 9)?;
        self.abs_setup(ABS_MT_TRACKING_ID, 0, 65535)?;
        self.abs_setup(ABS_MT_POSITION_X, 0, width - 1)?;
        self.abs_setup(ABS_MT_POSITION_Y, 0, height - 1)?;
        self.abs_setup(ABS_MT_PRESSURE, 0, 255)?;
        self.abs_setup(ABS_MT_TOUCH_MAJOR, 0, 255)?;

        let mut setup = UinputSetup {
            bustype: 0x06, // BUS_VIRTUAL
            vendor: 0,
            product: 0,
            version: 1,
            name: [0; 80],
            ff_effects_max: 0,
        };
        let name = b"droidmirror";
        setup.name[..name.len()].copy_from_slice(name);
        let rc = unsafe {
            libc::ioctl(
                self.fd,
                ioc(1, 3, std::mem::size_of::<UinputSetup>() as i32),
                &setup as *const UinputSetup,
            )
        };
        if rc < 0 {
            return Err(format!("UI_DEV_SETUP: {}", std::io::Error::last_os_error()));
        }
        let rc = unsafe { libc::ioctl(self.fd, ioc(0, 1, 0)) };
        if rc < 0 {
            return Err(format!("UI_DEV_CREATE: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn abs_setup(&self, code: u16, min: i32, max: i32) -> Result<(), String> {
        let info = UinputAbsSetup {
            code,
            _pad: 0,
            value: 0,
            minimum: min,
            maximum: max.max(min),
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };
        let rc = unsafe {
            libc::ioctl(
                self.fd,
                ioc(1, 4, std::mem::size_of::<UinputAbsSetup>() as i32),
                &info as *const UinputAbsSetup,
            )
        };
        if rc < 0 {
            return Err(format!("UI_ABS_SETUP {code}: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn ioctl_int(&self, req: libc::Ioctl, value: i32) -> Result<(), String> {
        let rc = unsafe { libc::ioctl(self.fd, req, value) };
        if rc < 0 {
            Err(format!("ioctl: {}", std::io::Error::last_os_error()))
        } else {
            Ok(())
        }
    }

    fn write_events(&self, events: &[crate::inject::Ev]) {
        for e in events {
            let mut buf = [0u8; 24];
            buf[16..18].copy_from_slice(&e.type_.to_ne_bytes());
            buf[18..20].copy_from_slice(&e.code.to_ne_bytes());
            buf[20..24].copy_from_slice(&e.value.to_ne_bytes());
            let _ = unsafe { libc::write(self.fd, buf.as_ptr() as *const _, buf.len()) };
        }
    }

    fn tap_key(&self, code: u16, down: bool) {
        self.write_events(&key_events(code, down));
    }
}

impl Drop for UInput {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.fd, ioc(0, 2, 0));
            libc::close(self.fd);
        }
    }
}

impl Inject for UInput {
    fn touch(&mut self, action: u8, id: u8, x: i32, y: i32, pressure: u16) {
        let evs = self.slots.apply(action, id, x, y, pressure);
        self.write_events(&evs);
    }

    fn key(&mut self, action: u8, keycode: u32, meta: u32) {
        let Some(code) = android_to_linux(keycode) else {
            return;
        };
        let down = action == ACTION_DOWN;
        if down {
            self.modifiers(meta, true);
        }
        self.tap_key(code, down);
        if !down {
            self.modifiers(meta, false);
        }
    }

    fn text(&mut self, s: &str) {
        for c in s.chars() {
            let Some((code, shift)) = ascii_to_linux(c) else {
                log_line(&format!("no uinput key for {c:?}"));
                continue;
            };
            if shift {
                self.tap_key(KEY_LEFTSHIFT, true);
            }
            self.tap_key(code, true);
            self.tap_key(code, false);
            if shift {
                self.tap_key(KEY_LEFTSHIFT, false);
            }
        }
    }

    fn scroll(&mut self, _x: i32, _y: i32, h: i32, v: i32) {
        self.write_events(&scroll_events(h, v));
    }
}

impl UInput {
    fn modifiers(&self, meta: u32, down: bool) {
        if meta & META_SHIFT != 0 {
            self.tap_key(KEY_LEFTSHIFT, down);
        }
        if meta & META_ALT != 0 {
            self.tap_key(KEY_LEFTALT, down);
        }
        if meta & META_CTRL != 0 {
            self.tap_key(KEY_LEFTCTRL, down);
        }
    }
}

/// Optional debug path when `/dev/uinput` cannot be created. Not the quality tier.
pub struct ShellInput;

impl Inject for ShellInput {
    fn touch(&mut self, action: u8, _id: u8, x: i32, y: i32, _pressure: u16) {
        let verb = match action {
            0 => "DOWN",
            1 | 3 => "UP",
            2 => "MOVE",
            _ => return,
        };
        let _ = Command::new("input")
            .args(["motionevent", verb, &x.to_string(), &y.to_string()])
            .status();
    }

    fn key(&mut self, action: u8, keycode: u32, _meta: u32) {
        if action != ACTION_DOWN {
            return;
        }
        let _ = Command::new("input")
            .args(["keyevent", &keycode.to_string()])
            .status();
    }

    fn text(&mut self, s: &str) {
        let _ = Command::new("input").args(["text", s]).status();
    }

    fn scroll(&mut self, x: i32, y: i32, _h: i32, v: i32) {
        let y2 = y - v.signum() * 80;
        let _ = Command::new("input")
            .args([
                "swipe",
                &x.to_string(),
                &y.to_string(),
                &x.to_string(),
                &y2.to_string(),
                "50",
            ])
            .status();
    }
}

fn ioc(dir: i32, nr: i32, size: i32) -> libc::Ioctl {
    ((dir << 30) | (size << 16) | ((b'U' as i32) << 8) | nr) as libc::Ioctl
}

const EV_KEY: i32 = 1;
const EV_REL: i32 = 2;
const EV_ABS: i32 = 3;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;
const BTN_TOUCH: u16 = 0x14a;
const REL_WHEEL: u16 = 0x08;
const REL_HWHEEL: u16 = 0x06;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}
