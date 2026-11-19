//! Hestia RS graphical client. The UI lives in `ui/*.slint`; this crate wires
//! it to `hestia-core` and runs every network call on worker threads.

mod controller;
mod hostinfo;
mod keymap;
mod touch;
mod typing;
mod view;

use std::path::PathBuf;

slint::include_modules!();

/// Builds the window and runs the event loop until it closes.
pub fn run(data_dir: PathBuf) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let controller = controller::Controller::new(&ui);
    controller.wire(&ui);
    controller.start(data_dir);
    ui.run()
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("hestia"),
    );
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
