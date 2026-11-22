//! Desktop input below Slint, from winit: keys by their position on the
//! keyboard, every finger of a touch screen, and relative mouse motion with
//! a locked pointer for mouse capture. Slint's key events only carry the
//! text a key types (none for dead keys, the US layout's keys for others),
//! it forwards only the first finger of a touch, and it has no relative
//! motion; the controller's intents take what it can't carry.
//!
//! Slint's winit backend runs [`Handler`] before it handles each event, so
//! keys the stream takes never reach the UI, and touches still do (the UI
//! decides what they hit, see `hestia_frontend::input`).

use std::sync::{Arc, OnceLock};

use hestia_frontend::Controller;
use hestia_frontend::input::{PointerPhase, RawPointer};
use log::warn;
use slint::winit_030::winit::event::{DeviceEvent, DeviceId, ElementState, TouchPhase, WindowEvent};
use slint::winit_030::winit::event_loop::ActiveEventLoop;
use slint::winit_030::winit::platform::scancode::PhysicalKeyExtScancode;
use slint::winit_030::winit::window::{CursorGrabMode, Window, WindowId};
use slint::winit_030::{CustomApplicationHandler, EventResult, WinitWindowAccessor};

/// The controller the handler feeds; the window (and so the event loop)
/// exists before it does.
static CONTROLLER: OnceLock<Arc<Controller>> = OnceLock::new();

/// Selects Slint's winit backend with [`Handler`]. Call before the first
/// window is created; without it (another backend) keys go by the text
/// they type and the mouse can't be captured.
pub(crate) fn install() {
    if let Err(e) = slint::BackendSelector::new()
        .with_winit_custom_application_handler(Handler)
        .select()
    {
        warn!("winit input handler unavailable ({e}): keys go by character, no mouse capture");
    }
}

/// Starts feeding `controller`.
pub(crate) fn connect(controller: &Arc<Controller>) {
    let _ = CONTROLLER.set(controller.clone());
}

/// Locks the pointer where it is (Wayland pointer constraints; confines it
/// to the window where locking isn't supported) or frees it. The stream
/// view hides the cursor itself.
pub(crate) fn set_pointer_capture(window: &slint::Window, on: bool) {
    window.with_winit_window(|w: &Window| {
        let result = if on {
            w.set_cursor_grab(CursorGrabMode::Locked)
                .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined))
        } else {
            w.set_cursor_grab(CursorGrabMode::None)
        };
        if let Err(e) = result {
            warn!("pointer capture {}: {e}", if on { "on" } else { "off" });
        }
    });
}

/// Stable small ids for winit's touch ids.
fn finger_id(id: u64) -> i32 {
    (id % i32::MAX as u64) as i32
}

pub(crate) struct Handler;

impl CustomApplicationHandler for Handler {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _winit_window: Option<&Window>,
        _slint_window: Option<&slint::Window>,
        event: &WindowEvent,
    ) -> EventResult {
        let Some(controller) = CONTROLLER.get() else {
            return EventResult::Propagate;
        };
        match event {
            WindowEvent::KeyboardInput {
                event, is_synthetic, ..
            } => {
                if *is_synthetic {
                    // Keys already held when the window gets the focus back
                    // (the Alt of an Alt+Tab): not pressed in the stream.
                    return if controller.stream_has_keyboard() {
                        EventResult::PreventDefault
                    } else {
                        EventResult::Propagate
                    };
                }
                let Some(code) = event.physical_key.to_scancode() else {
                    return EventResult::Propagate;
                };
                let pressed = event.state == ElementState::Pressed;
                if controller.stream_physical_key(code, pressed, event.repeat) {
                    EventResult::PreventDefault
                } else {
                    EventResult::Propagate
                }
            }
            WindowEvent::Touch(touch) => {
                let phase = match touch.phase {
                    TouchPhase::Started => PointerPhase::Down,
                    TouchPhase::Moved => PointerPhase::Move,
                    TouchPhase::Ended => PointerPhase::Up,
                    TouchPhase::Cancelled => PointerPhase::Cancel,
                };
                let mut pointer = RawPointer::finger(
                    finger_id(touch.id),
                    phase,
                    touch.location.x as f32,
                    touch.location.y as f32,
                );
                pointer.pressure = touch.force.map(|f| f.normalized() as f32);
                if controller.stream_raw_pointer(pointer) {
                    EventResult::PreventDefault
                } else {
                    EventResult::Propagate
                }
            }
            _ => EventResult::Propagate,
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _device_id: DeviceId, event: DeviceEvent) -> EventResult {
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event
            && let Some(controller) = CONTROLLER.get()
        {
            controller.stream_mouse_motion(dx, dy);
        }
        EventResult::Propagate
    }
}

#[cfg(test)]
mod tests {
    use super::finger_id;

    #[test]
    fn finger_ids_fit() {
        assert_eq!(finger_id(3), 3);
        assert!(finger_id(u64::MAX) >= 0);
    }
}
