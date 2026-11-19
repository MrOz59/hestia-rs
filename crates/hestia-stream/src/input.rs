//! Input events a front-end hands to the streaming session. They are encoded
//! into GameStream input packets (encrypted on the control stream) by the
//! session; the UI never deals with wire formats.

/// Windows virtual-key code, which is what GameStream hosts expect.
pub type VirtualKey = u16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
    Hover,
}

/// Stylus tool (S Pen and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenTool {
    Pen,
    Eraser,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Key {
        key: VirtualKey,
        pressed: bool,
        modifiers: Modifiers,
    },
    /// Unicode text (soft keyboards, IME commits).
    Text(char),
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
    Pen {
        tool: PenTool,
        phase: TouchPhase,
        x: f32,
        y: f32,
        pressure: f32,
        /// Degrees, 0 = perpendicular to the screen.
        tilt: f32,
        /// Degrees around the screen normal.
        rotation: f32,
        barrel_button: bool,
    },
    Gamepad(GamepadState),
}

/// Full state of one controller, sent whenever anything changes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GamepadState {
    pub index: u8,
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
