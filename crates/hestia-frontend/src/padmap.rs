//! Linux evdev game controllers, as GameStream state: which evdev button
//! is which GameStream button, sticks and triggers scaled from each axis'
//! own range, the hat as a d-pad. Pure logic; `desktop_pads` reads the
//! devices.

use hestia_core::stream::input::buttons;

// linux/input-event-codes.h
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_C: u16 = 0x132;
pub const BTN_NORTH: u16 = 0x133;
pub const BTN_WEST: u16 = 0x134;
pub const BTN_Z: u16 = 0x135;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;
pub const BTN_DPAD_UP: u16 = 0x220;
pub const BTN_DPAD_DOWN: u16 = 0x221;
pub const BTN_DPAD_LEFT: u16 = 0x222;
pub const BTN_DPAD_RIGHT: u16 = 0x223;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_GAS: u16 = 0x09;
pub const ABS_BRAKE: u16 = 0x0a;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

/// Axes a controller may have, to query their ranges.
pub const AXES: [u16; 10] = [
    ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_GAS, ABS_BRAKE, ABS_HAT0X, ABS_HAT0Y,
];

/// GameStream's bit for an evdev button (the kernel's gamepad layout:
/// south = A, east = B, north = Y, west = X).
pub fn button_bit(code: u16) -> Option<u32> {
    Some(match code {
        BTN_SOUTH => buttons::A,
        BTN_EAST => buttons::B,
        BTN_NORTH => buttons::Y,
        BTN_WEST => buttons::X,
        BTN_TL => buttons::LB,
        BTN_TR => buttons::RB,
        BTN_SELECT => buttons::BACK,
        BTN_START => buttons::PLAY,
        BTN_MODE => buttons::SPECIAL,
        BTN_THUMBL => buttons::LS_CLICK,
        BTN_THUMBR => buttons::RS_CLICK,
        BTN_DPAD_UP => buttons::UP,
        BTN_DPAD_DOWN => buttons::DOWN,
        BTN_DPAD_LEFT => buttons::LEFT,
        BTN_DPAD_RIGHT => buttons::RIGHT,
        // Extra buttons some pads have (Nintendo's capture, 8BitDo...).
        BTN_C | BTN_Z => buttons::MISC,
        _ => return None,
    })
}

/// An axis' range, as `EVIOCGABS` reports it.
pub type Range = (i32, i32);

/// -32768…32767 from an axis value in `range` (0 when the range is empty).
pub fn stick(value: i32, (min, max): Range) -> i16 {
    if max <= min {
        return 0;
    }
    let center = (f64::from(min) + f64::from(max)) / 2.0;
    let half = (f64::from(max) - f64::from(min)) / 2.0;
    ((f64::from(value) - center) / half * 32767.0)
        .round()
        .clamp(-32768.0, 32767.0) as i16
}

/// 0…255 from a trigger value in `range`.
pub fn trigger(value: i32, (min, max): Range) -> u8 {
    if max <= min {
        return 0;
    }
    ((f64::from(value) - f64::from(min)) / (f64::from(max) - f64::from(min)) * 255.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Which axis is what on this controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The right stick's axes.
    pub right: (u16, u16),
    /// The analog triggers' axes, if any.
    pub triggers: Option<(u16, u16)>,
}

impl Layout {
    /// From the axes the controller has: the usual RX/RY right stick with
    /// Z/RZ (or brake/gas) triggers; DirectInput-style pads put the right
    /// stick on Z/RZ instead.
    pub fn detect(has: impl Fn(u16) -> bool) -> Self {
        let triggers = if has(ABS_BRAKE) && has(ABS_GAS) {
            Some((ABS_BRAKE, ABS_GAS))
        } else if has(ABS_RX) && has(ABS_RY) && has(ABS_Z) && has(ABS_RZ) {
            Some((ABS_Z, ABS_RZ))
        } else {
            None
        };
        let right = if has(ABS_RX) && has(ABS_RY) {
            (ABS_RX, ABS_RY)
        } else {
            (ABS_Z, ABS_RZ)
        };
        Self { right, triggers }
    }
}

/// One controller's state between `SYN_REPORT`s.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PadState {
    keys: u32,
    hat: (i32, i32),
    left: (i16, i16),
    right: (i16, i16),
    triggers: (u8, u8),
    digital_triggers: (bool, bool),
}

impl PadState {
    pub fn key(&mut self, code: u16, pressed: bool) {
        let bit = match code {
            BTN_TL2 => {
                self.digital_triggers.0 = pressed;
                return;
            }
            BTN_TR2 => {
                self.digital_triggers.1 = pressed;
                return;
            }
            _ => match button_bit(code) {
                Some(bit) => bit,
                None => return,
            },
        };
        if pressed {
            self.keys |= bit;
        } else {
            self.keys &= !bit;
        }
    }

    /// An axis moved; `range` is that axis' range. Y axes point down in
    /// evdev and up in GameStream.
    pub fn axis(&mut self, layout: &Layout, code: u16, value: i32, range: Range) {
        match code {
            ABS_X => self.left.0 = stick(value, range),
            ABS_Y => self.left.1 = stick(value, range).saturating_neg(),
            ABS_HAT0X => self.hat.0 = value.signum(),
            ABS_HAT0Y => self.hat.1 = value.signum(),
            c if c == layout.right.0 => self.right.0 = stick(value, range),
            c if c == layout.right.1 => self.right.1 = stick(value, range).saturating_neg(),
            c if layout.triggers.is_some_and(|(l, _)| l == c) => self.triggers.0 = trigger(value, range),
            c if layout.triggers.is_some_and(|(_, r)| r == c) => self.triggers.1 = trigger(value, range),
            _ => {}
        }
    }

    /// Buttons (with the hat as a d-pad), sticks and triggers (a digital
    /// trigger press counts as fully pulled).
    pub fn snapshot(&self) -> (u32, (i16, i16), (i16, i16), (u8, u8)) {
        let mut b = self.keys;
        for (on, bit) in [
            (self.hat.0 < 0, buttons::LEFT),
            (self.hat.0 > 0, buttons::RIGHT),
            (self.hat.1 < 0, buttons::UP),
            (self.hat.1 > 0, buttons::DOWN),
        ] {
            if on {
                b |= bit;
            }
        }
        let pulled = |analog: u8, digital: bool| if digital { 255 } else { analog };
        (
            b,
            self.left,
            self.right,
            (
                pulled(self.triggers.0, self.digital_triggers.0),
                pulled(self.triggers.1, self.digital_triggers.1),
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xbox_like_pad() {
        let layout =
            Layout::detect(|c| [ABS_X, ABS_Y, ABS_Z, ABS_RX, ABS_RY, ABS_RZ, ABS_HAT0X, ABS_HAT0Y].contains(&c));
        assert_eq!(layout.right, (ABS_RX, ABS_RY));
        assert_eq!(layout.triggers, Some((ABS_Z, ABS_RZ)));
        let stick_range = (-32768, 32767);
        let mut s = PadState::default();
        s.key(BTN_SOUTH, true);
        s.key(BTN_START, true);
        s.axis(&layout, ABS_Y, -32768, stick_range);
        s.axis(&layout, ABS_RX, 32767, stick_range);
        s.axis(&layout, ABS_RZ, 1023, (0, 1023));
        s.axis(&layout, ABS_HAT0Y, -1, (-1, 1));
        let (b, left, right, triggers) = s.snapshot();
        assert_eq!(b, buttons::A | buttons::PLAY | buttons::UP);
        assert_eq!(left.1, 32767, "stick pushed up is positive");
        assert_eq!(right.0, 32767);
        assert_eq!(triggers, (0, 255));
        s.key(BTN_SOUTH, false);
        s.axis(&layout, ABS_HAT0Y, 0, (-1, 1));
        assert_eq!(s.snapshot().0, buttons::PLAY);
    }

    #[test]
    fn directinput_pad_and_digital_triggers() {
        // Right stick on Z/RZ, triggers as buttons only, 0…255 axes.
        let layout = Layout::detect(|c| [ABS_X, ABS_Y, ABS_Z, ABS_RZ].contains(&c));
        assert_eq!(layout.right, (ABS_Z, ABS_RZ));
        assert_eq!(layout.triggers, None);
        let mut s = PadState::default();
        s.axis(&layout, ABS_Z, 255, (0, 255));
        s.axis(&layout, ABS_RZ, 128, (0, 255));
        s.key(BTN_TL2, true);
        let (_, _, right, triggers) = s.snapshot();
        assert_eq!(right.0, 32767);
        assert!(right.1.abs() < 200, "centred: {}", right.1);
        assert_eq!(triggers, (255, 0));
    }

    #[test]
    fn scaling_edges() {
        assert_eq!(stick(0, (0, 0)), 0);
        assert_eq!(stick(-32768, (-32768, 32767)), -32767);
        assert_eq!(trigger(512, (0, 1023)), 128);
        assert_eq!(button_bit(BTN_NORTH), Some(buttons::Y));
        assert_eq!(button_bit(0x100), None, "a mouse button isn't a pad button");
    }
}
