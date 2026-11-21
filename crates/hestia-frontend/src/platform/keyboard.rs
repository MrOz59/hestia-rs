//! System keyboard and IME events.

#[cfg(target_os = "android")]
use super::android::with_helper;

use crate::stream_input::ImeEvent;

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
static IME_SINK: std::sync::OnceLock<Box<dyn Fn(ImeEvent) + Send + Sync>> = std::sync::OnceLock::new();

/// Where keyboard text goes (the controller sends it to the stream).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn set_ime_sink(sink: impl Fn(ImeEvent) + Send + Sync + 'static) {
    let _ = IME_SINK.set(Box::new(sink));
}

/// Whether this platform has an on-screen keyboard to show.
pub fn has_soft_keyboard() -> bool {
    cfg!(target_os = "android")
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
    if let Some(sink) = IME_SINK.get() {
        sink(event);
    }
}
