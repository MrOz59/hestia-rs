//! Hermes host tools screen: display recovery, server commands, clipboard,
//! permissions and diagnostics, all through the Hestia API.

use std::time::Duration;

use hestia_core::api::{ApiError, MAX_CLIPBOARD_BYTES, ServerCommand};
use slint::{ModelRc, SharedString, VecModel};

use super::{Controller, Shared, spawn};
use crate::{CommandRow, hostinfo};

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
                    name: c.name.as_str().into(),
                    confirm: c.requires_confirmation,
                    armed: armed == Some(c.id),
                })
                .collect();
            ui.set_tools_commands(ModelRc::new(VecModel::from(rows)));
        });
    }

    pub(super) fn open_tools(self: &Shared) {
        self.ui(|ui| {
            ui.set_tools_message(SharedString::new());
            ui.set_tools_display_status(SharedString::new());
            ui.set_tools_permissions(SharedString::new());
            ui.set_tools_diagnostics(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
            ui.set_tools_commands(ModelRc::new(VecModel::from(Vec::<CommandRow>::new())));
            ui.set_screen(5);
        });
        self.tools_refresh();
    }

    pub(super) fn tools_refresh(self: &Shared) {
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
                notes.push("This host doesn't offer the Hermes tools API (plain Sunshine or Apollo).".to_owned());
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
                let lines: Vec<SharedString> = diag_lines.into_iter().map(SharedString::from).collect();
                ui.set_tools_diagnostics(ModelRc::new(VecModel::from(lines)));
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

    pub(super) fn tools_recover_display(self: &Shared) {
        self.tools_action("Recover display", |api| {
            api.recover_display(false)
                .map(|v| format!("Display recovery: {}", hostinfo::json_summary(&v, 3)))
        });
    }

    pub(super) fn tools_run_command(self: &Shared, id: i32) {
        let id = id as u32;
        let command: Option<ServerCommand> = self.state().tools_commands.iter().find(|c| c.id == id).cloned();
        let Some(command) = command else { return };
        let armed = self.state().tools_armed == Some(id);
        if command.requires_confirmation && !armed {
            self.state().tools_armed = Some(id);
            self.push_commands();
            // Disarm if the second tap doesn't come.
            let c = self.clone();
            slint::Timer::single_shot(CONFIRM_WINDOW, move || {
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

    pub(super) fn tools_send_clipboard(self: &Shared, text: String) {
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

    pub(super) fn tools_fetch_clipboard(self: &Shared) {
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
}
