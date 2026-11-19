//! Game controllers: turns Android key codes and joystick axes into the
//! GameStream controller state (`GamepadState`), one slot per physical
//! controller (up to four), the way Moonlight/Artemis map them.
//!
//! Pure logic; the platform glue feeds it events and sends what it returns.

#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use hestia_core::stream::input::{GamepadState, buttons};

/// Most controllers GameStream addresses.
pub const MAX_CONTROLLERS: usize = 4;

/// Stick travel ignored around the centre (a worn stick rests off-centre).
const DEADZONE: f32 = 0.07;

/// Android source bits.
pub const SOURCE_GAMEPAD: u32 = 0x0000_0401;
pub const SOURCE_JOYSTICK: u32 = 0x0100_0010;
pub const SOURCE_DPAD: u32 = 0x0000_0201;

/// Whether an event's source is a game controller (gamepad buttons, a
/// joystick, or a controller's d-pad).
pub fn is_controller_source(source: u32) -> bool {
    source & SOURCE_GAMEPAD == SOURCE_GAMEPAD || source & SOURCE_JOYSTICK == SOURCE_JOYSTICK
}

/// What an Android key code means on a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Button(u32),
    LeftTrigger,
    RightTrigger,
}

pub fn control_for_keycode(keycode: u32) -> Option<Control> {
    use Control::*;
    Some(match keycode {
        19 => Button(buttons::UP),
        20 => Button(buttons::DOWN),
        21 => Button(buttons::LEFT),
        22 => Button(buttons::RIGHT),
        96 => Button(buttons::A),
        97 => Button(buttons::B),
        99 => Button(buttons::X),
        100 => Button(buttons::Y),
        102 => Button(buttons::LB),
        103 => Button(buttons::RB),
        104 => LeftTrigger,
        105 => RightTrigger,
        106 => Button(buttons::LS_CLICK),
        107 => Button(buttons::RS_CLICK),
        108 => Button(buttons::PLAY),
        109 => Button(buttons::BACK),
        110 => Button(buttons::SPECIAL),
        _ => return None,
    })
}

/// One joystick motion sample (Android axis values).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Axes {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub rz: f32,
    pub rx: f32,
    pub ry: f32,
    pub ltrigger: f32,
    pub rtrigger: f32,
    pub brake: f32,
    pub gas: f32,
    pub hat_x: f32,
    pub hat_y: f32,
}

fn stick(x: f32, y: f32) -> (i16, i16) {
    let magnitude = x.hypot(y);
    if magnitude < DEADZONE {
        return (0, 0);
    }
    // Rescale past the deadzone so small movements still start at zero.
    let scale = ((magnitude - DEADZONE) / (1.0 - DEADZONE)).min(1.0) / magnitude;
    let to_i16 = |v: f32| (v * scale * 32767.0).round().clamp(-32767.0, 32767.0) as i16;
    // GameStream's Y axis points up; Android's down.
    (to_i16(x), to_i16(-y))
}

fn trigger(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    device: i32,
    state: GamepadState,
    /// D-pad from the hat axes and from key events, kept apart so one
    /// doesn't release the other.
    hat: u32,
    keys: u32,
    digital_triggers: (bool, bool),
}

/// Every controller seen in this stream.
#[derive(Debug, Default)]
pub struct Controllers {
    slots: Vec<Slot>,
}

impl Controllers {
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    fn mask(&self) -> u16 {
        self.slots.iter().fold(0u16, |m, s| m | 1 << s.state.index)
    }

    /// The slot for `device`, assigning the next free one.
    fn slot(&mut self, device: i32) -> Option<&mut Slot> {
        if let Some(i) = self.slots.iter().position(|s| s.device == device) {
            return self.slots.get_mut(i);
        }
        if self.slots.len() >= MAX_CONTROLLERS {
            return None;
        }
        let index = (0..MAX_CONTROLLERS as u8).find(|i| self.slots.iter().all(|s| s.state.index != *i))?;
        self.slots.push(Slot {
            device,
            state: GamepadState {
                index,
                ..Default::default()
            },
            hat: 0,
            keys: 0,
            digital_triggers: (false, false),
        });
        let mask = self.mask();
        for s in &mut self.slots {
            s.state.active_mask = mask;
        }
        self.slots.last_mut()
    }

    /// Android device id of controller `index`, for rumble.
    pub fn device_of(&self, index: u8) -> Option<i32> {
        self.slots.iter().find(|s| s.state.index == index).map(|s| s.device)
    }

    pub fn reset(&mut self) {
        self.slots.clear();
    }

    /// A controller key went down or up; the new state if it changed.
    pub fn key(&mut self, device: i32, keycode: u32, pressed: bool) -> Option<GamepadState> {
        let control = control_for_keycode(keycode)?;
        let slot = self.slot(device)?;
        let before = slot.state;
        match control {
            Control::Button(bit) => {
                if pressed {
                    slot.keys |= bit;
                } else {
                    slot.keys &= !bit;
                }
            }
            Control::LeftTrigger => slot.digital_triggers.0 = pressed,
            Control::RightTrigger => slot.digital_triggers.1 = pressed,
        }
        Self::apply_buttons(slot);
        if slot.digital_triggers.0 {
            slot.state.left_trigger = 255;
        } else if matches!(control, Control::LeftTrigger) {
            slot.state.left_trigger = 0;
        }
        if slot.digital_triggers.1 {
            slot.state.right_trigger = 255;
        } else if matches!(control, Control::RightTrigger) {
            slot.state.right_trigger = 0;
        }
        (slot.state != before).then_some(slot.state)
    }

    /// New joystick axes; the new state if it changed.
    pub fn motion(&mut self, device: i32, a: Axes) -> Option<GamepadState> {
        let slot = self.slot(device)?;
        let before = slot.state;
        slot.state.left_stick = stick(a.x, a.y);
        // Most controllers put the right stick on Z/RZ; some on RX/RY.
        let (rx, ry) = if a.z != 0.0 || a.rz != 0.0 || (a.rx == 0.0 && a.ry == 0.0) {
            (a.z, a.rz)
        } else {
            (a.rx, a.ry)
        };
        slot.state.right_stick = stick(rx, ry);
        if !slot.digital_triggers.0 {
            slot.state.left_trigger = trigger(a.ltrigger.max(a.brake));
        }
        if !slot.digital_triggers.1 {
            slot.state.right_trigger = trigger(a.rtrigger.max(a.gas));
        }
        slot.hat = 0;
        if a.hat_x < -0.5 {
            slot.hat |= buttons::LEFT;
        } else if a.hat_x > 0.5 {
            slot.hat |= buttons::RIGHT;
        }
        if a.hat_y < -0.5 {
            slot.hat |= buttons::UP;
        } else if a.hat_y > 0.5 {
            slot.hat |= buttons::DOWN;
        }
        Self::apply_buttons(slot);
        (slot.state != before).then_some(slot.state)
    }

    fn apply_buttons(slot: &mut Slot) {
        slot.state.buttons = slot.keys | slot.hat;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        assert!(is_controller_source(SOURCE_GAMEPAD));
        assert!(is_controller_source(SOURCE_GAMEPAD | SOURCE_DPAD | 0x101));
        assert!(is_controller_source(SOURCE_JOYSTICK));
        assert!(!is_controller_source(0x101), "keyboard");
        assert!(!is_controller_source(SOURCE_DPAD), "TV remote d-pad");
        assert!(!is_controller_source(0x1002), "touchscreen");
    }

    #[test]
    fn buttons_and_slots() {
        let mut c = Controllers::default();
        let s = c.key(7, 96, true).unwrap();
        assert_eq!((s.index, s.buttons, s.active_mask), (0, buttons::A, 0b1));
        assert!(c.key(7, 96, true).is_none(), "no change, nothing to send");
        let s = c.key(9, 100, true).unwrap();
        assert_eq!((s.index, s.buttons, s.active_mask), (1, buttons::Y, 0b11));
        assert_eq!(c.key(7, 96, false).unwrap().buttons, 0);
        assert_eq!(c.device_of(1), Some(9));
        assert!(c.key(7, 4, true).is_none(), "BACK isn't a controller button here");
    }

    #[test]
    fn sticks_triggers_and_hat() {
        let mut c = Controllers::default();
        let s = c
            .motion(
                1,
                Axes {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                    rz: 1.0,
                    ltrigger: 0.5,
                    gas: 1.0,
                    hat_x: -1.0,
                    hat_y: 1.0,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(s.left_stick, (32767, 0));
        assert_eq!(s.right_stick, (0, -32767), "down on Android is negative here");
        assert_eq!((s.left_trigger, s.right_trigger), (128, 255));
        assert_eq!(s.buttons, buttons::LEFT | buttons::DOWN);

        // Inside the deadzone: centred.
        let s = c
            .motion(
                1,
                Axes {
                    x: 0.03,
                    y: 0.03,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!((s.left_stick, s.buttons), ((0, 0), 0));

        // Right stick on RX/RY when Z/RZ stay at rest.
        let s = c
            .motion(
                1,
                Axes {
                    rx: -1.0,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(s.right_stick, (-32767, 0));
    }

    #[test]
    fn hat_and_key_dpad_dont_release_each_other() {
        let mut c = Controllers::default();
        c.key(1, 19, true); // d-pad up as a key
        let s = c
            .motion(
                1,
                Axes {
                    hat_x: 1.0,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(s.buttons, buttons::UP | buttons::RIGHT);
        let s = c.motion(1, Axes::default()).unwrap();
        assert_eq!(s.buttons, buttons::UP);
    }

    #[test]
    fn digital_triggers() {
        let mut c = Controllers::default();
        assert_eq!(c.key(1, 104, true).unwrap().left_trigger, 255);
        assert!(
            c.motion(1, Axes::default()).is_none(),
            "analog 0 doesn't release a held digital trigger"
        );
        assert_eq!(c.key(1, 104, false).unwrap().left_trigger, 0);
    }

    #[test]
    fn at_most_four_controllers() {
        let mut c = Controllers::default();
        for d in 0..4 {
            assert!(c.key(d, 96, true).is_some());
        }
        assert!(c.key(99, 96, true).is_none());
        c.reset();
        assert_eq!(c.key(99, 96, true).unwrap().index, 0);
    }
}
