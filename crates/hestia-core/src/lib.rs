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
pub mod diagnosis;
pub mod logcapture;
pub mod presets;
pub mod report;
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
pub use hestia_stream as stream;
pub use presets::Preset;
pub use settings::{AudioLayout, CodecPreference, LaunchMode, StreamSettings, VideoScale};
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
    /// Hermes readiness preflight, runtime status and dependencies, when the
    /// host offers them to this client.
    pub diagnostics: Option<hestia_api::HostDiagnostics>,
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
    /// Why Hermes declined to prepare the session (the app was launched in
    /// plain GameStream mode instead).
    pub prepare_error: Option<String>,
    /// Settings in effect when the app was launched.
    pub settings: StreamSettings,
}

impl LaunchedSession {
    /// Adds what was asked for and what `/launch` (and Hermes session
    /// preparation) answered to a session report.
    pub fn add_to_report(&self, r: &mut report::StreamReport) {
        self.settings.add_to_report(r);
        let info = &self.server_info;
        r.field("Host", "name", &self.host.name);
        r.field("Host", "address", &self.host.address);
        r.field("Host", "version", &info.app_version);
        if !info.gfe_version.is_empty() {
            r.field("Host", "GFE version", &info.gfe_version);
        }
        r.field(
            "Host",
            "codecs",
            hestia_nvhttp::codecs::describe(info.codec_mode_support).join(", "),
        );
        r.field("Launch", "session URL", &self.response.session_url);
        r.field("Launch", "resumed", self.response.resumed);
        r.field(
            "Launch",
            "requested",
            format!("{}x{} @ {}", self.request.width, self.request.height, self.request.fps),
        );
        match &self.prepared {
            Some(p) => {
                let vd = &p.virtual_display;
                r.field("Launch", "Hermes session", &p.session_id);
                r.field(
                    "Launch",
                    "Hermes display",
                    format!(
                        "{} via {}, {}x{} @ {}{}",
                        vd.name,
                        vd.backend,
                        vd.width,
                        vd.height,
                        vd.fps,
                        if vd.hdr { ", HDR" } else { "" }
                    ),
                );
                for w in &p.warnings {
                    r.field("Launch", "Hermes warning", w);
                }
            }
            None => match &self.prepare_error {
                Some(why) => r.field(
                    "Launch",
                    "Hermes session",
                    format!("DECLINED: {why} (plain GameStream used)"),
                ),
                None => r.field("Launch", "Hermes session", "not prepared (not a Hermes host)"),
            },
        }
        r.event(if self.response.resumed {
            "app resumed on the host"
        } else {
            "app launched on the host"
        });
    }

    /// Parameters for [`hestia_stream::StreamSession`] matching this launch.
    /// `decoder_formats` are the formats the chosen decoder handles; they are
    /// reordered by the user's codec preference.
    pub fn stream_config(&self, decoder_formats: &[hestia_stream::VideoFormat]) -> Result<hestia_stream::StreamConfig> {
        use hestia_stream::{SessionUrl, StreamConfig, VideoCodec, is_private_address};

        let session_url = SessionUrl::parse(&self.response.session_url, &self.host.address)
            .map_err(|e| CoreError::Invalid(format!("bad session URL {:?}: {e}", self.response.session_url)))?;
        let remote_ip = session_url.host.parse::<std::net::IpAddr>().ok().or_else(|| {
            use std::net::ToSocketAddrs;
            (session_url.host.as_str(), session_url.port)
                .to_socket_addrs()
                .ok()?
                .next()
                .map(|a| a.ip())
        });
        let streaming_remotely = remote_ip.is_some_and(|ip| !is_private_address(ip));

        let preferred = match self.settings.codec {
            CodecPreference::Auto => None,
            CodecPreference::H264 => Some(VideoCodec::H264),
            CodecPreference::Hevc => Some(VideoCodec::Hevc),
            CodecPreference::Av1 => Some(VideoCodec::Av1),
        };
        let mut video_formats: Vec<_> = decoder_formats
            .iter()
            .copied()
            .filter(|f| self.request.hdr || !f.ten_bit)
            .collect();
        if let Some(codec) = preferred {
            video_formats.sort_by_key(|f| f.codec != codec);
        }
        if video_formats.is_empty() {
            return Err(CoreError::Invalid("the decoder supports no usable video format".into()));
        }

        Ok(StreamConfig {
            session_url,
            server_codec_modes: self.server_info.codec_mode_support,
            width: self.request.width,
            height: self.request.height,
            fps: self.request.fps,
            bitrate_kbps: self.settings.bitrate_kbps,
            packet_size: if streaming_remotely {
                StreamConfig::WAN_PACKET_SIZE
            } else {
                StreamConfig::LAN_PACKET_SIZE
            },
            video_formats,
            audio_channels: self.request.audio.channel_count() as u8,
            audio_channel_mask: self.request.audio.channel_mask(),
            remote_input_key: self.request.remote_input_key,
            remote_input_key_id: self.request.remote_input_key_id,
            streaming_remotely,
        })
    }
}

/// Hermes v1 validates `client.platform` against this list; anything else
/// (Android included) must be sent as `unknown`.
fn hermes_platform(platform: &str) -> &'static str {
    match platform {
        "linux" => "linux",
        "windows" => "windows",
        "macos" => "macos",
        "steamdeck" => "steamdeck",
        _ => "unknown",
    }
}

/// The mode must be one Hermes advertises (frame rates are a fixed list).
fn check_prepare_limits(limits: &hestia_api::Limits, s: &StreamSettings) -> std::result::Result<(), String> {
    if limits.max_width > 0 && (s.width > limits.max_width || s.height > limits.max_height) {
        return Err(format!(
            "{}x{} is above the host's limit of {}x{}",
            s.width, s.height, limits.max_width, limits.max_height
        ));
    }
    if !limits.supported_fps.is_empty() && !limits.supported_fps.contains(&s.fps) {
        let list: Vec<String> = limits.supported_fps.iter().map(u32::to_string).collect();
        return Err(format!(
            "Hermes doesn't offer {} fps (it offers {})",
            s.fps,
            list.join(", ")
        ));
    }
    if !(1..=500).contains(&s.scale_factor) {
        return Err(format!("scale factor {}% is outside 1-500%", s.scale_factor));
    }
    Ok(())
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
                    diagnostics: None,
                    error: Some(e.to_string()),
                });
            }
        };

        // When only the HTTPS port failed (busy host, blocked port), plain
        // HTTP can't tell whether we're paired: keep what we know.
        let paired = record.is_paired() && (info.paired || info.secure_error.is_some());
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
        // Readiness preflight for the host health badge (best effort: the
        // endpoint can be permission-gated or absent on older Hermes).
        let diagnostics = capabilities
            .as_ref()
            .and_then(|_| match HestiaApi::new(&client).host_diagnostics() {
                Ok(d) => Some(d),
                Err(e) => {
                    log::debug!("Hermes diagnostics unavailable on {}: {e}", record.name);
                    None
                }
            });

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
            diagnostics,
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
        self.launch_with(id, app_id, device, self.settings())
    }

    /// Like [`launch`](Self::launch) with one-off settings (not persisted).
    pub fn launch_with(
        &self,
        id: &str,
        app_id: u32,
        device: &DeviceInfo,
        settings: StreamSettings,
    ) -> Result<LaunchedSession> {
        let record = self.host(id)?;
        if !record.is_paired() {
            return Err(CoreError::Host(hestia_nvhttp::NvHttpError::NotPaired));
        }
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

        let (prepared, prepare_error) = match self.prepare_hermes_session(&client, &settings, device, app_id) {
            Some(Ok(prepared)) => (Some(prepared), None),
            Some(Err(e)) => (None, Some(e)),
            None => (None, None),
        };

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
            prepare_error,
            settings,
        })
    }

    fn prepare_hermes_session(
        &self,
        client: &NvHttpClient,
        settings: &StreamSettings,
        device: &DeviceInfo,
        app_id: u32,
    ) -> Option<std::result::Result<SessionPrepareResponse, String>> {
        let api = HestiaApi::new(client);
        let caps = api.capabilities().ok().filter(Capabilities::is_compatible)?;
        // Hermes rejects (400) rather than negotiates: check what it says it
        // accepts and explain, instead of sending something it will refuse.
        if let Err(why) = check_prepare_limits(&caps.limits, settings) {
            return Some(Err(why));
        }
        let features = &caps.features;
        let hdr = settings.hdr && features.hdr_mode_control;
        let request = SessionPrepareRequest {
            client: ClientDescription {
                name: "Hestia RS".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                platform: hermes_platform(device.platform).into(),
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
                Some(Ok(prepared))
            }
            Err(e) => {
                warn!("Hermes session prepare failed, continuing with plain GameStream: {e}");
                let why = if e.is_permission_denied() {
                    "this device is not allowed to prepare sessions".to_owned()
                } else {
                    e.to_string()
                };
                Some(Err(why))
            }
        }
    }

    /// Hermes: releases this client's prepared session (virtual display,
    /// Gamescope session) when its stream ends. Best effort; plain hosts and
    /// errors are ignored. Only this client's session is affected.
    pub fn end_hermes_session(&self, id: &str) {
        let Ok(record) = self.host(id) else { return };
        let client = self.nvhttp(&record);
        let api = HestiaApi::new(&client);
        if api.capabilities().is_ok_and(|c| c.is_compatible()) {
            match api.stop_session() {
                Ok(()) => info!("Hermes session on {} stopped", record.name),
                Err(e) => log::debug!("Hermes session stop failed: {e}"),
            }
        }
    }

    /// The host's web UI (Hermes/Sunshine/Apollo serve it on the HTTP port + 1).
    pub fn web_ui_url(&self, id: &str) -> Result<String> {
        let record = self.host(id)?;
        let host = if record.address.contains(':') && !record.address.starts_with('[') {
            format!("[{}]", record.address)
        } else {
            record.address.clone()
        };
        Ok(format!("https://{host}:{}", record.http_port + 1))
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

/// Adds the stream parameters a session asked for to a report.
pub fn add_stream_config_to_report(c: &hestia_stream::StreamConfig, decoder: &str, r: &mut report::StreamReport) {
    r.field("Stream", "RTSP", &c.session_url.raw);
    r.field("Stream", "decoder", decoder);
    r.field("Stream", "offered formats", format!("{:?}", c.video_formats));
    r.field("Stream", "packet size", c.packet_size);
    r.field("Stream", "remote (WAN)", c.streaming_remotely);
}

/// Adds what the RTSP handshake settled on to a report.
pub fn add_negotiated_to_report(n: &hestia_stream::rtsp::Negotiated, r: &mut report::StreamReport) {
    r.field("Negotiated", "video", format!("{:?}", n.video_format));
    r.field(
        "Negotiated",
        "audio",
        format!(
            "{} ch, {} streams, {} ms packets{}",
            n.audio_format.channels,
            n.audio_format.streams,
            n.audio_packet_duration_ms,
            if n.audio_encrypted { ", encrypted" } else { "" }
        ),
    );
    r.field("Negotiated", "encryption flags", format!("{:#x}", n.encryption_enabled));
    r.field(
        "Negotiated",
        "host feature flags",
        format!("{:#x}", n.host_feature_flags),
    );
    r.field(
        "Negotiated",
        "ports",
        format!(
            "video {} · audio {} · control {}",
            n.video_port, n.audio_port, n.control_port
        ),
    );
    r.field("Negotiated", "packet size", n.packet_size);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_prepare_validation() {
        assert_eq!(hermes_platform("android"), "unknown");
        assert_eq!(hermes_platform("linux"), "linux");
        let limits = hestia_api::Limits {
            max_width: 7680,
            max_height: 4320,
            max_fps: 240,
            supported_fps: vec![30, 40, 45, 60, 90, 120],
            supported_codecs: vec![],
        };
        let mut s = StreamSettings::default();
        (s.width, s.height, s.fps) = (1920, 1080, 60);
        assert_eq!(check_prepare_limits(&limits, &s), Ok(()));
        s.fps = 144;
        assert!(check_prepare_limits(&limits, &s).unwrap_err().contains("144 fps"));
        s.fps = 60;
        s.width = 8000;
        assert!(check_prepare_limits(&limits, &s).is_err());
        assert_eq!(check_prepare_limits(&hestia_api::Limits::default(), &s), Ok(()));
    }
    use hestia_stream::{StreamConfig, VideoCodec, VideoFormat};

    fn session(url: &str, codec: CodecPreference) -> LaunchedSession {
        let mut request = LaunchRequest::new(7, 1280, 720, 120);
        request.audio = AudioConfig::Surround51;
        request.remote_input_key = [9; 16];
        request.remote_input_key_id = 42;
        LaunchedSession {
            host: HostRecord {
                address: "192.168.1.20".into(),
                ..Default::default()
            },
            request,
            response: LaunchResponse {
                session_url: url.into(),
                resumed: false,
            },
            server_info: ServerInfo {
                codec_mode_support: 0x0101,
                ..Default::default()
            },
            prepared: None,
            prepare_error: None,
            settings: StreamSettings {
                bitrate_kbps: 15_000,
                codec,
                ..Default::default()
            },
        }
    }

    const H264: VideoFormat = VideoFormat {
        codec: VideoCodec::H264,
        ten_bit: false,
        yuv444: false,
    };
    const HEVC: VideoFormat = VideoFormat {
        codec: VideoCodec::Hevc,
        ten_bit: false,
        yuv444: false,
    };
    const HEVC10: VideoFormat = VideoFormat {
        codec: VideoCodec::Hevc,
        ten_bit: true,
        yuv444: false,
    };

    #[test]
    fn stream_config_from_launch() {
        let s = session("rtspenc://192.168.1.20:48010", CodecPreference::Auto);
        let c = s.stream_config(&[HEVC10, HEVC, H264]).unwrap();
        assert!(c.session_url.encrypted);
        assert_eq!((c.width, c.height, c.fps, c.bitrate_kbps), (1280, 720, 120, 15_000));
        assert_eq!((c.audio_channels, c.audio_channel_mask), (6, 0x3f));
        assert_eq!((c.remote_input_key, c.remote_input_key_id), ([9; 16], 42));
        assert_eq!(c.server_codec_modes, 0x0101);
        assert!(!c.streaming_remotely);
        assert_eq!(c.packet_size, StreamConfig::LAN_PACKET_SIZE);
        // No HDR requested: 10-bit formats are not offered.
        assert_eq!(c.video_formats, vec![HEVC, H264]);
    }

    #[test]
    fn stream_config_preferences_and_fallbacks() {
        let s = session("rtsp://203.0.113.9:48010", CodecPreference::H264);
        let c = s.stream_config(&[HEVC, H264]).unwrap();
        assert_eq!(c.video_formats, vec![H264, HEVC]);
        assert!(c.streaming_remotely);
        assert_eq!(c.packet_size, StreamConfig::WAN_PACKET_SIZE);

        // Old hosts send no session URL: use the stored address.
        let c = session("", CodecPreference::Auto).stream_config(&[H264]).unwrap();
        assert_eq!(
            (c.session_url.host.as_str(), c.session_url.port),
            ("192.168.1.20", 48010)
        );

        assert!(session("", CodecPreference::Auto).stream_config(&[HEVC10]).is_err());
    }
}
