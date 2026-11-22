//! Input events and their GameStream wire encoding (port of the packet
//! layouts in moonlight-common-c `Input.h` / `InputStream.c`). Packets start
//! with `size: u32 BE` (bytes after this field) and `magic: u32 LE`, and are
//! sent as control-stream input messages on per-class channels.

use crate::control::{CHANNEL_GAMEPAD_BASE, CHANNEL_KEYBOARD, CHANNEL_MOUSE, CHANNEL_PEN, CHANNEL_TOUCH};

/// Windows virtual-key code, which is what GameStream hosts expect.
pub type VirtualKey = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Modifiers {
    fn bits(self) -> u8 {
        (self.shift as u8) | (self.ctrl as u8) << 1 | (self.alt as u8) << 2 | (self.meta as u8) << 3
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    X1,
    X2,
}

impl MouseButton {
    fn code(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
            Self::X1 => 4,
            Self::X2 => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    /// Moving above the screen without touching it (pens).
    Hover,
    Down,
    Up,
    Move,
    Cancel,
    /// Left the hover range (pens).
    HoverLeave,
}

impl TouchPhase {
    fn code(self) -> u8 {
        match self {
            Self::Hover => 0x00,
            Self::Down => 0x01,
            Self::Up => 0x02,
            Self::Move => 0x03,
            Self::Cancel => 0x04,
            Self::HoverLeave => 0x06,
        }
    }
}

/// Stylus tool (S Pen and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenTool {
    Pen,
    Eraser,
}

/// Pen button bits.
pub mod pen_buttons {
    /// The barrel button (S Pen's side button).
    pub const PRIMARY: u8 = 0x01;
    pub const SECONDARY: u8 = 0x02;
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    Key {
        key: VirtualKey,
        pressed: bool,
        modifiers: Modifiers,
    },
    /// Unicode text (soft keyboards, IME commits).
    Text(String),
    MouseMove {
        dx: i16,
        dy: i16,
    },
    /// Absolute position within a reference surface of `width`×`height`.
    MousePosition {
        x: i16,
        y: i16,
        width: i16,
        height: i16,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// High-resolution scroll; 120 = one notch.
    Scroll {
        vertical: i16,
        horizontal: i16,
    },
    /// Normalised coordinates (0.0–1.0).
    Touch {
        pointer_id: u32,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
    },
    /// Normalised coordinates (0.0–1.0), like [`InputEvent::Touch`].
    Pen {
        tool: PenTool,
        phase: TouchPhase,
        x: f32,
        y: f32,
        /// Pressure (0.0–1.0) while touching; distance from the screen
        /// (0.0–1.0, 0 when unknown) while hovering.
        pressure: f32,
        /// Degrees from perpendicular (0–90), if the pen reports it.
        tilt: Option<f32>,
        /// Degrees around the screen normal, clockwise from up (0–359), if
        /// the pen reports it.
        rotation: Option<f32>,
        /// Bitmask of `pen_buttons::*` held.
        buttons: u8,
    },
    Gamepad(GamepadState),
}

/// Full state of one controller, sent whenever anything changes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GamepadState {
    pub index: u8,
    /// Bitmask of all connected gamepads.
    pub active_mask: u16,
    /// Bitmask of `buttons::*`.
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub left_stick: (i16, i16),
    pub right_stick: (i16, i16),
}

/// Controller button bits (Moonlight/GameStream layout).
pub mod buttons {
    pub const UP: u32 = 0x0001;
    pub const DOWN: u32 = 0x0002;
    pub const LEFT: u32 = 0x0004;
    pub const RIGHT: u32 = 0x0008;
    pub const PLAY: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LS_CLICK: u32 = 0x0040;
    pub const RS_CLICK: u32 = 0x0080;
    pub const LB: u32 = 0x0100;
    pub const RB: u32 = 0x0200;
    pub const SPECIAL: u32 = 0x0400;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;
    pub const PADDLE1: u32 = 0x10000;
    pub const PADDLE2: u32 = 0x20000;
    pub const PADDLE3: u32 = 0x40000;
    pub const PADDLE4: u32 = 0x80000;
    pub const TOUCHPAD: u32 = 0x100000;
    pub const MISC: u32 = 0x200000;
}

/// An encoded input packet and the control channel it travels on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedInput {
    pub channel: u8,
    pub bytes: Vec<u8>,
}

fn packet(magic: u32, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    v.extend_from_slice(&magic.to_le_bytes());
    v.extend_from_slice(body);
    v
}

const KEY_DOWN: u32 = 0x03;
const KEY_UP: u32 = 0x04;
const UTF8_TEXT: u32 = 0x17;
const MOUSE_REL: u32 = 0x07;
const MOUSE_ABS: u32 = 0x05;
const MOUSE_BUTTON_DOWN: u32 = 0x08;
const MOUSE_BUTTON_UP: u32 = 0x09;
const SCROLL: u32 = 0x0a;
const SS_HSCROLL: u32 = 0x5500_0001;
const SS_TOUCH: u32 = 0x5500_0002;
const SS_PEN: u32 = 0x5500_0003;
const MULTI_CONTROLLER: u32 = 0x0c;
/// "Rotation unknown" marker.
const ROTATION_UNKNOWN: u16 = 0xffff;
/// "Tilt unknown" marker.
const TILT_UNKNOWN: u8 = 0xff;
const MAX_TEXT_CHUNK: usize = 32;

pub fn encode(event: &InputEvent) -> Vec<EncodedInput> {
    let one = |channel: u8, bytes: Vec<u8>| vec![EncodedInput { channel, bytes }];
    match event {
        InputEvent::Key {
            key,
            pressed,
            modifiers,
        } => {
            let mut body = Vec::with_capacity(6);
            body.push(0); // flags
            body.extend_from_slice(&(0x8000 | key).to_le_bytes());
            body.push(modifiers.bits());
            body.extend_from_slice(&[0, 0]);
            one(
                CHANNEL_KEYBOARD,
                packet(if *pressed { KEY_DOWN } else { KEY_UP }, &body),
            )
        }
        InputEvent::Text(text) => {
            // Split on character boundaries into <= 32-byte chunks.
            let mut out = Vec::new();
            let mut chunk = String::new();
            for c in text.chars() {
                if chunk.len() + c.len_utf8() > MAX_TEXT_CHUNK {
                    out.push(EncodedInput {
                        channel: CHANNEL_KEYBOARD,
                        bytes: packet(UTF8_TEXT, chunk.as_bytes()),
                    });
                    chunk.clear();
                }
                chunk.push(c);
            }
            if !chunk.is_empty() {
                out.push(EncodedInput {
                    channel: CHANNEL_KEYBOARD,
                    bytes: packet(UTF8_TEXT, chunk.as_bytes()),
                });
            }
            out
        }
        InputEvent::MouseMove { dx, dy } => {
            if *dx == 0 && *dy == 0 {
                return Vec::new();
            }
            let mut body = Vec::with_capacity(4);
            body.extend_from_slice(&dx.to_be_bytes());
            body.extend_from_slice(&dy.to_be_bytes());
            one(CHANNEL_MOUSE, packet(MOUSE_REL, &body))
        }
        InputEvent::MousePosition { x, y, width, height } => {
            let mut body = Vec::with_capacity(10);
            body.extend_from_slice(&x.to_be_bytes());
            body.extend_from_slice(&y.to_be_bytes());
            body.extend_from_slice(&0i16.to_be_bytes());
            body.extend_from_slice(&(width - 1).to_be_bytes());
            body.extend_from_slice(&(height - 1).to_be_bytes());
            one(CHANNEL_MOUSE, packet(MOUSE_ABS, &body))
        }
        InputEvent::MouseButton { button, pressed } => one(
            CHANNEL_MOUSE,
            packet(
                if *pressed { MOUSE_BUTTON_DOWN } else { MOUSE_BUTTON_UP },
                &[button.code()],
            ),
        ),
        InputEvent::Scroll { vertical, horizontal } => {
            let mut out = Vec::new();
            if *vertical != 0 {
                let mut body = Vec::with_capacity(6);
                body.extend_from_slice(&vertical.to_be_bytes());
                body.extend_from_slice(&vertical.to_be_bytes());
                body.extend_from_slice(&[0, 0]);
                out.push(EncodedInput {
                    channel: CHANNEL_MOUSE,
                    bytes: packet(SCROLL, &body),
                });
            }
            if *horizontal != 0 {
                out.push(EncodedInput {
                    channel: CHANNEL_MOUSE,
                    bytes: packet(SS_HSCROLL, &horizontal.to_be_bytes()),
                });
            }
            out
        }
        InputEvent::Touch {
            pointer_id,
            phase,
            x,
            y,
            pressure,
        } => {
            let mut body = Vec::with_capacity(28);
            body.push(phase.code());
            body.push(0);
            body.extend_from_slice(&ROTATION_UNKNOWN.to_le_bytes());
            body.extend_from_slice(&pointer_id.to_le_bytes());
            for f in [*x, *y, *pressure, 0.0, 0.0] {
                body.extend_from_slice(&f.to_le_bytes());
            }
            one(CHANNEL_TOUCH, packet(SS_TOUCH, &body))
        }
        InputEvent::Pen {
            tool,
            phase,
            x,
            y,
            pressure,
            tilt,
            rotation,
            buttons,
        } => {
            let mut body = Vec::with_capacity(28);
            body.push(phase.code());
            body.push(match tool {
                PenTool::Pen => 0x01,
                PenTool::Eraser => 0x02,
            });
            body.push(*buttons & (pen_buttons::PRIMARY | pen_buttons::SECONDARY));
            body.push(0);
            for f in [*x, *y, *pressure] {
                body.extend_from_slice(&f.to_le_bytes());
            }
            let rotation = rotation.map_or(ROTATION_UNKNOWN, |r| (r.rem_euclid(360.0) as u16).min(359));
            body.extend_from_slice(&rotation.to_le_bytes());
            body.push(tilt.map_or(TILT_UNKNOWN, |t| t.clamp(0.0, 90.0) as u8));
            body.push(0);
            for f in [0.0f32, 0.0] {
                body.extend_from_slice(&f.to_le_bytes());
            }
            one(CHANNEL_PEN, packet(SS_PEN, &body))
        }
        InputEvent::Gamepad(g) => {
            let mut body = Vec::with_capacity(26);
            body.extend_from_slice(&0x001au16.to_le_bytes());
            body.extend_from_slice(&(g.index as u16).to_le_bytes());
            body.extend_from_slice(&g.active_mask.to_le_bytes());
            body.extend_from_slice(&0x0014u16.to_le_bytes());
            body.extend_from_slice(&(g.buttons as u16).to_le_bytes());
            body.push(g.left_trigger);
            body.push(g.right_trigger);
            for v in [g.left_stick.0, g.left_stick.1, g.right_stick.0, g.right_stick.1] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            body.extend_from_slice(&0x009cu16.to_le_bytes());
            body.extend_from_slice(&((g.buttons >> 16) as u16).to_le_bytes());
            body.extend_from_slice(&0x0055u16.to_le_bytes());
            one(CHANNEL_GAMEPAD_BASE + (g.index & 0x0f), packet(MULTI_CONTROLLER, &body))
        }
    }
}

/// Windows virtual-key codes for keys that aren't plain letters/digits.
pub mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0d;
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    pub const MENU: u16 = 0x12;
    pub const PAUSE: u16 = 0x13;
    pub const CAPITAL: u16 = 0x14;
    pub const ESCAPE: u16 = 0x1b;
    pub const SPACE: u16 = 0x20;
    pub const PRIOR: u16 = 0x21;
    pub const NEXT: u16 = 0x22;
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const SNAPSHOT: u16 = 0x2c;
    pub const INSERT: u16 = 0x2d;
    pub const DELETE: u16 = 0x2e;
    pub const LWIN: u16 = 0x5b;
    pub const RWIN: u16 = 0x5c;
    pub const APPS: u16 = 0x5d;
    /// Keypad 0; keypad 1–9 follow.
    pub const NUMPAD0: u16 = 0x60;
    pub const MULTIPLY: u16 = 0x6a;
    pub const ADD: u16 = 0x6b;
    /// Keypad comma.
    pub const SEPARATOR: u16 = 0x6c;
    pub const SUBTRACT: u16 = 0x6d;
    pub const DECIMAL: u16 = 0x6e;
    pub const DIVIDE: u16 = 0x6f;
    /// F1; F2–F24 follow.
    pub const F1: u16 = 0x70;
    pub const NUMLOCK: u16 = 0x90;
    pub const SCROLL: u16 = 0x91;
    pub const LSHIFT: u16 = 0xa0;
    pub const RSHIFT: u16 = 0xa1;
    pub const LCONTROL: u16 = 0xa2;
    pub const RCONTROL: u16 = 0xa3;
    pub const LMENU: u16 = 0xa4;
    pub const RMENU: u16 = 0xa5;
    pub const OEM_1: u16 = 0xba; // ;:
    pub const OEM_PLUS: u16 = 0xbb;
    pub const OEM_COMMA: u16 = 0xbc;
    pub const OEM_MINUS: u16 = 0xbd;
    pub const OEM_PERIOD: u16 = 0xbe;
    pub const OEM_2: u16 = 0xbf; // /?
    pub const OEM_3: u16 = 0xc0; // `~
    pub const OEM_4: u16 = 0xdb; // [{
    pub const OEM_5: u16 = 0xdc; // \|
    pub const OEM_6: u16 = 0xdd; // ]}
    pub const OEM_7: u16 = 0xde; // '"
    /// The extra key of ISO keyboards, next to the left Shift (`<>`, `\|`).
    pub const OEM_102: u16 = 0xe2;
    /// The extra key of Brazilian (ABNT2) and Japanese keyboards, next to the
    /// right Shift (`/?`, `\_`).
    pub const ABNT_C1: u16 = 0xc1;

    /// Whether `key` is one of the modifier keys.
    pub fn is_modifier(key: u16) -> bool {
        matches!(
            key,
            SHIFT | CONTROL | MENU | LSHIFT | RSHIFT | LCONTROL | RCONTROL | LMENU | RMENU | LWIN | RWIN
        )
    }

    /// Maps a printable US-layout character to its key and whether Shift is implied.
    pub fn from_char(c: char) -> Option<(u16, bool)> {
        let lower = c.to_ascii_lowercase();
        Some(match lower {
            'a'..='z' => (lower.to_ascii_uppercase() as u16, c.is_ascii_uppercase()),
            '0'..='9' => (c as u16, false),
            ' ' => (SPACE, false),
            ')' => (b'0' as u16, true),
            '!' => (b'1' as u16, true),
            '@' => (b'2' as u16, true),
            '#' => (b'3' as u16, true),
            '$' => (b'4' as u16, true),
            '%' => (b'5' as u16, true),
            '^' => (b'6' as u16, true),
            '&' => (b'7' as u16, true),
            '*' => (b'8' as u16, true),
            '(' => (b'9' as u16, true),
            ';' => (OEM_1, false),
            ':' => (OEM_1, true),
            '=' => (OEM_PLUS, false),
            '+' => (OEM_PLUS, true),
            ',' => (OEM_COMMA, false),
            '<' => (OEM_COMMA, true),
            '-' => (OEM_MINUS, false),
            '_' => (OEM_MINUS, true),
            '.' => (OEM_PERIOD, false),
            '>' => (OEM_PERIOD, true),
            '/' => (OEM_2, false),
            '?' => (OEM_2, true),
            '`' => (OEM_3, false),
            '~' => (OEM_3, true),
            '[' => (OEM_4, false),
            '{' => (OEM_4, true),
            '\\' => (OEM_5, false),
            '|' => (OEM_5, true),
            ']' => (OEM_6, false),
            '}' => (OEM_6, true),
            '\'' => (OEM_7, false),
            '"' => (OEM_7, true),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_layout() {
        let p = encode(&InputEvent::Key {
            key: 0x41,
            pressed: true,
            modifiers: Modifiers {
                shift: true,
                ..Default::default()
            },
        });
        assert_eq!(p[0].channel, CHANNEL_KEYBOARD);
        // size=10 BE, magic=3 LE, flags, key 0x8041 LE, modifiers, zero
        assert_eq!(p[0].bytes, vec![0, 0, 0, 10, 3, 0, 0, 0, 0, 0x41, 0x80, 1, 0, 0]);
    }

    #[test]
    fn mouse_layouts() {
        let rel = &encode(&InputEvent::MouseMove { dx: 1, dy: -1 })[0].bytes;
        assert_eq!(rel, &vec![0, 0, 0, 8, 7, 0, 0, 0, 0, 1, 0xff, 0xff]);
        let abs = &encode(&InputEvent::MousePosition {
            x: 10,
            y: 20,
            width: 1920,
            height: 1080,
        })[0]
            .bytes;
        assert_eq!(abs.len(), 4 + 14);
        assert_eq!(&abs[14..18], &[0x07, 0x7f, 0x04, 0x37]); // 1919, 1079
        let btn = &encode(&InputEvent::MouseButton {
            button: MouseButton::Right,
            pressed: false,
        })[0]
            .bytes;
        assert_eq!(btn, &vec![0, 0, 0, 5, 9, 0, 0, 0, 3]);
        let scroll = encode(&InputEvent::Scroll {
            vertical: 120,
            horizontal: -120,
        });
        assert_eq!(scroll.len(), 2);
        assert_eq!(&scroll[0].bytes[8..12], &[0, 120, 0, 120]);
    }

    #[test]
    fn sizes_match_structs() {
        let touch = &encode(&InputEvent::Touch {
            pointer_id: 1,
            phase: TouchPhase::Down,
            x: 0.5,
            y: 0.5,
            pressure: 1.0,
        })[0];
        assert_eq!(touch.bytes.len(), 4 + 32);
        let pen = &encode(&InputEvent::Pen {
            tool: PenTool::Pen,
            phase: TouchPhase::Move,
            x: 0.1,
            y: 0.2,
            pressure: 0.3,
            tilt: Some(10.0),
            rotation: Some(45.0),
            buttons: 0,
        })[0];
        assert_eq!(pen.bytes.len(), 4 + 4 + 28);
        let pad = &encode(&InputEvent::Gamepad(GamepadState::default()))[0];
        assert_eq!(pad.bytes.len(), 4 + 30);
        assert_eq!(pad.channel, CHANNEL_GAMEPAD_BASE);
        let text = encode(&InputEvent::Text("ç".repeat(20)));
        assert_eq!(text.len(), 2, "40 bytes of UTF-8 split into two packets");
    }

    #[test]
    fn pen_layout() {
        let pen = &encode(&InputEvent::Pen {
            tool: PenTool::Eraser,
            phase: TouchPhase::Down,
            x: 0.25,
            y: 0.5,
            pressure: 1.0,
            tilt: Some(30.0),
            rotation: Some(-90.0),
            buttons: pen_buttons::PRIMARY | 0x80,
        })[0];
        assert_eq!(pen.channel, CHANNEL_PEN);
        // After size and magic: event type, tool, buttons, padding.
        assert_eq!(&pen.bytes[8..12], &[0x01, 0x02, 0x01, 0]);
        assert_eq!(&pen.bytes[12..16], &0.25f32.to_le_bytes());
        // Rotation 270° LE, tilt 30.
        assert_eq!(&pen.bytes[24..28], &[0x0e, 0x01, 30, 0]);

        let hover = &encode(&InputEvent::Pen {
            tool: PenTool::Pen,
            phase: TouchPhase::HoverLeave,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            tilt: None,
            rotation: None,
            buttons: 0,
        })[0];
        assert_eq!(hover.bytes[8], 0x06);
        assert_eq!(&hover.bytes[24..27], &[0xff, 0xff, 0xff], "rotation and tilt unknown");
    }
}
