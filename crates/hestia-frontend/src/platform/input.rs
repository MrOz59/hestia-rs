//! Input read below the toolkit: physical keys by position, every finger
//! and pen of a touch screen (see [`crate::input`]), game controllers; and
//! whether the pointer can be captured.

use std::sync::RwLock;

use crate::input::RawPointer;

type KeySink = Box<dyn Fn(u32, bool, bool) -> bool + Send + Sync>;
type PointerSink = Box<dyn Fn(RawPointer) -> bool + Send + Sync>;

static KEY_SINK: RwLock<Option<KeySink>> = RwLock::new(None);
static POINTER_SINK: RwLock<Option<PointerSink>> = RwLock::new(None);

/// Where physical keys go: `(Linux key code, pressed, repeat)`, returning
/// true when taken (the controller's `stream_physical_key`).
pub fn set_key_sink(sink: impl Fn(u32, bool, bool) -> bool + Send + Sync + 'static) {
    *KEY_SINK.write().unwrap() = Some(Box::new(sink));
}

/// Where fingers and pens go, returning true when the toolkit must not see
/// the event (the controller's `stream_raw_pointer`).
pub fn set_pointer_sink(sink: impl Fn(RawPointer) -> bool + Send + Sync + 'static) {
    *POINTER_SINK.write().unwrap() = Some(Box::new(sink));
}

/// Whether the window can lock the pointer and report relative motion
/// (Wayland pointer constraints and relative pointer on the desktop).
pub fn has_pointer_capture() -> bool {
    cfg!(not(target_os = "android"))
}

#[cfg(target_os = "android")]
fn physical_key(code: u32, pressed: bool, repeat: bool) -> bool {
    KEY_SINK
        .read()
        .unwrap()
        .as_ref()
        .is_some_and(|sink| sink(code, pressed, repeat))
}

#[cfg(target_os = "android")]
fn raw_pointer(pointer: RawPointer) -> bool {
    POINTER_SINK.read().unwrap().as_ref().is_some_and(|sink| sink(pointer))
}

/// Sees every input event before the toolkit (Slint: the patched backend's
/// hook) and returns true for those it takes: game controllers and
/// physical keys while a stream has the input, pens hovering over it.
/// Touches go on to the toolkit too: it decides what they hit.
#[cfg(target_os = "android")]
pub fn android_input_hook(event: &android_activity::input::InputEvent<'_>) -> bool {
    use android_activity::input::InputEvent;

    if super::controller_tester::capture(event) {
        return true;
    }
    if super::gamepads::android_gamepad_event(event) {
        return true;
    }
    if !super::gamepads::streaming() {
        return false;
    }
    match event {
        InputEvent::KeyEvent(key) => android::key(key),
        InputEvent::MotionEvent(motion) => android::motion(motion),
        _ => false,
    }
}

#[cfg(target_os = "android")]
mod android {
    use android_activity::input::{
        Axis, ButtonState, HistoricalPointer, KeyAction, KeyEvent, MotionAction, MotionEvent, Pointer, ToolType,
    };
    use hestia_core::stream::input::pen_buttons;

    use crate::input::{PointerPhase, PointerTool, RawPointer};

    /// `KeyCharacterMap.VIRTUAL_KEYBOARD`: keys sent by on-screen keyboards.
    const VIRTUAL_KEYBOARD: i32 = -1;
    const SOURCE_TOUCHSCREEN: u32 = 0x0000_1002;
    const SOURCE_STYLUS: u32 = 0x0000_4002;

    /// A hardware keyboard's key, by its scan code (the Linux key code).
    /// On-screen keyboards have none: they type characters, which the
    /// toolkit's key events carry.
    pub(super) fn key(key: &KeyEvent<'_>) -> bool {
        let code = key.scan_code();
        if code <= 0
            || key.device_id() == VIRTUAL_KEYBOARD
            || crate::gamepad::is_controller_source(u32::from(key.source()))
        {
            return false;
        }
        let pressed = match key.action() {
            KeyAction::Down => true,
            KeyAction::Up => false,
            _ => return false,
        };
        super::physical_key(code as u32, pressed, key.repeat_count() > 0)
    }

    /// Fingers and pens on the touch screen (mice and touchpads are the
    /// toolkit's).
    pub(super) fn motion(motion: &MotionEvent<'_>) -> bool {
        let source = u32::from(motion.source());
        if source & SOURCE_TOUCHSCREEN != SOURCE_TOUCHSCREEN && source & SOURCE_STYLUS != SOURCE_STYLUS {
            return false;
        }
        let buttons = pen_buttons_of(motion.button_state());
        let send = |pointer: &Pointer<'_>, phase| super::raw_pointer(raw(pointer, phase, buttons));
        match motion.action() {
            MotionAction::Down | MotionAction::Up => {
                let phase = if motion.action() == MotionAction::Down {
                    PointerPhase::Down
                } else {
                    PointerPhase::Up
                };
                send(&motion.pointer_at_index(0), phase);
                false
            }
            MotionAction::PointerDown | MotionAction::PointerUp => {
                let phase = if motion.action() == MotionAction::PointerDown {
                    PointerPhase::Down
                } else {
                    PointerPhase::Up
                };
                send(&motion.pointer_at_index(motion.pointer_index()), phase);
                false
            }
            MotionAction::Move => {
                for pointer in motion.pointers() {
                    for sample in pointer.history() {
                        super::raw_pointer(historical(&pointer, &sample, buttons));
                    }
                    send(&pointer, PointerPhase::Move);
                }
                false
            }
            MotionAction::Cancel => {
                for pointer in motion.pointers() {
                    send(&pointer, PointerPhase::Cancel);
                }
                false
            }
            MotionAction::HoverEnter | MotionAction::HoverMove => {
                send(&motion.pointer_at_index(0), PointerPhase::Hover)
            }
            MotionAction::HoverExit => send(&motion.pointer_at_index(0), PointerPhase::HoverLeave),
            _ => false,
        }
    }

    fn pen_buttons_of(state: ButtonState) -> u8 {
        // S Pens report the side button as the stylus button on some
        // devices and as the secondary (right) button on others.
        let mut buttons = 0;
        if state.stylus_primary() || state.secondary() {
            buttons |= pen_buttons::PRIMARY;
        }
        if state.stylus_secondary() || state.teriary() {
            buttons |= pen_buttons::SECONDARY;
        }
        buttons
    }

    fn tool(pointer: &Pointer<'_>) -> PointerTool {
        match pointer.tool_type() {
            ToolType::Stylus => PointerTool::Pen,
            ToolType::Eraser => PointerTool::Eraser,
            _ => PointerTool::Finger,
        }
    }

    /// Android's tilt (radians from perpendicular) and orientation (radians
    /// clockwise from up, -π to π) in degrees. A pen that reports no tilt
    /// stands upright, where a rotation means nothing.
    fn angles(tilt: f32, orientation: f32) -> (Option<f32>, Option<f32>) {
        let tilt = tilt.to_degrees();
        let rotation = (tilt > 0.5).then(|| orientation.to_degrees());
        (Some(tilt), rotation)
    }

    fn raw(pointer: &Pointer<'_>, phase: PointerPhase, buttons: u8) -> RawPointer {
        let tool = tool(pointer);
        let (tilt, rotation) = if tool.is_pen() {
            angles(pointer.axis_value(Axis::Tilt), pointer.orientation())
        } else {
            (None, None)
        };
        RawPointer {
            id: pointer.pointer_id(),
            phase,
            tool,
            x: pointer.x(),
            y: pointer.y(),
            pressure: Some(pointer.pressure()),
            distance: None,
            tilt,
            rotation,
            buttons: if tool.is_pen() { buttons } else { 0 },
        }
    }

    /// A batched earlier sample of a moving pointer.
    fn historical(pointer: &Pointer<'_>, sample: &HistoricalPointer<'_>, buttons: u8) -> RawPointer {
        let tool = tool(pointer);
        let (tilt, rotation) = if tool.is_pen() {
            angles(sample.axis_value(Axis::Tilt), sample.orientation())
        } else {
            (None, None)
        };
        RawPointer {
            id: pointer.pointer_id(),
            phase: PointerPhase::Move,
            tool,
            x: sample.x(),
            y: sample.y(),
            pressure: Some(sample.pressure()),
            distance: None,
            tilt,
            rotation,
            buttons: if tool.is_pen() { buttons } else { 0 },
        }
    }
}
