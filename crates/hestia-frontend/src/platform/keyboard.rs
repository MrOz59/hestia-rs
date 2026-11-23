//! System keyboard and IME events.

#[cfg(target_os = "android")]
use super::android::with_helper;
use std::sync::RwLock;

use crate::stream_input::ImeEvent;

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
type ImeSink = Box<dyn Fn(ImeEvent) + Send + Sync>;
static IME_SINK: RwLock<Option<ImeSink>> = RwLock::new(None);

/// Where keyboard text goes (the controller sends it to the stream).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn set_ime_sink(sink: impl Fn(ImeEvent) + Send + Sync + 'static) {
    *IME_SINK.write().unwrap() = Some(Box::new(sink));
}

/// Whether this platform has an on-screen keyboard to show.
pub fn has_soft_keyboard() -> bool {
    cfg!(target_os = "android")
}


/// Whether the user has a real, non-IME keyboard attached. Android tracks
/// USB/Bluetooth keyboards via its current native configuration. Recheck
/// whenever the settings page opens so hot-plugging does not need a restart.
pub fn has_physical_keyboard() -> bool {
    #[cfg(target_os = "android")]
    {
        use android_activity::ndk::configuration::Keyboard;
        super::android::android_app().is_some_and(|app| app.config().keyboard() != Keyboard::NoKeys)
    }
    #[cfg(not(target_os = "android"))]
    {
        // On desktop the keyboard comes from the compositor seat; keyboard
        // configuration is expected to be available independently of touch.
        true
    }
}

/// Shows or hides the on-screen keyboard that types into the host.
pub fn set_keyboard_visible(visible: bool) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            env.call_static_method(
                helper,
                "setKeyboardVisible",
                "(Landroid/app/Activity;Z)V",
                &[JValue::Object(activity), JValue::Bool(u8::from(visible))],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("toggling the keyboard failed: {e}");
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = visible;
}

#[cfg(target_os = "android")]
pub(super) fn ime_event(event: ImeEvent) {
    if let Some(sink) = IME_SINK.read().unwrap().as_ref() {
        sink(event);
    }
}
