//! Stream profiles are local to the selected host. The app's global settings
//! provide device preferences and defaults, not a shared profile namespace.

use log::warn;

use super::{Controller, Shared};
use crate::{platform, view};

impl Controller {
    /// Fills the profile selector and editor for the current host only.
    pub(super) fn push_profiles(self: &Shared) {
        let Some(core) = self.core() else { return };
        let id = self.state().current_host.clone();
        let names: Vec<String> = id
            .as_deref()
            .and_then(|id| core.profiles(id).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.name)
            .collect();
        let chosen = id.and_then(|id| core.host(&id).ok()).and_then(|h| h.profile);
        let index = chosen
            .and_then(|name| names.iter().position(|n| n.eq_ignore_ascii_case(&name)))
            .map_or(0, |i| i + 1);
        self.ui(move |ui| {
            let mut options = vec!["Host defaults".to_owned()];
            options.extend(names.iter().cloned());
            if ui.cfg_profile_pick() >= names.len() as i32 {
                ui.set_cfg_profile_pick(0);
            }
            ui.set_cfg_profile_names(names);
            ui.set_apps_profile_options(options);
            ui.set_apps_profile_index(index as i32);
        });
    }

    /// Save a profile only for the host whose settings page is open.
    pub fn save_profile(self: &Shared, name: &str) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            self.toast("Choose a host before creating a stream profile");
            return;
        };
        let name = name.to_owned();
        let c = self.clone();
        self.ui(move |ui| {
            let (base, _) = core.settings_for_host(&id);
            let settings = view::read_settings_from(ui, &base);
            match core.save_profile(&id, &name, &settings) {
                Ok(saved) => {
                    if let Err(e) = core.set_host_profile(&id, Some(&saved)) {
                        c.toast(format!("Saved profile, but selection failed: {e}"));
                        return;
                    }
                    let index = core
                        .profiles(&id)
                        .unwrap_or_default()
                        .iter()
                        .position(|p| p.name == saved)
                        .unwrap_or_default();
                    c.push_profiles();
                    c.ui(move |ui| ui.set_cfg_profile_pick(index as i32));
                    c.toast(format!("Saved profile “{saved}” for this host"));
                }
                Err(e) => c.toast(format!("Profile not saved: {e}")),
            }
        });
    }

    /// Load this host's profile into the editor without modifying its selection.
    pub fn load_profile(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let Some(profile) = core
            .profiles(&id)
            .unwrap_or_default()
            .into_iter()
            .nth(index.max(0) as usize)
        else {
            return;
        };
        if let Err(e) = core.set_host_profile(&id, Some(&profile.name)) {
            self.toast(format!("Could not select profile: {e}"));
            return;
        }
        self.push_profiles();
        self.toast(format!("Loaded “{}” for editing", profile.name));
        self.ui(move |ui| {
            let (base, _) = core.settings_for_host(&id);
            let settings = profile.apply(&view::read_settings_from(ui, &base));
            view::load_settings_into(ui, &settings, platform::native_display());
        });
    }

    /// Deleting a profile never affects another host.
    pub fn delete_profile(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let Some(profile) = core
            .profiles(&id)
            .unwrap_or_default()
            .into_iter()
            .nth(index.max(0) as usize)
        else {
            return;
        };
        match core.delete_profile(&id, &profile.name) {
            Ok(()) => {
                self.push_profiles();
                self.toast(format!("Deleted “{}” from this host", profile.name));
            }
            Err(e) => {
                warn!("deleting profile {} failed: {e}", profile.name);
                self.toast(format!("Could not delete “{}”: {e}", profile.name));
            }
        }
    }

    /// Selects a profile from this host's own profile list.
    pub fn choose_host_profile(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let name = usize::try_from(index - 1)
            .ok()
            .and_then(|i| core.profiles(&id).ok()?.into_iter().nth(i))
            .map(|p| p.name);
        match core.set_host_profile(&id, name.as_deref()) {
            Ok(()) => {
                self.push_profiles();
                self.toast(match name {
                    Some(name) => format!("This host now launches with “{name}”"),
                    None => "This host now uses its own defaults".to_owned(),
                });
            }
            Err(e) => self.toast(format!("Could not save the choice: {e}")),
        }
    }
}
