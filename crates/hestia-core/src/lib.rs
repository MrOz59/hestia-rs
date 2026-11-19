//! UI-agnostic client facade. The Slint app, the CLI and (later) other
//! front-ends all drive hosts through [`HestiaClient`]:
//!
//! * loads or creates the client identity,
//! * keeps the list of known hosts (manual + discovered) on disk,
//! * pairs, lists apps, fetches box art,
//! * probes Hermes' Hestia API and uses it when present,
//! * launches/quits apps, handing the streaming layer what it needs.
//!
//! All methods are blocking and thread-safe; call them from worker threads.

pub mod address;
pub mod settings;
pub mod store;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hestia_api::{
    AppRequest, Capabilities, ClientDescription, HestiaApi, SessionPrepareRequest, SessionPrepareResponse,
    StreamRequest, VirtualDisplayRequest,
};
use hestia_crypto::{ClientIdentity, pem};
use hestia_discovery::DiscoveredHost;
use hestia_nvhttp::{
    AppInfo, AudioConfig, DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT, LaunchRequest, LaunchResponse, NvHttpClient,
    PairingOutcome, PairingPin, ServerInfo,
};
use log::{info, warn};
use serde::{Deserialize, Serialize};

pub use hestia_api as api;
pub use hestia_discovery as discovery;
pub use hestia_nvhttp as nvhttp;
pub use settings::{AudioLayout, CodecPreference, LaunchMode, StreamSettings};
pub use store::Store;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("storage error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Crypto(#[from] hestia_crypto::CryptoError),
    #[error(transparent)]
    Host(#[from] hestia_nvhttp::NvHttpError),
    #[error(transparent)]
    Api(#[from] hestia_api::ApiError),
    #[error("unknown host {0}")]
    UnknownHost(String),
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// A host as remembered on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostRecord {
    /// The host's GameStream unique id once known; a placeholder before the
    /// first successful `/serverinfo`.
    pub id: String,
    pub name: String,
    /// Address used to reach it (IP or DNS name).
    pub address: String,
    pub http_port: u16,
    pub https_port: u16,
    /// Pinned host certificate (PEM), present once paired.
    pub server_cert_pem: Option<String>,
    pub mac: Option<String>,
    /// Added by hand rather than discovered.
    pub manual: bool,
    /// Unix seconds of the last successful contact.
    pub last_seen: Option<u64>,
}

impl Default for HostRecord {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            address: String::new(),
            http_port: DEFAULT_HTTP_PORT,
            https_port: DEFAULT_HTTPS_PORT,
            server_cert_pem: None,
            mac: None,
            manual: false,
            last_seen: None,
        }
    }
}

impl HostRecord {
    pub fn is_paired(&self) -> bool {
        self.server_cert_pem.is_some()
    }

    fn server_cert_der(&self) -> Option<Vec<u8>> {
        self.server_cert_pem
            .as_deref()
            .and_then(|p| pem::decode("CERTIFICATE", p))
    }
}

/// Live view of a host after talking to it.
#[derive(Debug, Clone)]
pub struct HostSnapshot {
    pub record: HostRecord,
    pub online: bool,
    /// The host still accepts our certificate.
    pub paired: bool,
    pub info: Option<ServerInfo>,
    /// Present when the host is Hermes (Hestia protocol v1) and we are paired.
    pub capabilities: Option<Capabilities>,
    pub error: Option<String>,
}

/// Everything the streaming layer needs once `/launch` succeeded.
#[derive(Debug, Clone)]
pub struct LaunchedSession {
    pub host: HostRecord,
    pub request: LaunchRequest,
    pub response: LaunchResponse,
    pub server_info: ServerInfo,
    /// Hermes session preparation result, when the host supports it.
    pub prepared: Option<SessionPrepareResponse>,
}

/// Facts about this device that go into Hermes' session preparation.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub platform: &'static str,
    pub display_width: u32,
    pub display_height: u32,
    pub refresh_rate: u32,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        Self {
            platform: if cfg!(target_os = "android") {
                "android"
            } else if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else if cfg!(target_os = "macos") {
                "macos"
            } else {
                "unknown"
            },
            display_width: 1920,
            display_height: 1080,
            refresh_rate: 60,
        }
    }
}

pub struct HestiaClient {
    store: Store,
    identity: Arc<ClientIdentity>,
    hosts: Mutex<Vec<HostRecord>>,
    settings: Mutex<StreamSettings>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl HestiaClient {
    /// Opens (or initialises) the client state in `data_dir`. Generates the
    /// identity on first run, which takes a moment.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self> {
        let store = Store::open(data_dir)?;
        let identity = match store.load_identity()? {
            Some(saved) => ClientIdentity::from_pem(&saved.unique_id, &saved.certificate_pem, &saved.private_key_pem)?,
            None => {
                info!("generating a new client identity");
                let identity = ClientIdentity::generate()?;
                store.save_identity(&store::StoredIdentity {
                    unique_id: identity.unique_id().to_owned(),
                    certificate_pem: identity.cert_pem(),
                    private_key_pem: identity.key_pem(),
                })?;
                identity
            }
        };
        let hosts = store.load_hosts()?;
        let settings = store.load_settings().unwrap_or_else(|e| {
            warn!("ignoring unreadable settings: {e}");
            StreamSettings::default()
        });
        Ok(Self {
            store,
            identity: Arc::new(identity),
            hosts: Mutex::new(hosts),
            settings: Mutex::new(settings),
        })
    }

    pub fn identity(&self) -> &ClientIdentity {
        &self.identity
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.store.dir()
    }

    // ------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------

    pub fn settings(&self) -> StreamSettings {
        self.settings.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub fn set_settings(&self, settings: StreamSettings) -> Result<()> {
        self.store.save_settings(&settings)?;
        *self.settings.lock().unwrap_or_else(|p| p.into_inner()) = settings;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Host registry
    // ------------------------------------------------------------------

    pub fn hosts(&self) -> Vec<HostRecord> {
        self.lock_hosts().clone()
    }

    pub fn host(&self, id: &str) -> Result<HostRecord> {
        self.lock_hosts()
            .iter()
            .find(|h| h.id == id)
            .cloned()
            .ok_or_else(|| CoreError::UnknownHost(id.to_owned()))
    }

    fn lock_hosts(&self) -> std::sync::MutexGuard<'_, Vec<HostRecord>> {
        self.hosts.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn update_hosts<R>(&self, f: impl FnOnce(&mut Vec<HostRecord>) -> R) -> Result<R> {
        let mut hosts = self.lock_hosts();
        let result = f(&mut hosts);
        self.store.save_hosts(&hosts)?;
        Ok(result)
    }

    /// Adds a host by address (`192.168.0.10`, `pc.local:47989`, `[fe80::1]:47989`)
    /// and contacts it.
    pub fn add_manual(&self, address: &str) -> Result<HostSnapshot> {
        let (host, port) = address::parse(address)?;
        let existing = self
            .lock_hosts()
            .iter()
            .find(|h| h.address.eq_ignore_ascii_case(&host) && h.http_port == port)
            .map(|h| h.id.clone());
        let id = match existing {
            Some(id) => id,
            None => {
                let record = HostRecord {
                    id: format!("pending:{host}:{port}"),
                    name: host.clone(),
                    address: host,
                    http_port: port,
                    manual: true,
                    ..Default::default()
                };
                let id = record.id.clone();
                self.update_hosts(|hosts| hosts.push(record))?;
                id
            }
        };
        let snapshot = self.refresh(&id)?;
        if !snapshot.online {
            // Keep it so the user can retry, but report the failure.
            return Err(CoreError::Invalid(
                snapshot.error.unwrap_or_else(|| "host did not answer".into()),
            ));
        }
        Ok(snapshot)
    }

    /// Records a host seen via mDNS. Returns the id of the matching record.
    pub fn note_discovered(&self, found: &DiscoveredHost) -> Result<String> {
        let addresses: Vec<String> = found.addresses.iter().map(|a| a.to_string()).collect();
        let Some(primary) = addresses.first().cloned() else {
            return Err(CoreError::InvalidAddress(format!("{} has no address", found.name)));
        };
        self.update_hosts(|hosts| {
            if let Some(h) = hosts
                .iter_mut()
                .find(|h| addresses.contains(&h.address) || (!h.name.is_empty() && h.name == found.name))
            {
                if !addresses.contains(&h.address) && !h.manual {
                    h.address = primary.clone();
                }
                h.http_port = found.port;
                return h.id.clone();
            }
            let record = HostRecord {
                id: format!("pending:{primary}:{}", found.port),
                name: found.name.clone(),
                address: primary.clone(),
                http_port: found.port,
                ..Default::default()
            };
            let id = record.id.clone();
            hosts.push(record);
            id
        })
    }

    /// Forgets a host locally (the host keeps its own paired-client entry
    /// until removed in its web UI).
    pub fn forget(&self, id: &str) -> Result<()> {
        self.update_hosts(|hosts| hosts.retain(|h| h.id != id))
    }

    /// A ready-to-use GameStream client for a host.
    pub fn nvhttp(&self, record: &HostRecord) -> NvHttpClient {
        NvHttpClient::new(record.address.clone(), record.http_port, self.identity.clone())
            .with_https_port(record.https_port)
            .with_server_cert(record.server_cert_der())
    }

    /// Talks to the host: `/serverinfo`, and for paired Hermes hosts the
    /// Hestia capabilities. Updates the stored record (id, name, ports...).
    pub fn refresh(&self, id: &str) -> Result<HostSnapshot> {
        let record = self.host(id)?;
        let mut client = self.nvhttp(&record);
        let info = match client.server_info() {
            Ok(info) => info,
            Err(e) => {
                return Ok(HostSnapshot {
                    paired: record.is_paired(),
                    record,
                    online: false,
                    info: None,
                    capabilities: None,
                    error: Some(e.to_string()),
                });
            }
        };

        let paired = record.is_paired() && info.paired;
        let capabilities = if paired {
            match HestiaApi::new(&client).capabilities() {
                Ok(caps) if caps.is_compatible() => Some(caps),
                Ok(caps) => {
                    info!(
                        "{} speaks Hestia protocol {}, which we don't",
                        record.name, caps.hestia_protocol
                    );
                    None
                }
                Err(e) => {
                    log::debug!("no Hestia API on {}: {e}", record.name);
                    None
                }
            }
        } else {
            None
        };

        let updated = self.update_hosts(|hosts| {
            let mut updated = record.clone();
            if !info.unique_id.is_empty() {
                updated.id = info.unique_id.clone();
            }
            if !info.hostname.is_empty() {
                updated.name = info.hostname.clone();
            }
            updated.https_port = client.https_port();
            updated.mac = info.mac.clone().or(updated.mac);
            updated.last_seen = Some(now_secs());

            // If another record already holds this unique id (e.g. the same PC
            // was discovered and added by hand), merge into one entry.
            if let Some(other) = hosts.iter().position(|h| h.id == updated.id && h.id != record.id) {
                let other = hosts.remove(other);
                if updated.server_cert_pem.is_none() {
                    updated.server_cert_pem = other.server_cert_pem;
                }
                updated.manual |= other.manual;
            }
            if let Some(slot) = hosts.iter_mut().find(|h| h.id == record.id) {
                *slot = updated.clone();
            } else {
                hosts.push(updated.clone());
            }
            updated
        })?;

        Ok(HostSnapshot {
            record: updated,
            online: true,
            paired,
            info: Some(info),
            capabilities,
            error: None,
        })
    }

    // ------------------------------------------------------------------
    // Pairing
    // ------------------------------------------------------------------

    /// Pairs with the host. With `otp_passphrase` = None, `pin` must be typed
    /// into the host's web UI while this call blocks. With a passphrase, `pin`
    /// and passphrase are the one-time values the host displayed.
    ///
    /// Returns the outcome and the host's id afterwards (a placeholder id is
    /// replaced by the host's unique id once known).
    pub fn pair(&self, id: &str, pin: &PairingPin, otp_passphrase: Option<&str>) -> Result<(PairingOutcome, String)> {
        let record = self.host(id)?;
        let mut client = self.nvhttp(&record);
        client.set_server_cert(None);
        let info = client.server_info()?;
        let outcome = client.pair(info.major_version(), pin, otp_passphrase)?;
        let mut final_id = record.id.clone();
        if let PairingOutcome::Paired { server_cert_der } = &outcome {
            let cert_pem = pem::encode("CERTIFICATE", server_cert_der);
            let https_port = client.https_port();
            let host_id = record.id.clone();
            final_id = self.update_hosts(|hosts| {
                let new_id = if info.unique_id.is_empty() {
                    host_id.clone()
                } else {
                    info.unique_id.clone()
                };
                // Drop a stale duplicate of the same host, if any.
                if new_id != host_id {
                    hosts.retain(|h| h.id != new_id);
                }
                if let Some(h) = hosts.iter_mut().find(|h| h.id == host_id) {
                    h.server_cert_pem = Some(cert_pem);
                    h.https_port = https_port;
                    h.id = new_id.clone();
                    if !info.hostname.is_empty() {
                        h.name = info.hostname.clone();
                    }
                }
                new_id
            })?;
        }
        Ok((outcome, final_id))
    }

    /// Asks the host to drop a pending pairing attempt (best effort).
    pub fn cancel_pairing(&self, id: &str) {
        if let Ok(record) = self.host(id) {
            self.nvhttp(&record).cancel_pairing();
        }
    }

    // ------------------------------------------------------------------
    // Apps & sessions
    // ------------------------------------------------------------------

    pub fn apps(&self, id: &str) -> Result<Vec<AppInfo>> {
        let record = self.host(id)?;
        Ok(self.nvhttp(&record).app_list()?)
    }

    pub fn box_art(&self, id: &str, app_id: u32) -> Result<Vec<u8>> {
        let record = self.host(id)?;
        Ok(self.nvhttp(&record).box_art(app_id)?)
    }

    pub fn capabilities(&self, id: &str) -> Result<Capabilities> {
        let record = self.host(id)?;
        Ok(HestiaApi::new(&self.nvhttp(&record)).capabilities()?)
    }

    /// Runs `f` with a Hestia API handle for the host.
    pub fn with_api<R>(&self, id: &str, f: impl FnOnce(&HestiaApi<'_>) -> R) -> Result<R> {
        let record = self.host(id)?;
        let client = self.nvhttp(&record);
        Ok(f(&HestiaApi::new(&client)))
    }

    /// Starts (or resumes) an app with the current settings. On Hermes the
    /// session is prepared first through the Hestia API.
    pub fn launch(&self, id: &str, app_id: u32, device: &DeviceInfo) -> Result<LaunchedSession> {
        let record = self.host(id)?;
        if !record.is_paired() {
            return Err(CoreError::Host(hestia_nvhttp::NvHttpError::NotPaired));
        }
        let settings = self.settings();
        let mut client = self.nvhttp(&record);
        let server_info = client.server_info()?;
        if !server_info.paired {
            return Err(CoreError::Host(hestia_nvhttp::NvHttpError::NotPaired));
        }
        if server_info.is_busy() && server_info.current_game != app_id {
            return Err(CoreError::Invalid(
                "another app is running on this host; quit it first".into(),
            ));
        }

        let mut request = LaunchRequest::new(app_id, settings.width, settings.height, settings.fps);
        request.optimize_game_settings = settings.optimize_game_settings;
        request.play_audio_on_host = settings.play_audio_on_host;
        request.audio = match settings.audio {
            AudioLayout::Stereo => AudioConfig::Stereo,
            AudioLayout::Surround51 => AudioConfig::Surround51,
            AudioLayout::Surround71 => AudioConfig::Surround71,
        };
        request.hdr = settings.hdr;
        request.virtual_display = settings.virtual_display && server_info.virtual_display_capable;
        if settings.scale_factor != 100 {
            request.scale_factor = Some(settings.scale_factor);
        }

        let prepared = self.prepare_hermes_session(&client, &settings, device, app_id);

        let response = if server_info.is_busy() {
            client.resume(&request)?
        } else {
            client.launch(&request)?
        };
        info!("session started on {}: {}", record.name, response.session_url);

        Ok(LaunchedSession {
            host: record,
            request,
            response,
            server_info,
            prepared,
        })
    }

    fn prepare_hermes_session(
        &self,
        client: &NvHttpClient,
        settings: &StreamSettings,
        device: &DeviceInfo,
        app_id: u32,
    ) -> Option<SessionPrepareResponse> {
        let api = HestiaApi::new(client);
        let caps = api.capabilities().ok().filter(Capabilities::is_compatible)?;
        let features = &caps.features;
        let hdr = settings.hdr && features.hdr_mode_control;
        let request = SessionPrepareRequest {
            client: ClientDescription {
                name: "Hestia RS".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                platform: device.platform.into(),
                display_width: device.display_width,
                display_height: device.display_height,
                refresh_rate: device.refresh_rate,
                hdr,
            },
            stream: StreamRequest {
                requested_width: settings.width,
                requested_height: settings.height,
                requested_fps: settings.fps,
                codec: settings.codec.api_name().into(),
                bitrate_kbps: settings.bitrate_kbps,
                hdr_mode: if hdr { "hdr" } else { "sdr" }.into(),
                scale_factor: if features.scale_factor {
                    settings.scale_factor
                } else {
                    100
                },
            },
            virtual_display: VirtualDisplayRequest {
                enabled: features.virtual_display && settings.virtual_display,
                backend: "auto".into(),
                desktop_integration: "auto".into(),
                recover_physical_monitor: features.display_recovery,
            },
            app: AppRequest {
                id: app_id.to_string(),
                launch_mode: if features.gamescope_session && settings.launch_mode == LaunchMode::Gamescope {
                    "gamescope"
                } else {
                    "normal"
                }
                .into(),
            },
        };
        match api.prepare_session(&request) {
            Ok(prepared) => {
                for warning in &prepared.warnings {
                    warn!("Hermes session warning: {warning}");
                }
                Some(prepared)
            }
            Err(e) => {
                warn!("Hermes session prepare failed, continuing with plain GameStream: {e}");
                None
            }
        }
    }

    /// Quits the running app (and, on Hermes, clears the prepared session).
    pub fn quit(&self, id: &str) -> Result<()> {
        let record = self.host(id)?;
        let client = self.nvhttp(&record);
        let api = HestiaApi::new(&client);
        if api.capabilities().is_ok_and(|c| c.is_compatible()) {
            if let Err(e) = api.stop_session() {
                log::debug!("Hestia session stop failed: {e}");
            }
        }
        client.quit_app()?;
        Ok(())
    }
}
