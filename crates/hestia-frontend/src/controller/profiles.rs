//! Named stream profiles: saved from the settings page, loaded back into
//! it, deleted, and chosen per host on the host's page.

use log::warn;

use super::{Controller, Shared};
use crate::{platform, view};

impl Controller {
    /// Fills both profile lists: the settings page's, and the current
    /// host's choice ("Settings" first, then the profiles).
    pub(super) fn push_profiles(self: &Shared) {
        let Some(core) = self.core() else { return };
        let names: Vec<String> = core.profiles().into_iter().map(|p| p.name).collect();
        let chosen = self
            .state()
            .current_host
            .clone()
            .and_then(|id| core.host(&id).ok())
            .and_then(|h| h.profile);
        let index = chosen
            .and_then(|c| names.iter().position(|n| n.eq_ignore_ascii_case(&c)))
            .map_or(0, |i| i + 1);
        self.ui(move |ui| {
            let mut options = vec!["Settings (default)".to_owned()];
            options.extend(names.iter().cloned());
            if ui.cfg_profile_pick() >= names.len() as i32 {
                ui.set_cfg_profile_pick(0);
            }
            ui.set_cfg_profile_names(names);
            ui.set_apps_profile_options(options);
            ui.set_apps_profile_index(index as i32);
        });
    }

    /// Settings → "Save as profile": the page as it is now, named.
    pub fn save_profile(self: &Shared, name: &str) {
        let Some(core) = self.core() else { return };
        let name = name.to_owned();
        let c = self.clone();
        self.ui(move |ui| {
            let settings = view::read_settings_from(ui, &core.settings());
            match core.save_profile(&name, &settings) {
                Ok(name) => {
                    let index = core.profiles().iter().position(|p| p.name == name).unwrap_or_default();
                    c.push_profiles();
                    c.ui(move |ui| ui.set_cfg_profile_pick(index as i32));
                    c.toast(format!("Saved profile “{name}”: choose it on a host's page"));
                }
                Err(e) => c.toast(format!("Profile not saved: {e}")),
            }
        });
    }

    /// Settings → "Load": a profile into the page (Save keeps it).
    pub fn load_profile(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(profile) = core.profiles().into_iter().nth(index.max(0) as usize) else {
            return;
        };
        self.toast(format!("Loaded “{}”; Save to make it the settings", profile.name));
        self.ui(move |ui| {
            let settings = profile.apply(&view::read_settings_from(ui, &core.settings()));
            view::load_settings_into(ui, &settings, platform::native_display());
        });
    }

    /// Settings → "Delete".
    pub fn delete_profile(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(profile) = core.profiles().into_iter().nth(index.max(0) as usize) else {
            return;
        };
        match core.delete_profile(&profile.name) {
            Ok(()) => {
                self.push_profiles();
                self.toast(format!("Deleted “{}”", profile.name));
            }
            Err(e) => {
                warn!("deleting profile {} failed: {e}", profile.name);
                self.toast(format!("Could not delete “{}”: {e}", profile.name));
            }
        }
    }

    /// Host page → "Stream profile": what this host launches with.
    pub fn choose_host_profile(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let name = usize::try_from(index - 1)
            .ok()
            .and_then(|i| core.profiles().into_iter().nth(i))
            .map(|p| p.name);
        match core.set_host_profile(&id, name.as_deref()) {
            Ok(()) => self.toast(match name {
                Some(name) => format!("This host now launches with “{name}”"),
                None => "This host now launches with the settings".to_owned(),
            }),
            Err(e) => self.toast(format!("Could not save the choice: {e}")),
        }
    }
}
