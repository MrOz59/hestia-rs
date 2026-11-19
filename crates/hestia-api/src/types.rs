//! JSON documents of the Hestia protocol v1, as served by Hermes under
//! `/api/hestia/v1/*`. Parsing is lenient (missing fields default, unknown
//! fields are ignored) so newer hosts keep working with older clients.

use serde::{Deserialize, Serialize};

/// Highest Hestia protocol version this client speaks.
pub const CLIENT_PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub ok: bool,
    pub server_name: String,
    /// Protocol lineage; stays `Apollo` for compatibility.
    pub base: String,
    pub hestia_protocol: u32,
    pub min_client_protocol: u32,
    pub max_client_protocol: u32,
    pub server_version: String,
    pub compatibility: Compatibility,
    pub features: Features,
    pub limits: Limits,
}

impl Capabilities {
    /// The host speaks a protocol range that includes ours.
    pub fn is_compatible(&self) -> bool {
        self.ok
            && self.hestia_protocol >= 1
            && self.min_client_protocol <= CLIENT_PROTOCOL
            && CLIENT_PROTOCOL <= self.max_client_protocol.max(self.min_client_protocol)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Compatibility {
    pub gamestream: bool,
    pub moonlight: bool,
    pub sunshine: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Features {
    pub virtual_display: bool,
    /// `evdi` and/or `hermes_kms`.
    pub virtual_display_backend: Vec<String>,
    pub hermes_kms_multi_output: Option<MultiOutput>,
    pub hermes_kms_isolated_sessions: Option<IsolatedSessions>,
    pub kde_kscreen: bool,
    pub display_recovery: bool,
    pub client_resolution_matching: bool,
    pub client_fps_matching: bool,
    pub hdr_mode_control: bool,
    pub scale_factor: bool,
    pub gamescope_session: bool,
    pub server_commands: bool,
    pub clipboard_sync: bool,
    pub permission_system: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MultiOutput {
    pub supported: bool,
    pub experimental: bool,
    pub max_outputs: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IsolatedSessions {
    pub status: String,
    pub experimental: bool,
    pub profiles: Vec<String>,
    pub max_sessions: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub supported_fps: Vec<u32>,
    /// `h264`, `hevc`, `av1`, `pyrowave`.
    pub supported_codecs: Vec<String>,
}

// ---------------------------------------------------------------------------
// Session preparation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPrepareRequest {
    pub client: ClientDescription,
    pub stream: StreamRequest,
    pub virtual_display: VirtualDisplayRequest,
    pub app: AppRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientDescription {
    pub name: String,
    pub version: String,
    /// `linux`, `android`, ...
    pub platform: String,
    pub display_width: u32,
    pub display_height: u32,
    pub refresh_rate: u32,
    pub hdr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRequest {
    pub requested_width: u32,
    pub requested_height: u32,
    pub requested_fps: u32,
    /// `h264`, `hevc`, `av1`.
    pub codec: String,
    pub bitrate_kbps: u32,
    /// `sdr` or `hdr`.
    pub hdr_mode: String,
    /// Percent; 100 when the host lacks the `scale_factor` feature.
    pub scale_factor: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualDisplayRequest {
    pub enabled: bool,
    /// `auto`, `evdi`, `hermes_kms`.
    pub backend: String,
    pub desktop_integration: String,
    pub recover_physical_monitor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRequest {
    /// App id as a string, as Hestia sends it.
    pub id: String,
    /// `normal` or `gamescope`.
    pub launch_mode: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionPrepareResponse {
    pub ok: bool,
    pub session_id: String,
    pub virtual_display: PreparedDisplay,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PreparedDisplay {
    pub created: bool,
    pub name: String,
    pub backend: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hdr: bool,
    pub kscreen_enabled: bool,
}

// ---------------------------------------------------------------------------
// Misc endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientPermissions {
    pub ok: bool,
    pub client_id: String,
    pub permissions: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionSet {
    pub view_streams: bool,
    pub list_apps: bool,
    pub launch_apps: bool,
    pub keyboard_input: bool,
    pub mouse_input: bool,
    pub gamepad_input: bool,
    pub virtual_display: bool,
    pub server_commands: bool,
    pub clipboard_sync: bool,
    pub display_recovery: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCommand {
    pub id: u32,
    pub name: String,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CommandList {
    pub commands: Vec<ServerCommand>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ClipboardText {
    pub text: Option<String>,
}

/// `{"ok": false, "error": {"code": "...", "message": "..."}}`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_parse_and_tolerate_unknown_fields() {
        let json = r#"{
            "ok": true, "server_name": "Hermes", "base": "Apollo", "hestia_protocol": 1,
            "min_client_protocol": 1, "max_client_protocol": 1, "server_version": "0.7.0",
            "compatibility": {"gamestream": true, "moonlight": true, "sunshine": true},
            "features": {"virtual_display": true, "virtual_display_backend": ["evdi", "hermes_kms"],
                "hermes_kms_multi_output": {"supported": true, "experimental": true, "max_outputs": 4},
                "clipboard_sync": true, "brand_new_feature": {"x": 1}},
            "limits": {"max_width": 7680, "max_height": 4320, "max_fps": 240,
                "supported_fps": [30, 60, 120], "supported_codecs": ["h264", "hevc", "pyrowave"]}
        }"#;
        let caps: Capabilities = serde_json::from_str(json).unwrap();
        assert!(caps.is_compatible());
        assert_eq!(caps.features.virtual_display_backend, ["evdi", "hermes_kms"]);
        assert_eq!(caps.features.hermes_kms_multi_output.unwrap().max_outputs, 4);
        assert!(caps.features.hermes_kms_isolated_sessions.is_none());
        assert_eq!(caps.limits.supported_codecs.len(), 3);
    }

    #[test]
    fn incompatible_protocol() {
        let caps: Capabilities = serde_json::from_str(
            r#"{"ok": true, "hestia_protocol": 2, "min_client_protocol": 2, "max_client_protocol": 2}"#,
        )
        .unwrap();
        assert!(!caps.is_compatible());
    }
}

// ---------------------------------------------------------------------------
// Diagnostics document (GET /diagnostics), the parts clients show
// ---------------------------------------------------------------------------

/// One readiness check from `diagnostics.preflight.checks`. The id set is
/// open: unknown ids are kept, unknown statuses count as warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreflightCheck {
    pub id: String,
    pub status: String,
    pub message: String,
}

impl PreflightCheck {
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    pub fn is_fail(&self) -> bool {
        self.status == "fail"
    }
}

/// `diagnostics.preflight`: is the host ready to stream, and why not.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preflight {
    pub ready: bool,
    pub checks: Vec<PreflightCheck>,
}

impl Preflight {
    /// Checks that are not "ok", failures first.
    pub fn problems(&self) -> Vec<&PreflightCheck> {
        let mut out: Vec<&PreflightCheck> = self.checks.iter().filter(|c| !c.is_ok() && !c.id.is_empty()).collect();
        out.sort_by_key(|c| !c.is_fail());
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EncoderStatus {
    /// False until Hermes has probed its encoders.
    pub probed: Option<bool>,
    pub name: String,
    pub hardware: bool,
    pub codecs: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsStatus {
    pub active: u32,
    pub streaming: bool,
    /// `client_quit`, `client_lost`, `handshake_failed`, `protocol_error`,
    /// `server_stopped`, `permission_revoked` or `unknown`.
    pub last_termination: String,
}

impl SessionsStatus {
    /// The last termination reason as a person reads it.
    pub fn last_termination_text(&self) -> Option<String> {
        let text = match self.last_termination.as_str() {
            "" | "unknown" => return None,
            "client_quit" => "the client closed the stream",
            "client_lost" => "the client connection was lost (network or crash)",
            "handshake_failed" => "the stream handshake failed",
            "protocol_error" => "a protocol error",
            "server_stopped" => "stopped by the host",
            "permission_revoked" => "permission revoked",
            other => other,
        };
        Some(text.to_owned())
    }
}

/// Live encoder metrics while a stream runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineStatus {
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub encode_ms: f64,
    pub capture_to_encode_ms: f64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeStatus {
    pub encoder: Option<EncoderStatus>,
    pub sessions: Option<SessionsStatus>,
    pub pipeline: Option<PipelineStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DependencyStatus {
    pub available: bool,
    pub diagnostic: String,
    #[serde(rename = "manualInstall")]
    pub manual_install: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dependencies {
    pub clipboard: Option<DependencyStatus>,
}

/// The parts of `GET /diagnostics` that clients present. Every section is
/// optional: older hosts omit some of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostDiagnostics {
    pub ok: bool,
    pub preflight: Option<Preflight>,
    pub runtime: Option<RuntimeStatus>,
    pub dependencies: Option<Dependencies>,
}

impl HostDiagnostics {
    /// Lenient: a section that doesn't parse is treated as absent.
    pub fn from_value(value: &serde_json::Value) -> Self {
        let section = |key: &str| value.get(key).cloned().unwrap_or(serde_json::Value::Null);
        Self {
            ok: value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false),
            preflight: serde_json::from_value(section("preflight")).ok(),
            runtime: serde_json::from_value(section("runtime")).ok(),
            dependencies: serde_json::from_value(section("dependencies")).ok(),
        }
    }
}

// ---------------------------------------------------------------------------
// Stream provenance (GET /stream/info)
// ---------------------------------------------------------------------------

/// How Hermes serves the current stream, from its capture-path check just
/// before the stream started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamReadiness {
    pub ok: bool,
    pub frame_seen: bool,
    /// e.g. "the detached desktop via KWin screencast".
    pub capture: String,
    /// e.g. "vaapi", "nvenc".
    pub encoder: String,
    /// e.g. "/dev/dri/renderD128 (amdgpu, AMD 1002:7550)"; empty for a
    /// software encoder.
    pub gpu: String,
    pub failure: String,
}

impl StreamReadiness {
    /// "AMD 1002:7550 (amdgpu)" from "/dev/dri/renderD128 (amdgpu, AMD 1002:7550)".
    pub fn gpu_name(&self) -> String {
        let gpu = &self.gpu;
        if let (Some(open), true) = (gpu.find(" ("), gpu.ends_with(')')) {
            if let Some(comma) = gpu[open..].find(", ").map(|i| open + i) {
                let driver = &gpu[open + 2..comma];
                let device = &gpu[comma + 2..gpu.len() - 1];
                return format!("{device} ({driver})");
            }
        }
        gpu.clone()
    }

    pub fn encoder_name(&self) -> String {
        self.encoder.to_uppercase()
    }

    /// "Detached desktop via KWin screencast".
    pub fn capture_text(&self) -> String {
        let text = self.capture.strip_prefix("the ").unwrap_or(&self.capture);
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct StreamInfoEnvelope {
    pub readiness: Option<StreamReadiness>,
}

/// Size cap for `GET /diagnostics/report` (as Hestia desktop).
pub const MAX_DIAGNOSTIC_REPORT_BYTES: usize = 4 * 1024 * 1024;

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_diagnostics_sections() {
        let v = json!({
            "ok": true,
            "preflight": {"ready": false, "checks": [
                {"id": "kms", "status": "ok", "message": ""},
                {"id": "clipboard", "status": "warn", "message": "wl-clipboard missing"},
                {"id": "encoder", "status": "fail", "message": "no VAAPI device"},
                {"status": "fail", "message": "no id: ignored"}
            ]},
            "runtime": {
                "encoder": {"probed": true, "name": "vaapi", "hardware": true, "codecs": ["h264", "hevc"]},
                "sessions": {"active": 0, "streaming": false, "last_termination": "client_lost"},
                "pipeline": {"fps": 59.9, "bitrate_kbps": 18000, "encode_ms": 3.2,
                             "capture_to_encode_ms": 5.1, "frames_encoded": 900, "frames_dropped": 2}
            },
            "dependencies": {"clipboard": {"available": false, "diagnostic": "wl-copy not found",
                                           "manualInstall": "sudo pacman -S wl-clipboard"}},
            "preflight_extra": "ignored"
        });
        let d = HostDiagnostics::from_value(&v);
        assert!(d.ok);
        let pf = d.preflight.unwrap();
        assert!(!pf.ready);
        let problems: Vec<&str> = pf.problems().iter().map(|c| c.id.as_str()).collect();
        assert_eq!(problems, ["encoder", "clipboard"], "failures first, nameless ignored");
        let rt = d.runtime.unwrap();
        assert_eq!(rt.encoder.unwrap().codecs, ["h264", "hevc"]);
        assert_eq!(
            rt.sessions.unwrap().last_termination_text().as_deref(),
            Some("the client connection was lost (network or crash)")
        );
        assert_eq!(rt.pipeline.unwrap().frames_dropped, 2);
        assert_eq!(
            d.dependencies.unwrap().clipboard.unwrap().manual_install,
            "sudo pacman -S wl-clipboard"
        );
    }

    #[test]
    fn missing_or_broken_sections_are_absent() {
        let d = HostDiagnostics::from_value(&json!({"ok": true, "runtime": "nonsense"}));
        assert!(d.preflight.is_none() && d.runtime.is_none() && d.dependencies.is_none());
    }

    #[test]
    fn readiness_texts() {
        let r = StreamReadiness {
            ok: true,
            capture: "the detached desktop via KWin screencast".into(),
            encoder: "vaapi".into(),
            gpu: "/dev/dri/renderD128 (amdgpu, AMD 1002:7550)".into(),
            ..Default::default()
        };
        assert_eq!(r.gpu_name(), "AMD 1002:7550 (amdgpu)");
        assert_eq!(r.encoder_name(), "VAAPI");
        assert_eq!(r.capture_text(), "Detached desktop via KWin screencast");
        let odd = StreamReadiness {
            gpu: "Intel Arc".into(),
            ..Default::default()
        };
        assert_eq!(odd.gpu_name(), "Intel Arc");
    }
}
