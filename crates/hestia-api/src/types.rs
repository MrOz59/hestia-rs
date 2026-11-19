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
