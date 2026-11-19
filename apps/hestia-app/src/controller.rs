//! Glue between the Slint window and `hestia-core`.
//!
//! Rules: callbacks run on the UI thread and must never block, so every core
//! call goes to a worker thread; results come back through
//! `upgrade_in_event_loop`. Shared state sits behind one mutex that is never
//! held across network calls.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use hestia_core::nvhttp::{AppInfo, PairingOutcome, PairingPin};
use hestia_core::{DeviceInfo, HestiaClient, HostSnapshot, Preset, presets};
use log::{info, warn};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::AppWindow;
use crate::{platform, view};

mod report;
mod streaming;
mod tools;

#[derive(Default)]
struct State {
    snapshots: HashMap<String, HostSnapshot>,
    checking: HashSet<String>,
    current_host: Option<String>,
    apps: Vec<AppInfo>,
    /// Host being paired and a generation counter that invalidates results
    /// of abandoned attempts.
    pairing_host: Option<String>,
    pair_generation: u64,
    /// Hermes server commands on the tools screen, and the one waiting for
    /// its confirmation tap.
    tools_commands: Vec<hestia_core::api::ServerCommand>,
    tools_armed: Option<u32>,
    /// The launch in progress (sequence number, host id); cancelling clears
    /// it so the launch thread knows to back out.
    launching: Option<(u64, String)>,
    launch_seq: u64,
}

pub struct Controller {
    weak: Mutex<slint::Weak<AppWindow>>,
    core: OnceLock<Arc<HestiaClient>>,
    state: Mutex<State>,
    stream: Mutex<Option<streaming::ActiveStream>>,
    stream_generation: AtomicU64,
    /// What a finger does in the stream (`TouchMode` index; persisted).
    touch_mode: AtomicU8,
    /// The report screen's content.
    last_report: Mutex<Option<report::LastReport>>,
}

type Shared = Arc<Controller>;

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(f) {
        warn!("failed to spawn {name}: {e}");
    }
}

impl Controller {
    pub fn new(ui: &AppWindow) -> Shared {
        Arc::new(Self {
            weak: Mutex::new(ui.as_weak()),
            core: OnceLock::new(),
            state: Mutex::new(State::default()),
            stream: Mutex::new(None),
            stream_generation: AtomicU64::new(0),
            touch_mode: AtomicU8::new(0),
            last_report: Mutex::new(None),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn core(&self) -> Option<Arc<HestiaClient>> {
        self.core.get().cloned()
    }

    /// Runs `f` on the UI thread.
    fn ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let _ = weak.upgrade_in_event_loop(f);
    }

    fn toast(self: &Shared, message: impl Into<String>) {
        let message: SharedString = message.into().into();
        self.ui(move |ui| {
            ui.set_toast(message.clone());
            let weak = ui.as_weak();
            slint::Timer::single_shot(Duration::from_secs(4), move || {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_toast() == message {
                        ui.set_toast(SharedString::new());
                    }
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // Wiring
    // ------------------------------------------------------------------

    pub fn wire(self: &Shared, ui: &AppWindow) {
        let c = self.clone();
        ui.on_refresh_hosts(move || c.refresh_all(true));
        let c = self.clone();
        ui.on_open_host(move |id| c.open_host(id.to_string()));
        let c = self.clone();
        ui.on_add_host(move |address| c.add_host(address.to_string()));

        let c = self.clone();
        ui.on_pair_retry(move || c.restart_pairing());
        let c = self.clone();
        ui.on_pair_use_otp(move || c.switch_to_otp());
        let c = self.clone();
        ui.on_pair_submit_otp(move |pin, pass| c.submit_otp(pin.to_string(), pass.to_string()));
        let c = self.clone();
        ui.on_pair_cancel(move || c.cancel_pairing());

        let c = self.clone();
        ui.on_launch_app(move |app_id| c.launch(app_id.max(0) as u32));
        let c = self.clone();
        ui.on_quit_app(move || c.quit_app());
        let c = self.clone();
        ui.on_forget_host(move || c.forget_current());
        let c = self.clone();
        ui.on_back_to_hosts(move || {
            c.state().current_host = None;
            c.ui(|ui| ui.set_screen(0));
            c.push_hosts();
        });
        let c = self.clone();
        ui.on_back_to_apps(move || {
            // While starting, "back" means cancel.
            if c.cancel_launch() {
                return;
            }
            // The report screen can be opened from Settings with no host open.
            let screen = if c.state().current_host.is_some() { 1 } else { 0 };
            c.ui(move |ui| ui.set_screen(screen));
            c.refresh_current_running();
        });
        let c = self.clone();
        ui.on_report_export(move || c.report_export());
        let c = self.clone();
        ui.on_report_send_to_host(move || c.report_send_to_host());
        let c = self.clone();
        ui.on_open_last_report(move || c.open_last_report());
        let c = self.clone();
        ui.on_open_app_log(move || c.open_app_log());

        let c = self.clone();
        ui.on_stream_key(move |text, pressed, repeat, ctrl, alt, shift, meta| {
            let modifiers = hestia_core::stream::input::Modifiers { shift, ctrl, alt, meta };
            c.stream_key(&text, pressed, repeat, modifiers);
        });
        let c = self.clone();
        ui.on_stream_pointer(move |kind, button, x, y, w, h, finger, sx, sy| {
            c.stream_pointer(kind, button, (x, y), (w, h), finger, (sx, sy))
        });
        let c = self.clone();
        ui.on_stream_toggle_pad(move || c.toggle_pad());
        let c = self.clone();
        ui.on_stream_toggle_zoom(move || c.toggle_zoom());
        let c = self.clone();
        ui.on_stream_view_size(move |w, h| c.stream_view_size(w, h));
        let c = self.clone();
        ui.on_settings_custom_resolution(move |text| c.custom_resolution(&text));
        let c = self.clone();
        ui.on_stream_scroll(move |dx, dy| c.stream_scroll(dx, dy));
        ui.on_stream_keyboard(platform::set_keyboard_visible);
        ui.set_stream_soft_keyboard(platform::has_soft_keyboard());
        let c = self.clone();
        platform::set_ime_sink(move |event| c.stream_ime(event));
        let c = self.clone();
        platform::set_gamepad_sink(move |state| c.stream_gamepad(state));
        let c = self.clone();
        ui.on_stream_special_key(move |name| c.stream_special_key(&name));
        let c = self.clone();
        ui.on_stream_cycle_touch_mode(move || c.cycle_touch_mode());
        let c = self.clone();
        ui.on_stream_request_keyframe(move || c.request_keyframe());
        let c = self.clone();
        ui.on_stream_toggle_fullscreen(move || c.toggle_fullscreen());
        let c = self.clone();
        ui.on_stream_disconnect(move || c.end_stream(None, None, false));
        let c = self.clone();
        ui.on_stream_quit(move || c.end_stream(None, None, true));

        let c = self.clone();
        ui.on_open_tools(move || c.open_tools());
        let c = self.clone();
        ui.on_tools_refresh(move || c.tools_refresh());
        let c = self.clone();
        ui.on_tools_recover_display(move || c.tools_recover_display());
        let c = self.clone();
        ui.on_tools_run_command(move |id| c.tools_run_command(id));
        let c = self.clone();
        ui.on_tools_send_clipboard(move |text| c.tools_send_clipboard(text.to_string()));
        let c = self.clone();
        ui.on_tools_fetch_clipboard(move || c.tools_fetch_clipboard());
        let c = self.clone();
        ui.on_tools_open_web_ui(move || c.tools_open_web_ui());
        let c = self.clone();
        ui.on_tools_host_report(move || c.tools_host_report());

        let c = self.clone();
        ui.on_open_settings(move || {
            if let Some(core) = c.core() {
                let settings = core.settings();
                c.ui(move |ui| view::load_settings_into(&ui, &settings, platform::native_display()));
            }
        });
        let c = self.clone();
        ui.on_save_settings(move || c.save_settings());
        let c = self.clone();
        ui.on_settings_preset_changed(move |index| c.apply_preset(index));
        let c = self.clone();
        ui.on_stream_cycle_scale(move || c.cycle_video_scale());
    }

    // ------------------------------------------------------------------
    // Startup
    // ------------------------------------------------------------------

    pub fn start(self: &Shared, data_dir: PathBuf) {
        let c = self.clone();
        spawn("hestia-init", move || {
            c.ui(|ui| ui.set_starting_text("Loading this device's identity…".into()));
            let core = match HestiaClient::open(&data_dir) {
                Ok(core) => Arc::new(core),
                Err(e) => {
                    let text = format!("Could not start: {e}\n({})", data_dir.display());
                    c.ui(move |ui| ui.set_starting_text(text.into()));
                    return;
                }
            };
            let _ = c.core.set(core.clone());
            // Keep the log on disk too, so a killed app leaves it behind.
            if let Err(e) = hestia_core::logcapture::persist(&platform::reports_dir(core.data_dir())) {
                warn!("could not keep the log on disk: {e}");
            }
            info!("{} starting", view::version_text());
            if let Some(device) = platform::device_summary() {
                info!("device: {device}");
            }
            let settings = core.settings();
            c.touch_mode.store(settings.touch().index() as u8, Ordering::Relaxed);
            let client_id = core.identity().unique_id().to_owned();
            let dir = core.data_dir().display().to_string();
            c.ui(move |ui| {
                view::load_settings_into(&ui, &settings, platform::native_display());
                ui.set_stream_touch_mode(settings.touch().index());
                ui.set_version_text(view::version_text().into());
                ui.set_client_id(client_id.into());
                ui.set_data_dir(dir.into());
                ui.set_starting(false);
            });
            c.refresh_all(false);
            c.start_discovery();
        });
    }

    fn start_discovery(self: &Shared) {
        let c = self.clone();
        spawn("hestia-discovery", move || {
            let (browser, rx) = match hestia_core::discovery::Browser::start() {
                Ok(v) => v,
                Err(e) => {
                    warn!("mDNS discovery unavailable: {e}");
                    return;
                }
            };
            c.set_scanning_for(Duration::from_secs(5));
            while let Ok(found) = rx.recv() {
                let Some(core) = c.core() else { break };
                match core.note_discovered(&found) {
                    Ok(id) => {
                        info!("discovered {} at {:?}", found.name, found.addresses);
                        let known = {
                            let st = c.state();
                            st.snapshots.contains_key(&id) || st.checking.contains(&id)
                        };
                        if !known {
                            c.refresh_host(id);
                        } else {
                            c.push_hosts();
                        }
                    }
                    Err(e) => warn!("ignoring discovered host {}: {e}", found.name),
                }
            }
            drop(browser);
        });
    }

    fn set_scanning_for(self: &Shared, duration: Duration) {
        self.ui(|ui| ui.set_scanning(true));
        let c = self.clone();
        spawn("hestia-scan-timer", move || {
            std::thread::sleep(duration);
            c.ui(|ui| ui.set_scanning(false));
        });
    }

    // ------------------------------------------------------------------
    // Hosts
    // ------------------------------------------------------------------

    fn push_hosts(self: &Shared) {
        let Some(core) = self.core() else { return };
        let cards: Vec<_> = {
            let st = self.state();
            core.hosts()
                .iter()
                .map(|r| view::host_card(r, st.snapshots.get(&r.id), st.checking.contains(&r.id)))
                .collect()
        };
        self.ui(move |ui| ui.set_hosts(ModelRc::new(VecModel::from(cards))));
    }

    fn refresh_all(self: &Shared, scan_indicator: bool) {
        let Some(core) = self.core() else { return };
        if scan_indicator {
            self.set_scanning_for(Duration::from_secs(3));
        }
        for host in core.hosts() {
            self.refresh_host(host.id);
        }
        self.push_hosts();
    }

    fn refresh_host(self: &Shared, id: String) {
        self.state().checking.insert(id.clone());
        self.push_hosts();
        let c = self.clone();
        spawn("hestia-refresh", move || {
            let Some(core) = c.core() else { return };
            let result = core.refresh(&id);
            {
                let mut st = c.state();
                st.checking.remove(&id);
                if let Ok(snapshot) = &result {
                    st.snapshots.remove(&id);
                    if st.current_host.as_deref() == Some(id.as_str()) {
                        st.current_host = Some(snapshot.record.id.clone());
                    }
                    st.snapshots.insert(snapshot.record.id.clone(), snapshot.clone());
                }
            }
            if let Err(e) = result {
                warn!("refresh of {id} failed: {e}");
            }
            c.push_hosts();
        });
    }

    fn add_host(self: &Shared, address: String) {
        let address = address.trim().to_owned();
        if address.is_empty() {
            return;
        }
        self.ui(|ui| {
            ui.set_add_busy(true);
            ui.set_add_error(SharedString::new());
        });
        let c = self.clone();
        spawn("hestia-add", move || {
            let Some(core) = c.core() else { return };
            match core.add_manual(&address) {
                Ok(snapshot) => {
                    let name = snapshot.record.name.clone();
                    c.state().snapshots.insert(snapshot.record.id.clone(), snapshot);
                    c.ui(|ui| {
                        ui.set_add_busy(false);
                        ui.set_add_visible(false);
                        ui.set_add_address(SharedString::new());
                    });
                    c.toast(format!("Added {name}"));
                }
                Err(e) => {
                    let text = e.to_string();
                    c.ui(move |ui| {
                        ui.set_add_busy(false);
                        ui.set_add_error(text.into());
                    });
                }
            }
            c.push_hosts();
        });
    }

    fn open_host(self: &Shared, id: String) {
        let Some(core) = self.core() else { return };
        let Ok(record) = core.host(&id) else { return };
        let snapshot = self.state().snapshots.get(&id).cloned();
        match snapshot {
            Some(s) if s.online && s.paired => self.show_apps(s),
            Some(s) if s.online => self.start_pairing(id, record.name),
            Some(_) if record.mac.is_some() => self.wake_host(id, record.name),
            Some(_) => {
                self.toast(format!(
                    "{} is offline (Wake-on-LAN needs its MAC address, learned once it's been online)",
                    record.name
                ));
                self.refresh_host(id);
            }
            None => self.toast(format!("Still contacting {}…", record.name)),
        }
    }

    /// Wake-on-LAN, then check every few seconds for a minute.
    fn wake_host(self: &Shared, id: String, name: String) {
        let c = self.clone();
        spawn("hestia-wake", move || {
            let Some(core) = c.core() else { return };
            match core.wake(&id) {
                Ok(_) => c.toast(format!("Waking {name}… this can take a minute")),
                Err(e) => return c.toast(format!("Could not wake {name}: {e}")),
            }
            for _ in 0..12 {
                std::thread::sleep(Duration::from_secs(5));
                if let Ok(snapshot) = core.refresh(&id) {
                    let online = snapshot.online;
                    c.state().snapshots.insert(snapshot.record.id.clone(), snapshot);
                    c.push_hosts();
                    if online {
                        c.toast(format!("{name} is awake"));
                        return;
                    }
                }
            }
            c.toast(format!("{name} didn't answer; check that Wake-on-LAN is enabled on it"));
        });
    }

    fn forget_current(self: &Shared) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().current_host.take() else {
            return;
        };
        let name = core.host(&id).map(|h| h.name).unwrap_or_default();
        if let Err(e) = core.forget(&id) {
            self.toast(format!("Could not forget host: {e}"));
            return;
        }
        self.state().snapshots.remove(&id);
        self.ui(|ui| ui.set_screen(0));
        self.push_hosts();
        self.toast(format!("Forgot {name}. Remove this device in the host's web UI too."));
    }

    // ------------------------------------------------------------------
    // Pairing
    // ------------------------------------------------------------------

    fn start_pairing(self: &Shared, id: String, name: String) {
        let generation = {
            let mut st = self.state();
            st.pair_generation += 1;
            st.pairing_host = Some(id.clone());
            st.pair_generation
        };
        let pin = PairingPin::random();
        let pin_text: SharedString = pin.as_str().into();
        self.ui(move |ui| {
            ui.set_pair_host_name(name.into());
            ui.set_pair_pin(pin_text);
            ui.set_pair_status("Waiting for the PIN to be entered on the host…".into());
            ui.set_pair_busy(true);
            ui.set_pair_failed(false);
            ui.set_pair_otp_mode(false);
            ui.set_pair_visible(true);
        });
        self.run_pairing(id, generation, pin, None);
    }

    fn run_pairing(self: &Shared, id: String, generation: u64, pin: PairingPin, passphrase: Option<String>) {
        let c = self.clone();
        spawn("hestia-pair", move || {
            let Some(core) = c.core() else { return };
            let result = core.pair(&id, &pin, passphrase.as_deref());
            if c.state().pair_generation != generation {
                return; // abandoned attempt
            }
            let failure = match result {
                Ok((PairingOutcome::Paired { .. }, new_id)) => {
                    c.state().pairing_host = None;
                    c.ui(|ui| ui.set_pair_visible(false));
                    c.toast("Paired");
                    c.after_pairing(new_id);
                    return;
                }
                Ok((PairingOutcome::WrongPin, _)) => "That PIN didn't match. Try again with a new one.".to_owned(),
                Ok((PairingOutcome::AlreadyInProgress, _)) => {
                    "The host is busy with another pairing attempt. Try again in a moment.".to_owned()
                }
                Ok((PairingOutcome::Failed(why), _)) => why,
                Err(e) => {
                    let text = e.to_string();
                    match crate::hostinfo::network_hint(&text) {
                        Some(hint) => format!("{text}\n{hint}"),
                        None => text,
                    }
                }
            };
            c.ui(move |ui| {
                ui.set_pair_busy(false);
                ui.set_pair_failed(true);
                ui.set_pair_status(failure.into());
            });
        });
    }

    fn after_pairing(self: &Shared, id: String) {
        let c = self.clone();
        spawn("hestia-post-pair", move || {
            let Some(core) = c.core() else { return };
            match core.refresh(&id) {
                Ok(snapshot) => {
                    c.state().snapshots.insert(snapshot.record.id.clone(), snapshot.clone());
                    c.push_hosts();
                    if snapshot.paired {
                        c.show_apps(snapshot);
                    }
                }
                Err(e) => warn!("refresh after pairing failed: {e}"),
            }
        });
    }

    fn restart_pairing(self: &Shared) {
        let Some(core) = self.core() else { return };
        let Some(id) = self.state().pairing_host.clone() else {
            return;
        };
        let name = core.host(&id).map(|h| h.name).unwrap_or_default();
        self.start_pairing(id, name);
    }

    fn switch_to_otp(self: &Shared) {
        let Some(id) = self.state().pairing_host.clone() else {
            return;
        };
        self.state().pair_generation += 1;
        self.ui(|ui| {
            ui.set_pair_otp_mode(true);
            ui.set_pair_busy(false);
            ui.set_pair_failed(false);
            ui.set_pair_status(SharedString::new());
        });
        // Drop the pending client-PIN attempt on the host.
        let c = self.clone();
        spawn("hestia-pair-cancel", move || {
            if let Some(core) = c.core() {
                core.cancel_pairing(&id);
            }
        });
    }

    fn submit_otp(self: &Shared, pin: String, passphrase: String) {
        let Some(pin) = PairingPin::parse(&pin) else {
            self.ui(|ui| {
                ui.set_pair_failed(true);
                ui.set_pair_status("The PIN must be at least 4 digits.".into());
            });
            return;
        };
        let (id, generation) = {
            let mut st = self.state();
            st.pair_generation += 1;
            (st.pairing_host.clone(), st.pair_generation)
        };
        let Some(id) = id else { return };
        self.ui(|ui| {
            ui.set_pair_busy(true);
            ui.set_pair_failed(false);
            ui.set_pair_status("Pairing…".into());
        });
        self.run_pairing(id, generation, pin, Some(passphrase.trim().to_owned()));
    }

    fn cancel_pairing(self: &Shared) {
        let id = {
            let mut st = self.state();
            st.pair_generation += 1;
            st.pairing_host.take()
        };
        self.ui(|ui| {
            ui.set_pair_visible(false);
            ui.set_otp_pin(SharedString::new());
            ui.set_otp_passphrase(SharedString::new());
        });
        if let Some(id) = id {
            let c = self.clone();
            spawn("hestia-pair-cancel", move || {
                if let Some(core) = c.core() {
                    core.cancel_pairing(&id);
                }
            });
        }
    }

    // ------------------------------------------------------------------
    // Apps
    // ------------------------------------------------------------------

    fn show_apps(self: &Shared, snapshot: HostSnapshot) {
        let id = snapshot.record.id.clone();
        self.state().current_host = Some(id.clone());
        let name: SharedString = snapshot.record.name.as_str().into();
        let detail = match &snapshot.info {
            Some(info) => format!("{} · {}", snapshot.record.address, info.app_version),
            None => snapshot.record.address.clone(),
        };
        let chips = view::host_chips(&snapshot);
        let running = snapshot.info.as_ref().is_some_and(|i| i.is_busy());
        let hermes = snapshot.capabilities.is_some();
        self.ui(move |ui| {
            ui.set_host_hermes(hermes);
            ui.set_host_name(name);
            ui.set_host_detail(detail.into());
            ui.set_host_chips(ModelRc::new(VecModel::from(chips)));
            ui.set_apps(ModelRc::new(VecModel::from(Vec::new())));
            ui.set_apps_loading(true);
            ui.set_app_running(running);
            ui.set_screen(1);
        });

        let running_app = snapshot.info.as_ref().map_or(0, |i| i.current_game);
        let c = self.clone();
        spawn("hestia-apps", move || {
            let Some(core) = c.core() else { return };
            let apps = match core.apps(&id) {
                Ok(apps) => apps,
                Err(e) => {
                    c.ui(|ui| ui.set_apps_loading(false));
                    c.toast(format!("Could not load apps: {e}"));
                    return;
                }
            };
            c.state().apps = apps.clone();
            let for_ui = apps.clone();
            c.ui(move |ui| {
                // Cards hold a slint::Image (not Send), so build them here.
                let cards: Vec<_> = for_ui.iter().map(|a| view::app_card(a, running_app)).collect();
                ui.set_apps(ModelRc::new(VecModel::from(cards)));
                ui.set_apps_loading(false);
            });

            for (index, app) in apps.iter().enumerate() {
                if c.state().current_host.as_deref() != Some(id.as_str()) {
                    return; // user left the page
                }
                let Ok(bytes) = core.box_art(&id, app.id) else { continue };
                let Some((raw, w, h)) = view::decode_art(&bytes) else {
                    continue;
                };
                let app_id = app.id as i32;
                c.ui(move |ui| {
                    let model = ui.get_apps();
                    if let Some(mut card) = model.row_data(index) {
                        if card.id == app_id {
                            card.art = view::art_image(&raw, w, h);
                            card.has_art = true;
                            model.set_row_data(index, card);
                        }
                    }
                });
            }
        });
    }

    /// Re-reads the host's running app and updates the app grid.
    fn refresh_current_running(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let c = self.clone();
        spawn("hestia-running", move || {
            let Some(core) = c.core() else { return };
            let Ok(snapshot) = core.refresh(&id) else { return };
            let running_app = snapshot.info.as_ref().map_or(0, |i| i.current_game);
            c.state().snapshots.insert(snapshot.record.id.clone(), snapshot);
            c.ui(move |ui| {
                ui.set_app_running(running_app != 0);
                let model = ui.get_apps();
                for i in 0..model.row_count() {
                    if let Some(mut card) = model.row_data(i) {
                        let running = running_app != 0 && card.id as u32 == running_app;
                        if card.running != running {
                            card.running = running;
                            model.set_row_data(i, card);
                        }
                    }
                }
            });
            c.push_hosts();
        });
    }

    fn launch(self: &Shared, app_id: u32) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        let (title, host_name, address, other_running) = {
            let st = self.state();
            let title = st
                .apps
                .iter()
                .find(|a| a.id == app_id)
                .map(|a| a.title.clone())
                .unwrap_or_default();
            let snapshot = st.snapshots.get(&id);
            let current = snapshot.and_then(|s| s.info.as_ref()).map_or(0, |i| i.current_game);
            let host_name = snapshot.map(|s| s.record.name.clone()).unwrap_or_default();
            let address = snapshot.map(|s| s.record.address.clone()).unwrap_or_default();
            (title, host_name, address, current != 0 && current != app_id)
        };
        if other_running {
            self.toast("Another app is running on this host. Quit it first.");
            return;
        }

        let seq = {
            let mut st = self.state();
            if st.launching.is_some() {
                return;
            }
            st.launch_seq += 1;
            st.launching = Some((st.launch_seq, id.clone()));
            st.launch_seq
        };
        let c = self.clone();
        self.ui(move |ui| {
            ui.set_session_title(title.as_str().into());
            ui.set_session_subtitle(host_name.as_str().into());
            ui.set_session_lines(lines(&["Starting on the host…"]));
            ui.set_session_busy(true);
            ui.set_screen(2);

            // This display, landscape (streams are), else the window.
            let mut device = DeviceInfo::default();
            if let Some((w, h, hz)) = platform::native_display() {
                (device.display_width, device.display_height, device.refresh_rate) = (w, h, hz);
            } else {
                let size = ui.window().size();
                if size.width > 0 && size.height > 0 {
                    device.display_width = size.width.max(size.height);
                    device.display_height = size.width.min(size.height);
                }
            }
            spawn("hestia-launch", move || {
                let Some(core) = c.core() else { return };
                let session_report = report::begin(&title, &host_name, &device);
                let failure: String;
                let settings = core.settings();
                let colorimetry = hestia_media::Colorimetry::default();
                #[cfg(target_os = "android")]
                let decoder = if settings.hardware_decoding {
                    let candidates = platform::video_decoders(settings.width, settings.height, settings.fps)
                        .unwrap_or_else(|e| {
                            warn!("could not check Android video decoders: {e}");
                            Vec::new()
                        });
                    hestia_media::decoder_with_android_capabilities(colorimetry, candidates)
                } else {
                    hestia_media::decoder_with(colorimetry, false)
                };
                #[cfg(not(target_os = "android"))]
                let decoder = hestia_media::decoder_with(colorimetry, settings.hardware_decoding);
                let launched = decoder.and_then(|decoder| {
                    if !c.launch_current(seq) {
                        return Err("cancelled".into());
                    }
                    core.launch_with(&id, app_id, &device, settings.clone())
                        .map(|session| (core.stream_host(&session), session, decoder))
                        .map_err(|error| error.to_string())
                });
                if !c.launch_current(seq) {
                    // Release the preparation before another attempt can start.
                    drop(launched);
                    c.finish_launch(seq);
                    c.finish_report(session_report, "cancelled while starting", Some(id.clone()));
                    return;
                }
                let text: Vec<String> = match launched {
                    Ok((host, session, decoder)) => {
                        report::launched(&session_report, &session);
                        if let Some(why) = &session.prepare_error {
                            c.toast(format!(
                                "Hermes declined to prepare this session ({why}); streaming in standard mode"
                            ));
                        }
                        c.ui(|ui| ui.set_session_lines(lines(&["Connecting to the stream…"])));
                        match c.start_stream(id.clone(), &session, host, decoder, session_report.clone(), seq) {
                            Ok(()) => {
                                c.finish_launch(seq);
                                c.ui(|ui| ui.set_session_busy(false));
                                c.refresh_current_running();
                                return;
                            }
                            Err(_) if !c.launch_current(seq) => {
                                c.finish_launch(seq);
                                c.finish_report(session_report, "cancelled while connecting", Some(id.clone()));
                                return;
                            }
                            Err(e) => {
                                failure = format!("the stream could not start: {e}");
                                let mut out = vec![format!(
                                    "{} on {}, but the stream could not start: {e}",
                                    if session.response.resumed { "Resumed" } else { "Started" },
                                    session.host.name
                                )];
                                if let Some(p) = &session.prepared {
                                    let vd = &p.virtual_display;
                                    out.push(format!(
                                        "Hermes display: {} via {}, {}×{} @ {}{}",
                                        vd.name,
                                        vd.backend,
                                        vd.width,
                                        vd.height,
                                        vd.fps,
                                        if vd.hdr { ", HDR" } else { "" }
                                    ));
                                    out.extend(p.warnings.iter().map(|w| format!("Warning: {w}")));
                                }
                                out.push("The app keeps running on the host until you quit it.".into());
                                out
                            }
                        }
                    }
                    Err(e) => {
                        report::settings(&session_report, &settings);
                        session_report
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .field("Host", "address", &address);
                        failure = format!("could not start the app: {e}");
                        vec![format!("Could not start the app: {e}")]
                    }
                };
                c.finish_launch(seq);
                let show_report = c.finish_report(session_report, &failure, Some(id.clone()));
                let model_lines: Vec<SharedString> = text.into_iter().map(SharedString::from).collect();
                c.ui(move |ui| {
                    ui.set_session_lines(ModelRc::new(VecModel::from(model_lines)));
                    ui.set_session_busy(false);
                    if show_report && ui.get_screen() == 2 {
                        ui.set_screen(6);
                    }
                });
                c.refresh_current_running();
            });
        });
    }

    fn launch_current(&self, seq: u64) -> bool {
        let state = self.state();
        state.launch_seq == seq && state.launching.as_ref().is_some_and(|(s, _)| *s == seq)
    }

    fn finish_launch(&self, seq: u64) {
        let mut st = self.state();
        if st.launching.as_ref().is_some_and(|(s, _)| *s == seq) {
            st.launching = None;
        }
    }

    /// Cancels the launch in progress, if any: back to the apps right away;
    /// the launch thread backs out and Hermes gets its display back.
    fn cancel_launch(self: &Shared) -> bool {
        {
            let mut state = self.state();
            if !state
                .launching
                .as_ref()
                .is_some_and(|(seq, _)| *seq == state.launch_seq)
            {
                return false;
            }
            state.launch_seq += 1;
        }
        info!("launch cancelled by the user");
        self.ui(|ui| {
            ui.set_session_busy(false);
            ui.set_screen(1);
        });
        self.toast("Cancelled. If the app already started on the host, quit it from the app list.");
        true
    }

    fn quit_app(self: &Shared) {
        let Some(id) = self.state().current_host.clone() else {
            return;
        };
        self.ui(|ui| ui.set_session_busy(true));
        let c = self.clone();
        spawn("hestia-quit", move || {
            let Some(core) = c.core() else { return };
            match core.quit(&id) {
                Ok(()) => c.toast("App closed"),
                Err(e) => c.toast(format!("Could not quit: {e}")),
            }
            c.ui(|ui| {
                ui.set_session_busy(false);
                if ui.get_screen() == 2 {
                    ui.set_screen(1);
                }
            });
            c.refresh_current_running();
        });
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    fn save_settings(self: &Shared) {
        let Some(core) = self.core() else { return };
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // Called from the UI thread, so the window is reachable directly.
        let Some(ui) = weak.upgrade() else { return };
        let settings = view::read_settings_from(&ui, &core.settings());
        match core.set_settings(settings) {
            Ok(()) => self.toast("Settings saved"),
            Err(e) => self.toast(format!("Could not save settings: {e}")),
        }
    }

    /// A preset picked in Settings: fill in resolution, frame rate and
    /// bitrate for this display (saved with the rest on Save).
    fn apply_preset(self: &Shared, index: i32) {
        let Some(core) = self.core() else { return };
        let Some(&preset) = Preset::ALL.get(index.max(0) as usize) else {
            return;
        };
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(ui) = weak.upgrade() else { return };
        let native = platform::native_display();
        let mut settings = view::read_settings_from(&ui, &core.settings());
        settings.preset = preset;
        if let Some((w, h, fps, kbps)) = presets::preset_mode(preset, native.unwrap_or((1920, 1080, 60))) {
            (settings.width, settings.height, settings.fps, settings.bitrate_kbps) = (w, h, fps, kbps);
        }
        view::load_settings_into(&ui, &settings, native);
    }

    /// Settings → Custom resolution: adds it to the list and selects it.
    fn custom_resolution(self: &Shared, text: &str) {
        let Some((width, height)) = view::parse_resolution(text) else {
            self.toast("Type a size like 2560x1080 (320x200 to 7680x4320)");
            return;
        };
        let Some(core) = self.core() else { return };
        let weak = self.weak.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let Some(ui) = weak.upgrade() else { return };
        let mut settings = view::read_settings_from(&ui, &core.settings());
        (settings.width, settings.height) = (width, height);
        settings.preset = Preset::Custom;
        view::load_settings_into(&ui, &settings, platform::native_display());
        self.toast(format!("Resolution set to {width}×{height}; Save to keep it"));
    }

    /// Fit → Fill → Stretch, from the stream menu; remembered for next time.
    fn cycle_video_scale(self: &Shared) {
        let Some(core) = self.core() else { return };
        let mut settings = core.settings();
        settings.video_scale = settings.video_scale.next();
        let scale = settings.video_scale;
        self.ui(move |ui| {
            ui.set_stream_video_scale(scale.index());
            ui.set_cfg_video_scale(scale.index());
        });
        self.toast(match scale {
            hestia_core::VideoScale::Fit => "Picture: fit (whole picture, black bars if needed)",
            hestia_core::VideoScale::Fill => "Picture: fill (no bars, edges cropped)",
            hestia_core::VideoScale::Stretch => "Picture: stretch (no bars, distorted)",
        });
        spawn("hestia-settings", move || {
            if let Err(e) = core.set_settings(settings) {
                warn!("saving the picture scale failed: {e}");
            }
        });
    }
}

fn lines(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>(),
    ))
}
