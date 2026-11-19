//! Client for the Hestia protocol v1, the optional API Hermes exposes next to
//! the standard GameStream endpoints (capabilities, session preparation,
//! diagnostics, clipboard, server commands...).
//!
//! Everything here is optional: callers should probe [`HestiaApi::capabilities`]
//! and fall back to plain GameStream behaviour when it fails. Endpoints other
//! than `capabilities` authenticate with the paired client certificate, so
//! they only work after pairing.

pub mod types;

use std::time::Duration;

use hestia_nvhttp::NvHttpClient;
use hestia_nvhttp::http::Request;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use types::*;

const BASE: &str = "/api/hestia/v1";
const CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(1);
/// Clipboard payload limit enforced by Hestia/Hermes.
pub const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Transport(#[from] hestia_nvhttp::NvHttpError),
    #[error("host answered {status} ({code}): {message}")]
    Host { status: u16, code: String, message: String },
    #[error("invalid JSON from host: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Rejected(String),
}

impl ApiError {
    /// The host doesn't implement the Hestia API (plain Sunshine/Apollo).
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Host { status: 404, .. })
    }

    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::Host { status: 403, .. })
    }
}

pub type Result<T, E = ApiError> = std::result::Result<T, E>;

/// Borrowing view over an [`NvHttpClient`]: same host, same identity, same
/// pinned certificate, but the web/API port.
pub struct HestiaApi<'a> {
    client: &'a NvHttpClient,
}

impl<'a> HestiaApi<'a> {
    pub fn new(client: &'a NvHttpClient) -> Self {
        Self { client }
    }

    fn call<T: DeserializeOwned>(&self, request: Request<'_>, timeout: Duration) -> Result<T> {
        let response = self.client.https_request(self.client.api_port(), &request, timeout)?;
        if !response.is_success() {
            let body: types::ErrorEnvelope = serde_json::from_slice(&response.body).unwrap_or_default();
            return Err(ApiError::Host {
                status: response.status,
                code: body.error.code,
                message: body.error.message,
            });
        }
        Ok(serde_json::from_slice(&response.body)?)
    }

    fn get<T: DeserializeOwned>(&self, path: &str, timeout: Duration) -> Result<T> {
        let target = format!("{BASE}{path}");
        self.call(Request::get(&target), timeout)
    }

    fn post<T: DeserializeOwned, B: Serialize>(&self, path: &str, body: &B, timeout: Duration) -> Result<T> {
        let target = format!("{BASE}{path}");
        let json = serde_json::to_vec(body)?;
        self.call(Request::post_json(&target, &json), timeout)
    }

    fn post_ok<B: Serialize>(&self, path: &str, body: &B, timeout: Duration) -> Result<()> {
        let value: serde_json::Value = self.post(path, body, timeout)?;
        ensure_ok(&value)
    }

    /// Probes the host. Plain GameStream hosts answer 404 here
    /// ([`ApiError::is_unsupported`]).
    pub fn capabilities(&self) -> Result<Capabilities> {
        self.get("/capabilities", CAPABILITIES_TIMEOUT)
    }

    /// Asks the host to set up the display/session before `/launch`.
    pub fn prepare_session(&self, request: &SessionPrepareRequest) -> Result<SessionPrepareResponse> {
        let response: SessionPrepareResponse = self.post("/session/prepare", request, DEFAULT_TIMEOUT)?;
        if !response.ok || response.session_id.is_empty() {
            return Err(ApiError::Rejected("session prepare was not accepted".into()));
        }
        Ok(response)
    }

    /// Clears this client's prepared session and stops the stream it owns.
    pub fn stop_session(&self) -> Result<()> {
        self.post_ok("/session/stop", &serde_json::json!({}), DEFAULT_TIMEOUT)
    }

    pub fn display_status(&self) -> Result<serde_json::Value> {
        self.get("/display/status", DEFAULT_TIMEOUT)
    }

    pub fn recover_display(&self, force: bool) -> Result<serde_json::Value> {
        self.post(
            "/display/recover",
            &serde_json::json!({ "force": force }),
            DEFAULT_TIMEOUT,
        )
    }

    pub fn client_permissions(&self) -> Result<ClientPermissions> {
        self.get("/client/permissions", DEFAULT_TIMEOUT)
    }

    /// Live runtime status, readiness preflight and optional dependencies.
    /// Kept as raw JSON: the document is large and still evolving.
    pub fn diagnostics(&self) -> Result<serde_json::Value> {
        self.get("/diagnostics", DEFAULT_TIMEOUT)
    }

    pub fn clipboard(&self) -> Result<String> {
        let value: serde_json::Value = self.get("/clipboard", CLIPBOARD_TIMEOUT)?;
        ensure_ok(&value)?;
        let parsed: types::ClipboardText = serde_json::from_value(value)?;
        parsed
            .text
            .ok_or_else(|| ApiError::Rejected("clipboard response has no text".into()))
    }

    pub fn set_clipboard(&self, text: &str) -> Result<()> {
        if text.contains('\0') || text.len() > MAX_CLIPBOARD_BYTES {
            return Err(ApiError::Rejected("clipboard text is too large or contains NUL".into()));
        }
        self.post_ok("/clipboard", &serde_json::json!({ "text": text }), CLIPBOARD_TIMEOUT)
    }

    pub fn commands(&self) -> Result<Vec<ServerCommand>> {
        let list: types::CommandList = self.get("/commands", DEFAULT_TIMEOUT)?;
        Ok(list.commands)
    }

    pub fn run_command(&self, id: u32) -> Result<()> {
        self.post_ok("/commands/run", &serde_json::json!({ "id": id }), DEFAULT_TIMEOUT)
    }
}

fn ensure_ok(value: &serde_json::Value) -> Result<()> {
    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(ApiError::Rejected("host did not report ok".into()))
    }
}
