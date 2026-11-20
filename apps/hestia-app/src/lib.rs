//! Hestia RS graphical client. The UI lives in `ui/*.slint`; this crate wires
//! it to `hestia-core` and runs every network call on worker threads.

mod clipsync;
mod controller;
#[cfg(target_os = "linux")]
mod desktop_pads;
mod gamepad;
#[cfg(target_os = "android")]
mod gpu_render;
mod hostinfo;
mod keymap;
mod netroute;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod padmap;
mod platform;
mod presentation;
mod touch;
mod typing;
mod view;
mod virtual_pad;

use std::path::PathBuf;

slint::include_modules!();

/// The on-screen controller as the stream view draws it: layout, what is
/// held, where the stick knobs are.
fn pad_controls(pad: &virtual_pad::VirtualPad) -> Vec<PadControl> {
    use hestia_core::stream::input::buttons;
    use virtual_pad::Kind;
    pad.controls()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (pressed, (knob_x, knob_y), dpad) = pad.view(i);
            PadControl {
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
pub fn pad_preview(width: f32, height: f32) -> Vec<PadControl> {
    let mut pad = virtual_pad::VirtualPad::default();
    pad.resize(width, height);
    pad_controls(&pad)
}

/// Builds the window and runs the event loop until it closes.
pub fn run(data_dir: PathBuf) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_is_android(cfg!(target_os = "android"));
    let controller = controller::Controller::new(&ui);
    controller.wire(&ui);
    // Desktop: game controllers straight from evdev.
    #[cfg(target_os = "linux")]
    desktop_pads::start();
    controller.start(data_dir);
    ui.run()
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    // logcat as before; the capture keeps lines for stream reports.
    let logcat = android_logger::AndroidLogger::new(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("hestia"),
    );
    let _ = hestia_core::logcapture::install(Box::new(logcat), log::LevelFilter::Info);
    platform::set_android_app(app.clone());
    // Reach LAN hosts over Wi-Fi even when Android prefers mobile data.
    hestia_core::nvhttp::http::set_route_hook(platform::route_for);
    // Game controllers reach the host through the (patched) backend's hook.
    i_slint_backend_android_activity::set_input_hook(platform::android_input_hook);
    let data_dir = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp/hestia-rs"));
    if let Err(e) = slint::android::init(app) {
        log::error!("failed to initialise Slint on Android: {e}");
        return;
    }
    if let Err(e) = run(data_dir) {
        log::error!("Hestia exited with an error: {e}");
    }
}
