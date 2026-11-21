//! System services used by the graphical client.

#[cfg(target_os = "android")]
mod android;
mod clipboard;
mod display;
mod gamepads;
mod keyboard;
#[cfg(target_os = "android")]
mod network;
mod reports;

#[cfg(target_os = "android")]
pub use android::{set_android_app, video_decoders};
pub use clipboard::{clipboard_stamp, clipboard_text, set_clipboard_text};
pub use display::{device_summary, native_display, set_stream_mode};
#[cfg(target_os = "android")]
pub use gamepads::android_input_hook;
pub use gamepads::{
    gamepad_gone, gamepad_state, onscreen_gamepad, onscreen_gamepad_off, rumble, set_gamepad_sink,
    set_gamepads_streaming,
};
pub use keyboard::{has_soft_keyboard, set_form_navigation, set_ime_sink, set_keyboard_visible};
#[cfg(target_os = "android")]
pub use network::route_for;
pub use reports::{export_label, export_report, open_url, reports_dir};
