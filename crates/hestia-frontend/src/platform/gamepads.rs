//! Controller slots, gamepad dispatch and platform rumble.

#[cfg(target_os = "android")]
use super::android::with_helper;

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod controllers {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, RwLock};

    use hestia_core::stream::input::GamepadState;

    use crate::gamepad::Controllers;

    pub(super) static ACTIVE: AtomicBool = AtomicBool::new(false);
    pub(super) static STATE: Mutex<Controllers> = Mutex::new(Controllers::new());
    type GamepadSink = Box<dyn Fn(GamepadState) + Send + Sync>;
    pub(super) static SINK: RwLock<Option<GamepadSink>> = RwLock::new(None);

    pub(super) fn state() -> std::sync::MutexGuard<'static, Controllers> {
        STATE.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn send(update: Option<GamepadState>) {
        if let (Some(state), Some(sink)) = (update, SINK.read().unwrap().as_ref()) {
            sink(state);
        }
    }

    pub(super) fn dispatch(change: impl FnOnce(&mut Controllers) -> Option<GamepadState>) {
        let mut controllers = state();
        if active() {
            // Queue this update before another device can reuse the slot.
            send(change(&mut controllers));
        }
    }

    pub(super) fn active() -> bool {
        ACTIVE.load(Ordering::Relaxed)
    }
}

/// Where controller state goes (the controller sends it to the stream).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn set_gamepad_sink(sink: impl Fn(hestia_core::stream::input::GamepadState) + Send + Sync + 'static) {
    *controllers::SINK.write().unwrap() = Some(Box::new(sink));
}

/// Releases controller buttons and frees their slots when Android enters PiP.
#[cfg(target_os = "android")]
pub(crate) fn release_android_gamepads() {
    let mut controllers = controllers::state();
    let devices: Vec<_> = (0..crate::gamepad::MAX_CONTROLLERS as u8)
        .filter_map(|index| controllers.device_of(index))
        .collect();
    for &device in &devices {
        controllers::send(controllers.remove(device));
    }
    drop(controllers);
    for device in devices {
        rumble_device(device, 0, 0);
    }
    rumble_device(-1, 0, 0);
}

/// Controllers drive the host only while streaming; otherwise their d-pad
/// and B button navigate the app as usual. Ending clears them and stops
/// any rumble.
pub fn set_gamepads_streaming(on: bool) {
    let mut controllers = controllers::state();
    controllers::ACTIVE.store(on, std::sync::atomic::Ordering::Relaxed);
    if !on {
        let devices: Vec<i32> = (0..crate::gamepad::MAX_CONTROLLERS as u8)
            .filter_map(|i| controllers.device_of(i))
            .collect();
        controllers.reset();
        drop(controllers);
        for device in devices {
            rumble_device(device, 0, 0);
        }
        rumble_device(-1, 0, 0);
    }
    #[cfg(target_os = "linux")]
    if on {
        crate::desktop_pads::stream_started();
    }
}

/// A controller's whole state, by its own `device` key: the on-screen
/// one, or a desktop controller. It takes a slot like a physical one;
/// nothing is sent unless a stream runs.
pub fn gamepad_state(device: i32, buttons: u32, left_stick: (i16, i16), right_stick: (i16, i16), triggers: (u8, u8)) {
    controllers::dispatch(|controllers| controllers.set(device, buttons, left_stick, right_stick, triggers));
}

/// Controller `device` went away: release everything, free its slot.
pub fn gamepad_gone(device: i32) {
    controllers::dispatch(|controllers| controllers.remove(device));
}

/// The on-screen controller's state.
pub fn onscreen_gamepad(buttons: u32, left_stick: (i16, i16), right_stick: (i16, i16), triggers: (u8, u8)) {
    gamepad_state(crate::gamepad::ON_SCREEN, buttons, left_stick, right_stick, triggers);
}

/// The on-screen controller went away.
pub fn onscreen_gamepad_off() {
    gamepad_gone(crate::gamepad::ON_SCREEN);
}

/// Rumble for controller `index` from the host.
pub fn rumble(index: u8, low: u16, high: u16) {
    let device = controllers::state().device_of(index).unwrap_or(-1);
    rumble_device(device, low, high);
}

fn rumble_device(device: i32, low: u16, high: u16) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            env.call_static_method(
                helper,
                "rumble",
                "(Landroid/app/Activity;III)V",
                &[
                    JValue::Object(activity),
                    JValue::Int(device),
                    JValue::Int(i32::from(low)),
                    JValue::Int(i32::from(high)),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::debug!("rumble failed: {e}");
        }
    }
    #[cfg(target_os = "linux")]
    crate::desktop_pads::rumble(device, low, high);
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    let _ = (device, low, high);
}

/// Whether a stream is running (controllers then drive the host).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(super) fn streaming() -> bool {
    controllers::active()
}

/// Game controller buttons and sticks while a stream runs; true when taken.
/// Part of [`super::android_input_hook`].
#[cfg(target_os = "android")]
pub(super) fn android_gamepad_event(event: &android_activity::input::InputEvent<'_>) -> bool {
    use android_activity::input::{InputEvent, KeyAction, MotionAction};

    use crate::gamepad;

    if !controllers::active() {
        return false;
    }
    match event {
        InputEvent::KeyEvent(key) => {
            if !gamepad::is_controller_source(u32::from(key.source())) {
                return false;
            }
            let code = u32::from(key.key_code());
            if gamepad::control_for_keycode(code).is_none() {
                return false; // e.g. BACK, which opens the stream menu
            }
            let pressed = match key.action() {
                KeyAction::Down => true,
                KeyAction::Up => false,
                _ => return true,
            };
            if pressed && key.repeat_count() > 0 {
                return true;
            }
            controllers::dispatch(|controllers| controllers.key(key.device_id(), code, pressed));
            true
        }
        InputEvent::MotionEvent(motion) => {
            let source = u32::from(motion.source());
            if source & gamepad::SOURCE_JOYSTICK != gamepad::SOURCE_JOYSTICK || motion.action() != MotionAction::Move {
                return false;
            }
            let axes = android_axes(&motion);
            controllers::dispatch(|controllers| controllers.motion(motion.device_id(), axes));
            true
        }
        _ => false,
    }
}

/// Shared sampling: the debug tester must inspect exactly the same axes as streaming.
#[cfg(target_os = "android")]
pub(super) fn android_axes(motion: &android_activity::input::MotionEvent<'_>) -> crate::gamepad::Axes {
    use android_activity::input::Axis;
    use crate::gamepad::Axes;
    let p = motion.pointer_at_index(0);
    Axes {
        x: p.axis_value(Axis::X),
        y: p.axis_value(Axis::Y),
        z: p.axis_value(Axis::Z),
        rz: p.axis_value(Axis::Rz),
        rx: p.axis_value(Axis::Rx),
        ry: p.axis_value(Axis::Ry),
        ltrigger: p.axis_value(Axis::Ltrigger),
        rtrigger: p.axis_value(Axis::Rtrigger),
        brake: p.axis_value(Axis::Brake),
        gas: p.axis_value(Axis::Gas),
        hat_x: p.axis_value(Axis::HatX),
        hat_y: p.axis_value(Axis::HatY),
    }
}
