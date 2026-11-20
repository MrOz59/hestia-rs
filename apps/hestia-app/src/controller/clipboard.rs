//! Clipboard during a stream (Hermes hosts that offer it): kept in sync
//! both ways when the setting is on, and one-off send / fetch from the
//! stream menu.

use std::time::Duration;

use hestia_core::api::{ApiError, MAX_CLIPBOARD_BYTES};
use log::{debug, info, warn};

use super::{Controller, Shared, spawn};
use crate::clipsync::ClipSync;
use crate::platform;

/// How often both clipboards are checked.
const ROUND: Duration = Duration::from_millis(1500);

impl Controller {
    /// Keeps the clipboards in sync while stream `generation` runs.
    pub(super) fn start_clipboard_sync(self: &Shared, generation: u64, host_id: String) {
        let c = self.clone();
        spawn("hestia-clipboard-sync", move || {
            let Some(core) = c.core() else { return };
            let mut sync = ClipSync::new(MAX_CLIPBOARD_BYTES);
            let mut trouble_noted = false;
            let mut note = |what: String| {
                if !std::mem::replace(&mut trouble_noted, true) {
                    warn!("clipboard sync: {what} (will keep trying)");
                }
            };
            info!("clipboard sync on");
            while c.stream_is(generation) {
                // Local news first: the device's clipboard wins at the start.
                if sync.should_read_local(platform::clipboard_stamp())
                    && let Some(text) = sync.local(platform::clipboard_text())
                {
                    match core.with_api(&host_id, |api| api.set_clipboard(&text)) {
                        Ok(Ok(())) => {
                            debug!("clipboard → host ({} bytes)", text.len());
                            sync.sent(text);
                        }
                        Ok(Err(e)) if refused(&e) => return c.clipboard_refused(&e),
                        Ok(Err(e)) => {
                            note(e.to_string());
                            sync.send_failed();
                        }
                        Err(e) => {
                            note(e.to_string());
                            sync.send_failed();
                        }
                    }
                }
                match core.with_api(&host_id, |api| api.clipboard()) {
                    Ok(Ok(text)) => {
                        if let Some(text) = sync.host(text) {
                            if let Err(e) = platform::set_clipboard_text(&text) {
                                note(format!("can't write the local clipboard: {e}"));
                            }
                            debug!("clipboard ← host ({} bytes)", text.len());
                            // Taken either way: a local write that fails isn't retried.
                            sync.taken(text, platform::clipboard_stamp());
                        }
                    }
                    Ok(Err(e)) if refused(&e) => return c.clipboard_refused(&e),
                    Ok(Err(e)) => note(e.to_string()),
                    Err(e) => note(e.to_string()),
                }
                std::thread::sleep(ROUND);
            }
            info!("clipboard sync off");
        });
    }

    fn clipboard_refused(self: &Shared, e: &ApiError) {
        info!("clipboard sync stopped: {e}");
        self.toast(if e.is_unsupported() {
            "This host has no clipboard sync (it isn't Hermes)".to_owned()
        } else {
            "Clipboard sync isn't allowed for this device (Hermes → client permissions)".to_owned()
        });
    }

    /// Stream menu → "Send clipboard to host".
    pub(super) fn stream_clipboard_send(self: &Shared) {
        let Some(host_id) = self.stream_host() else { return };
        let c = self.clone();
        spawn("hestia-clipboard", move || {
            let Some(core) = c.core() else { return };
            let Some(text) = platform::clipboard_text().filter(|t| !t.is_empty()) else {
                return c.toast("Nothing to send: the clipboard is empty");
            };
            if text.len() > MAX_CLIPBOARD_BYTES {
                return c.toast(format!(
                    "Too long: the host takes up to {} KiB",
                    MAX_CLIPBOARD_BYTES / 1024
                ));
            }
            c.toast(match core.with_api(&host_id, |api| api.set_clipboard(&text)) {
                Ok(Ok(())) => "Clipboard sent: paste it on the host".to_owned(),
                Ok(Err(e)) => format!("Clipboard: {e}"),
                Err(e) => format!("Clipboard: {e}"),
            });
        });
    }

    /// Stream menu → "Get host's clipboard".
    pub(super) fn stream_clipboard_fetch(self: &Shared) {
        let Some(host_id) = self.stream_host() else { return };
        let c = self.clone();
        spawn("hestia-clipboard", move || {
            let Some(core) = c.core() else { return };
            c.toast(match core.with_api(&host_id, |api| api.clipboard()) {
                Ok(Ok(text)) if text.is_empty() => "The host's clipboard is empty".to_owned(),
                Ok(Ok(text)) => match platform::set_clipboard_text(&text) {
                    Ok(()) => "Copied the host's clipboard here".to_owned(),
                    Err(e) => format!("Clipboard: {e}"),
                },
                Ok(Err(e)) => format!("Clipboard: {e}"),
                Err(e) => format!("Clipboard: {e}"),
            });
        });
    }
}

fn refused(e: &ApiError) -> bool {
    e.is_permission_denied() || e.is_unsupported()
}
