//! Hestia RS's graphical client without a UI toolkit.
//!
//! Everything the app does except drawing lives here: the controller (hosts,
//! pairing, apps, launching, streaming, host tools, settings, reports), the
//! stream's input (touch gestures, on-screen and physical controllers,
//! keyboard mapping), system services (`platform`), and video presentation
//! (`presentation`, plus the toolkit-free GPU colour pass `gl_yuv`).
//!
//! A UI shell — today Slint, in `apps/hestia-app` — implements the port in
//! [`ui`] ([`ui::View`] and [`ui::UiHandle`]), forwards the user's actions to
//! [`controller::Controller`]'s public methods and draws the stream's
//! pictures. Swapping toolkits means writing another shell; nothing in this
//! crate changes. See `docs/FRONTENDS.md`.

mod clipsync;
pub mod controller;
#[cfg(target_os = "linux")]
pub mod desktop_pads;
mod gamepad;
#[cfg(not(target_os = "android"))]
pub mod gl_yuv;
mod hostinfo;
mod keymap;
mod netroute;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod padmap;
pub mod platform;
pub mod presentation;
mod stream_input;
mod touch;
mod typing;
pub mod ui;
mod video_backend;
mod view;
mod virtual_pad;

pub use controller::Controller;
pub use view::version_text;

/// The on-screen controller as the stream view draws it: layout, what is
/// held, where the stick knobs are.
fn pad_controls(pad: &virtual_pad::VirtualPad) -> Vec<ui::PadControl> {
    use hestia_core::stream::input::buttons;
    use virtual_pad::Kind;
    pad.controls()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (pressed, (knob_x, knob_y), dpad) = pad.view(i);
            ui::PadControl {
                kind: match c.kind {
                    Kind::Round(_) => 0,
                    Kind::Rect(_) | Kind::Trigger(_) => 1,
                    Kind::Stick(_) => 2,
                    Kind::Dpad => 3,
                },
                label: c.label.into(),
                x: c.x,
                y: c.y,
                w: c.w,
                h: c.h,
                pressed,
                knob_x,
                knob_y,
                up: dpad & buttons::UP != 0,
                down: dpad & buttons::DOWN != 0,
                left: dpad & buttons::LEFT != 0,
                right: dpad & buttons::RIGHT != 0,
            }
        })
        .collect()
}

/// The on-screen controller, untouched, for a `width`×`height` stream view
/// (the screenshot example draws it).
#[doc(hidden)]
pub fn pad_preview(width: f32, height: f32) -> Vec<ui::PadControl> {
    let mut pad = virtual_pad::VirtualPad::default();
    pad.resize(width, height);
    pad_controls(&pad)
}
