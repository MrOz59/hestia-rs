//! Session reports: started when an app is launched, filled while streaming
//! (negotiation, events, per-second stats), saved with every captured log
//! line when the session ends or fails, then shown on the report screen.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use hestia_core::api::MAX_CLIPBOARD_BYTES;
use hestia_core::report::{self, StreamReport};
use hestia_core::{DeviceInfo, LaunchedSession, StreamSettings, logcapture};
use log::warn;
use slint::{ModelRc, SharedString, VecModel};

use super::{Controller, Shared, spawn};
use crate::{platform, view};

pub(super) type SharedReport = Arc<Mutex<StreamReport>>;

/// What the report screen shows and exports.
pub(super) struct LastReport {
    title: String,
    text: String,
    path: Option<PathBuf>,
    /// Host to send the report to (Hermes clipboard), if any.
    host_id: Option<String>,
}

/// Android share sheets carry the text through Binder; stay well under its
/// ~1 MB transaction limit.
const SHARE_MAX_BYTES: usize = 400 * 1024;
const LOG_TAIL_LINES: usize = 40;

pub(super) fn lock(report: &SharedReport) -> MutexGuard<'_, StreamReport> {
    report.lock().unwrap_or_else(|p| p.into_inner())
}

/// Starts a report for launching `title` on a host (settings and host
/// details are added once `/launch` answers, see [`launched`]).
pub(super) fn begin(title: &str, host_name: &str, device: &DeviceInfo) -> SharedReport {
    let mut r = StreamReport::new(format!("{title} on {host_name}"), logcapture::mark());
    r.field("Client", "version", view::version_text());
    r.field(
        "Client",
        "platform",
        format!(
            "{} {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH,
            device.platform
        ),
    );
    r.field(
        "Client",
        "window",
        format!(
            "{}x{} @ {} Hz",
            device.display_width, device.display_height, device.refresh_rate
        ),
    );
    let decoders: Vec<&str> = hestia_media::video_backends()
        .into_iter()
        .filter(|b| b.available)
        .map(|b| b.name)
        .collect();
    r.field("Client", "video decoders", decoders.join(", "));
    r.field(
        "Client",
        "audio output",
        if cfg!(target_os = "android") {
            "none yet on Android"
        } else {
            "Opus via cpal (stereo)"
        },
    );
    r.event(format!("launching {title}"));
    Arc::new(Mutex::new(r))
}

/// Adds settings, host details and what `/launch` returned.
pub(super) fn launched(report: &SharedReport, l: &LaunchedSession) {
    l.add_to_report(&mut lock(report));
}

/// For failed launches: the settings that were used.
pub(super) fn settings(report: &SharedReport, settings: &StreamSettings) {
    settings.add_to_report(&mut lock(report));
}

/// The "Summary" section of a saved report, for reports read back from disk.
fn summary_from_text(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| !l.starts_with("== Summary"))
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())
        .collect()
}

impl Controller {
    fn last_report(&self) -> MutexGuard<'_, Option<LastReport>> {
        self.last_report.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Ends the report, saves it and remembers it for the report screen.
    /// Returns whether the report screen should be shown (the setting).
    /// Runs on a worker thread.
    pub(super) fn finish_report(self: &Shared, report: SharedReport, outcome: &str, host_id: Option<String>) -> bool {
        let core = self.core();
        let (title, text, summary, path) = {
            let mut r = lock(&report);
            r.finish(outcome);
            let text = r.render();
            let path = core.as_ref().and_then(|core| {
                let dir = platform::reports_dir(core.data_dir());
                match r.save_text(&dir, &text) {
                    Ok(path) => {
                        log::info!("stream report saved to {}", path.display());
                        Some(path)
                    }
                    Err(e) => {
                        warn!("could not save the stream report: {e}");
                        None
                    }
                }
            });
            (r.title().to_owned(), text, r.summary_lines(), path)
        };
        let hermes = host_id
            .as_ref()
            .is_some_and(|id| self.state().snapshots.get(id).is_some_and(|s| s.capabilities.is_some()));
        let last = LastReport {
            title,
            text,
            path,
            host_id: host_id.filter(|_| hermes),
        };
        self.push_report(&last, Some(summary));
        *self.last_report() = Some(last);
        core.is_some_and(|core| core.settings().show_stream_report)
    }

    fn push_report(self: &Shared, last: &LastReport, summary: Option<Vec<String>>) {
        let summary = summary.unwrap_or_else(|| summary_from_text(&last.text));
        let tail: Vec<String> = {
            let lines: Vec<&str> = last.text.lines().collect();
            let start = lines.len().saturating_sub(LOG_TAIL_LINES);
            lines[start..].iter().map(|l| (*l).to_owned()).collect()
        };
        let title = last.title.clone();
        let path = last.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        let can_send = last.host_id.is_some();
        self.ui(move |ui| {
            ui.set_report_title(title.into());
            ui.set_report_path(path.into());
            ui.set_report_can_send(can_send);
            ui.set_report_export_label(platform::export_label().into());
            let summary: Vec<SharedString> = summary.into_iter().map(SharedString::from).collect();
            ui.set_report_summary(ModelRc::new(VecModel::from(summary)));
            let tail: Vec<SharedString> = tail.into_iter().map(SharedString::from).collect();
            ui.set_report_log_tail(ModelRc::new(VecModel::from(tail)));
        });
    }

    /// Settings → "Open last stream report": the one from this run, or the
    /// newest file on disk.
    pub(super) fn open_last_report(self: &Shared) {
        let has_current = self.last_report().is_some();
        if !has_current {
            let Some(core) = self.core() else { return };
            let dir = platform::reports_dir(core.data_dir());
            let Some(path) = report::latest(&dir) else {
                self.toast("No stream report yet. One is made each time a stream ends.");
                return;
            };
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let title = text
                        .lines()
                        .next()
                        .and_then(|l| l.split_once(" — "))
                        .map_or_else(|| "Last stream".to_owned(), |(_, t)| t.to_owned());
                    *self.last_report() = Some(LastReport {
                        title,
                        text,
                        path: Some(path),
                        host_id: None,
                    });
                }
                Err(e) => {
                    self.toast(format!("Could not read {}: {e}", path.display()));
                    return;
                }
            }
        }
        if let Some(last) = self.last_report().as_ref() {
            self.push_report(last, None);
        }
        self.ui(|ui| ui.set_screen(6));
    }

    pub(super) fn report_export(self: &Shared) {
        let (subject, text, path) = {
            let guard = self.last_report();
            let Some(last) = guard.as_ref() else { return };
            (
                format!("Hestia RS stream report — {}", last.title),
                report::clip_middle(&last.text, SHARE_MAX_BYTES),
                last.path.clone(),
            )
        };
        if let Err(e) = platform::export_report(&subject, &text, path.as_deref()) {
            self.toast(e);
        }
    }

    pub(super) fn report_send_to_host(self: &Shared) {
        let (host_id, text) = {
            let guard = self.last_report();
            let Some(last) = guard.as_ref() else { return };
            let Some(host_id) = last.host_id.clone() else { return };
            (host_id, report::clip_middle(&last.text, MAX_CLIPBOARD_BYTES - 1024))
        };
        self.ui(|ui| ui.set_report_busy(true));
        let c = self.clone();
        spawn("hestia-report-send", move || {
            let Some(core) = c.core() else { return };
            let message = match core.with_api(&host_id, |api| api.set_clipboard(&text)) {
                Ok(Ok(())) => "Report copied to the host's clipboard: paste it on the PC".to_owned(),
                Ok(Err(e)) => format!("Could not send the report: {e}"),
                Err(e) => format!("Could not send the report: {e}"),
            };
            c.ui(|ui| ui.set_report_busy(false));
            c.toast(message);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_summary_back() {
        let mut r = StreamReport::new("Desktop on cachy", 0);
        r.finish("disconnected");
        let text = r.render_with_logs(&[]);
        let summary = summary_from_text(&text);
        assert_eq!(summary[0], "Result: disconnected");
        assert!(summary.iter().any(|l| l.starts_with("Duration:")));
    }
}
