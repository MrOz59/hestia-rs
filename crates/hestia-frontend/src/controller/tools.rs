//! Hermes host tools screen: display recovery, server commands, clipboard,
//! permissions and diagnostics, all through the Hestia API.

use std::time::Duration;

use hestia_core::api::{ApiError, HostDiagnostics, MAX_CLIPBOARD_BYTES, ServerCommand};

use super::{Controller, Shared, spawn};
use crate::ui::{CommandRow, PreflightRow, Screen};
use crate::{hostinfo, platform};

/// How long an armed "Confirm" button waits for the second tap.
const CONFIRM_WINDOW: Duration = Duration::from_secs(4);

fn api_error(what: &str, e: &ApiError) -> String {
    if e.is_permission_denied() {
        format!("{what}: not allowed for this device")
    } else if e.is_unsupported() {
        format!("{what}: not supported by this host")
    } else {
        format!("{what}: {e}")
    }
}

impl Controller {
    fn push_commands(self: &Shared) {
        let (commands, armed) = {
            let st = self.state();
            (st.tools_commands.clone(), st.tools_armed)
        };
        self.ui(move |ui| {
            let rows: Vec<CommandRow> = commands
                .iter()
                .map(|c| CommandRow {
                    id: c.id as i32,
                    name: c.name.clone(),
                    confirm: c.requires_confirmation,
                    armed: armed == Some(c.id),
                })
                .collect();
            ui.set_tools_commands(rows);
        });
    }

    /// The open host's Hermes tools page.
    pub fn open_tools(self: &Shared) {
        self.ui(|ui| {
            ui.set_tools_message(String::new());
            ui.set_tools_display_status(String::new());
            ui.set_tools_permissions(String::new());
            ui.set_tools_diagnostics(Vec::new());
            ui.set_tools_commands(Vec::new());
            ui.set_tools_status_lines(Vec::new());
            ui.set_tools_preflight(Vec::new());
            ui.set_screen(Screen::Tools);
        });
        self.tools_refresh();
    }

    pub fn tools_refresh(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        self.ui(|ui| ui.set_tools_loading(true));
        let c = self.clone();
        spawn("hestia-tools", move || {
            let Some(core) = c.core() else { return };
            let result = core.with_api(&id, |api| {
                (
                    api.client_permissions(),
                    api.display_status(),
                    api.commands(),
                    api.diagnostics(),
                )
            });
            let (perms, display, commands, diagnostics) = match result {
                Ok(parts) => parts,
                Err(e) => {
                    let text = format!("Could not reach the host: {e}");
                    c.ui(move |ui| {
                        ui.set_tools_loading(false);
                        ui.set_tools_message(text.into());
                    });
                    return;
                }
            };

            let unsupported = perms.as_ref().err().is_some_and(ApiError::is_unsupported);
            let mut notes = Vec::new();
            if unsupported {
                notes.push(
                    "This host doesn't offer the Hermes tools API: it isn't a Hermes host, or its Hermes is too old."
                        .to_owned(),
                );
            }
            let (perm_text, display_ok, commands_ok, clipboard_ok) = match &perms {
                Ok(p) => (
                    hostinfo::permission_summary(&p.permissions),
                    p.permissions.display_recovery,
                    p.permissions.server_commands,
                    p.permissions.clipboard_sync,
                ),
                Err(e) => (api_error("Permissions", e), !unsupported, !unsupported, !unsupported),
            };
            let display_text = match &display {
                Ok(v) => hostinfo::json_summary(v, 6),
                Err(e) => api_error("Display status", e),
            };
            let command_list = match commands {
                Ok(list) => list,
                Err(e) => {
                    if !unsupported && !e.is_permission_denied() {
                        notes.push(api_error("Server commands", &e));
                    }
                    Vec::new()
                }
            };
            let diag_lines = match &diagnostics {
                Ok(v) => hostinfo::json_lines(v, 40),
                Err(e) if !unsupported => vec![api_error("Diagnostics", e)],
                Err(_) => Vec::new(),
            };
            let host_diag = diagnostics.as_ref().ok().map(HostDiagnostics::from_value);
            let status = host_diag.as_ref().map(hostinfo::status_lines).unwrap_or_default();
            let preflight = host_diag
                .as_ref()
                .and_then(|d| d.preflight.as_ref())
                .map(hostinfo::preflight_rows)
                .unwrap_or_default();

            {
                let mut st = c.state();
                st.tools_commands = command_list;
                st.tools_armed = None;
            }
            c.push_commands();
            let message = notes.join("\n");
            c.ui(move |ui| {
                ui.set_tools_loading(false);
                ui.set_tools_message(message.into());
                ui.set_tools_permissions(perm_text.into());
                ui.set_tools_display_status(display_text.into());
                ui.set_tools_display_allowed(display_ok);
                ui.set_tools_commands_allowed(commands_ok);
                ui.set_tools_clipboard_allowed(clipboard_ok);
                ui.set_tools_diagnostics(diag_lines);
                ui.set_tools_status_lines(status);
                let rows: Vec<PreflightRow> = preflight
                    .into_iter()
                    .map(|(level, title, message)| PreflightRow {
                        level,
                        title: title.into(),
                        message: message.into(),
                    })
                    .collect();
                ui.set_tools_preflight(rows);
            });
        });
    }

    /// Runs `f` against the current host's API on a worker thread, with the
    /// tools screen marked busy; the returned text is shown as a toast.
    fn tools_action<F>(self: &Shared, name: &str, f: F)
    where
        F: FnOnce(&hestia_core::api::HestiaApi<'_>) -> Result<String, ApiError> + Send + 'static,
    {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        self.ui(|ui| ui.set_tools_busy(true));
        let c = self.clone();
        let name = name.to_owned();
        spawn("hestia-tool", move || {
            let Some(core) = c.core() else { return };
            let text = match core.with_api(&id, f) {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => api_error(&name, &e),
                Err(e) => format!("{name}: {e}"),
            };
            c.ui(|ui| ui.set_tools_busy(false));
            c.toast(text);
        });
    }

    pub fn tools_recover_display(self: &Shared) {
        self.tools_action("Recover display", |api| {
            api.recover_display(false)
                .map(|v| format!("Display recovery: {}", hostinfo::json_summary(&v, 3)))
        });
    }

    pub fn tools_run_command(self: &Shared, id: i32) {
        let id = id as u32;
        let command: Option<ServerCommand> = self.state().tools_commands.iter().find(|c| c.id == id).cloned();
        let Some(command) = command else { return };
        let armed = self.state().tools_armed == Some(id);
        if command.requires_confirmation && !armed {
            self.state().tools_armed = Some(id);
            self.push_commands();
            // Disarm if the second tap doesn't come.
            let c = self.clone();
            self.ui_after(CONFIRM_WINDOW, move |_| {
                let expired = {
                    let mut st = c.state();
                    let expired = st.tools_armed == Some(id);
                    if expired {
                        st.tools_armed = None;
                    }
                    expired
                };
                if expired {
                    c.push_commands();
                }
            });
            return;
        }
        self.state().tools_armed = None;
        self.push_commands();
        let name = command.name.clone();
        self.tools_action("Run command", move |api| {
            api.run_command(id).map(|()| format!("Ran “{name}”"))
        });
    }

    pub fn tools_send_clipboard(self: &Shared, text: String) {
        if text.len() > MAX_CLIPBOARD_BYTES {
            self.toast(format!(
                "Too long: the host accepts up to {} KiB",
                MAX_CLIPBOARD_BYTES / 1024
            ));
            return;
        }
        self.tools_action("Send clipboard", move |api| {
            api.set_clipboard(&text)
                .map(|()| "Sent to the host's clipboard".to_owned())
        });
    }

    pub fn tools_fetch_clipboard(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        self.ui(|ui| ui.set_tools_busy(true));
        let c = self.clone();
        spawn("hestia-clipboard", move || {
            let Some(core) = c.core() else { return };
            let result = core.with_api(&id, |api| api.clipboard());
            c.ui(|ui| ui.set_tools_busy(false));
            match result {
                Ok(Ok(text)) if text.is_empty() => c.toast("The host's clipboard is empty"),
                Ok(Ok(text)) => c.ui(move |ui| ui.set_tools_clipboard_text(text.into())),
                Ok(Err(e)) => c.toast(api_error("Clipboard", &e)),
                Err(e) => c.toast(format!("Clipboard: {e}")),
            }
        });
    }

    pub fn tools_open_web_ui(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let Some(core) = self.core() else { return };
        let result = core
            .web_ui_url(&id)
            .map_err(|e| e.to_string())
            .and_then(|url| platform::open_url(&url).map(|()| url));
        match result {
            Ok(url) => self.toast(format!("Opening {url} (the host uses a self-signed certificate)")),
            Err(e) => self.toast(e),
        }
    }

    /// Fetches Hermes' full diagnostic report, saves it next to the stream
    /// reports and opens it on the report screen (to share or keep).
    pub fn tools_host_report(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        self.ui(|ui| ui.set_tools_busy(true));
        let c = self.clone();
        spawn("hestia-host-report", move || {
            let Some(core) = c.core() else { return };
            let result = core.with_api(&id, |api| api.diagnostics_report());
            c.ui(|ui| ui.set_tools_busy(false));
            let text = match result {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => return c.toast(api_error("Diagnostic report", &e)),
                Err(e) => return c.toast(format!("Diagnostic report: {e}")),
            };
            let host = core.host(&id).map(|h| h.name).unwrap_or_default();
            let now = std::time::SystemTime::now();
            let dir = platform::reports_dir(core.data_dir());
            let path = match hestia_core::report::save_named(&dir, "hermes", now, &text) {
                Ok(path) => Some(path),
                Err(e) => {
                    log::warn!("could not save the Hermes report: {e}");
                    None
                }
            };
            let summary = vec![
                format!("Hermes diagnostic report from {host}"),
                format!("Fetched {}", hestia_core::report::utc(now)),
                format!("{} lines, {} KiB", text.lines().count(), text.len().div_ceil(1024)),
            ];
            c.show_text_report(format!("Hermes report — {host}"), text, path, summary);
        });
    }
}
