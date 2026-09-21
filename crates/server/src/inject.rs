//! Linux `input_event` sequences for the server's `/dev/uinput` device.
//! Pure so the multitouch / key translation is tested without a device.

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0x00;

pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_MT_PRESSURE: u16 = 0x3a;

pub const BTN_TOUCH: u16 = 0x14a;
pub const REL_WHEEL: u16 = 0x08;
pub const REL_HWHEEL: u16 = 0x06;

pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;
pub const ACTION_CANCEL: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ev {
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

fn ev(type_: u16, code: u16, value: i32) -> Ev {
    Ev { type_, code, value }
}

#[derive(Debug)]
pub struct TouchSlots {
    down: [bool; 10],
}

impl Default for TouchSlots {
    fn default() -> Self {
        Self { down: [false; 10] }
    }
}

impl TouchSlots {
    pub fn apply(&mut self, action: u8, id: u8, x: i32, y: i32, pressure: u16) -> Vec<Ev> {
        let slot = (id as usize).min(9);
        let mut out = vec![ev(EV_ABS, ABS_MT_SLOT, slot as i32)];
        match action {
            ACTION_UP | ACTION_CANCEL => {
                self.down[slot] = false;
                out.push(ev(EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
            ACTION_DOWN | ACTION_MOVE => {
                let tracking = if self.down[slot] { slot as i32 } else { slot as i32 };
                if action == ACTION_DOWN || !self.down[slot] {
                    out.push(ev(EV_ABS, ABS_MT_TRACKING_ID, tracking));
                }
                self.down[slot] = true;
                out.push(ev(EV_ABS, ABS_MT_POSITION_X, x));
                out.push(ev(EV_ABS, ABS_MT_POSITION_Y, y));
                let p = (pressure as i32).clamp(0, 255);
                out.push(ev(EV_ABS, ABS_MT_PRESSURE, p));
                out.push(ev(EV_ABS, ABS_MT_TOUCH_MAJOR, 6));
            }
            _ => return Vec::new(),
        }
        let any = self.down.iter().any(|d| *d);
        out.push(ev(EV_KEY, BTN_TOUCH, i32::from(any)));
        out.push(ev(EV_SYN, SYN_REPORT, 0));
        out
    }
}

pub fn scroll_events(h: i32, v: i32) -> Vec<Ev> {
    let mut out = Vec::new();
    if v != 0 {
        out.push(ev(EV_REL, REL_WHEEL, v.clamp(-8, 8)));
    }
    if h != 0 {
        out.push(ev(EV_REL, REL_HWHEEL, h.clamp(-8, 8)));
    }
    if !out.is_empty() {
        out.push(ev(EV_SYN, SYN_REPORT, 0));
    }
    out
}

pub fn key_events(code: u16, down: bool) -> Vec<Ev> {
    vec![
        ev(EV_KEY, code, i32::from(down)),
        ev(EV_SYN, SYN_REPORT, 0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_then_up_releases_button() {
        let mut t = TouchSlots::default();
        let down = t.apply(ACTION_DOWN, 0, 10, 20, 65535);
        assert!(down.iter().any(|e| e.code == ABS_MT_POSITION_X && e.value == 10));
        assert!(down.iter().any(|e| e.code == BTN_TOUCH && e.value == 1));
        let up = t.apply(ACTION_UP, 0, 10, 20, 0);
        assert!(up.iter().any(|e| e.code == ABS_MT_TRACKING_ID && e.value == -1));
        assert!(up.iter().any(|e| e.code == BTN_TOUCH && e.value == 0));
    }

    #[test]
    fn second_finger_keeps_button() {
        let mut t = TouchSlots::default();
        t.apply(ACTION_DOWN, 0, 1, 1, 1);
        let up0 = t.apply(ACTION_UP, 0, 1, 1, 0);
        t.apply(ACTION_DOWN, 1, 2, 2, 1);
        // re-down finger 1 after 0 was up: button is down
        let _ = up0;
        let still = t.apply(ACTION_MOVE, 1, 3, 4, 1);
        assert!(still.iter().any(|e| e.code == BTN_TOUCH && e.value == 1));
        assert!(still.iter().any(|e| e.code == ABS_MT_POSITION_Y && e.value == 4));
    }
}
