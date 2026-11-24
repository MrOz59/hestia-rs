//! Settings → Debug: hidden until About is tapped seven times, like
//! Android's developer options, and kept across restarts until hidden again.

use std::time::{Duration, Instant};

use hestia_core::{DebugOptions, logcapture};
use log::{info, warn};

use super::{Controller, Shared, spawn};
use crate::{platform, view};

/// Taps on About that unlock the section; the countdown shows from the
/// third, as on Android.
const UNLOCK_TAPS: u8 = 7;
const COUNTDOWN_FROM: u8 = 3;
/// A longer pause between taps starts the count again.
const TAP_GAP: Duration = Duration::from_secs(3);

/// The taps on About so far.
#[derive(Default)]
pub(super) struct Taps {
    count: u8,
    last: Option<Instant>,
}

impl Taps {
    /// Counts a tap at `now` and returns the count.
    fn tap(&mut self, now: Instant) -> u8 {
        if self.last.is_none_or(|last| now.duration_since(last) > TAP_GAP) {
            self.count = 0;
        }
        self.last = Some(now);
        self.count = (self.count + 1).min(UNLOCK_TAPS);
        self.count
    }
}

impl Controller {
    /// The saved options, at startup.
    pub(super) fn apply_debug_options(self: &Shared, options: DebugOptions) {
        self.state().debug = options;
        logcapture::set_verbose(options.verbose_log);
        if options.verbose_log {
            info!("verbose log on");
        }
        self.ui(move |ui| {
            ui.set_debug_options(options.unlocked);
            ui.set_debug_verbose_log(options.verbose_log);
        });
        if options.unlocked {
            self.refresh_debug_info();
        }
    }

    /// Settings → About tapped (or pressed with a remote).
    pub fn debug_tap(self: &Shared) {
        if self.core().is_none() {
            return;
        }
        if self.state().debug.unlocked {
            self.toast("Debug options are already on");
            return;
        }
        let taps = self.state().debug_taps.tap(Instant::now());
        if taps < UNLOCK_TAPS {
            if taps >= COUNTDOWN_FROM {
                let left = UNLOCK_TAPS - taps;
                self.toast(format!(
                    "{left} {} away from debug options",
                    if left == 1 { "tap" } else { "taps" }
                ));
            }
            return;
        }
        let options = DebugOptions {
            unlocked: true,
            ..self.state().debug
        };
        info!("debug options unlocked");
        self.save_debug_options(options);
        self.refresh_debug_info();
        self.ui(|ui| ui.set_debug_options(true));
        self.toast("Debug options are on: see the end of Settings");
    }

    /// Settings → Debug → Hide debug options: everything back off.
    pub fn hide_debug_options(self: &Shared) {
        self.state().debug_taps = Taps::default();
        logcapture::set_verbose(false);
        info!("debug options hidden");
        self.save_debug_options(DebugOptions::default());
        self.ui(|ui| {
            ui.set_debug_options(false);
            ui.set_debug_verbose_log(false);
        });
        self.toast("Debug options hidden");
    }

    /// Settings → Debug → Verbose log.
    pub fn set_verbose_log(self: &Shared, on: bool) {
        logcapture::set_verbose(on);
        info!("verbose log {}", if on { "on" } else { "off" });
        let options = DebugOptions {
            verbose_log: on,
            ..self.state().debug
        };
        self.save_debug_options(options);
        self.ui(move |ui| ui.set_debug_verbose_log(on));
    }

    /// Settings → Debug → This device: copies what the row shows.
    pub fn copy_debug_info(self: &Shared) {
        let c = self.clone();
        spawn("hestia-debug-copy", move || {
            let text = debug_info();
            match platform::set_clipboard_text(&text) {
                Ok(()) => c.toast("Device details copied"),
                Err(e) => c.toast(format!("Could not copy: {e}")),
            }
        });
    }

    fn refresh_debug_info(self: &Shared) {
        let c = self.clone();
        spawn("hestia-debug-info", move || {
            let text = debug_info();
            c.ui(move |ui| ui.set_debug_info(text));
        });
    }

    fn save_debug_options(self: &Shared, options: DebugOptions) {
        self.state().debug = options;
        let Some(core) = self.core() else { return };
        spawn("hestia-debug-save", move || {
            if let Err(e) = core.set_debug_options(options) {
                warn!("could not save the debug options: {e}");
            }
        });
    }
}

/// This build, device, display and (Android) video decoders, a line each.
/// Asks the platform, so it runs off the UI thread.
fn debug_info() -> String {
    let mut lines = vec![
        view::version_text(),
        format!("System: {} {}", std::env::consts::OS, std::env::consts::ARCH),
    ];
    if let Some(device) = platform::device_summary() {
        lines.push(format!("Device: {device}"));
    }
    lines.push(match platform::native_display() {
        Some((w, h, hz)) => format!("Display: {w}×{h} at {hz} Hz"),
        None => "Display: unknown".to_owned(),
    });
    #[cfg(target_os = "android")]
    {
        match platform::video_decoders(1920, 1080, 60) {
            Ok(decoders) if !decoders.is_empty() => {
                for (codec, name) in decoders {
                    lines.push(format!("Decoder: {codec:?} {name}"));
                }
            }
            Ok(_) => lines.push("Decoder: none for 1080p60".to_owned()),
            Err(e) => lines.push(format!("Decoder: {e}")),
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seven_quick_taps_unlock_and_a_pause_starts_over() {
        let start = Instant::now();
        let mut taps = Taps::default();
        let at = |ms| start + Duration::from_millis(ms);
        assert_eq!(taps.tap(at(0)), 1);
        assert_eq!(taps.tap(at(400)), 2);
        // Too long a pause: this tap is the first again.
        assert_eq!(taps.tap(at(400) + TAP_GAP + Duration::from_millis(1)), 1);
        let restart = 400 + TAP_GAP.as_millis() as u64 + 1;
        let counts: Vec<u8> = (1..=7).map(|i| taps.tap(at(restart + i * 300))).collect();
        assert_eq!(counts, [2, 3, 4, 5, 6, 7, 7]);
    }
}
