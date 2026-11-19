//! Per-host GameStream HTTP client.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hestia_crypto::{ClientIdentity, hex, pairing::random_bytes};
use log::{debug, warn};
use rustls::ClientConfig;
use rustls::pki_types::ServerName;

use crate::apps::{AppInfo, parse_app_list};
use crate::http::{self, Request, Response};
use crate::launch::{LaunchRequest, LaunchResponse};
use crate::serverinfo::ServerInfo;
use crate::xml::{self, Element};
use crate::{NvHttpError, Result, tls, url};

pub const DEFAULT_HTTP_PORT: u16 = 47989;
pub const DEFAULT_HTTPS_PORT: u16 = 47984;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct NvHttpClient {
    host: String,
    http_port: u16,
    https_port: u16,
    identity: Arc<ClientIdentity>,
    server_cert: Option<Vec<u8>>,
    tls: OnceLock<Arc<ClientConfig>>,
    timeout: Duration,
}

impl std::fmt::Debug for NvHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvHttpClient")
            .field("host", &self.host)
            .field("http_port", &self.http_port)
            .field("https_port", &self.https_port)
            .field("pinned", &self.server_cert.is_some())
            .finish()
    }
}

impl NvHttpClient {
    pub fn new(host: impl Into<String>, http_port: u16, identity: Arc<ClientIdentity>) -> Self {
        Self {
            host: host.into(),
            http_port,
            https_port: DEFAULT_HTTPS_PORT,
            identity,
            server_cert: None,
            tls: OnceLock::new(),
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// Pins the host certificate obtained at pairing time.
    pub fn with_server_cert(mut self, der: Option<Vec<u8>>) -> Self {
        self.set_server_cert(der);
        self
    }

    pub fn with_https_port(mut self, port: u16) -> Self {
        self.https_port = port;
        self
    }

    pub fn set_server_cert(&mut self, der: Option<Vec<u8>>) {
        self.server_cert = der;
        self.tls = OnceLock::new();
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn http_port(&self) -> u16 {
        self.http_port
    }

    pub fn https_port(&self) -> u16 {
        self.https_port
    }

    /// Port of the web UI / Hestia API server (HTTP port + 1 by convention).
    pub fn api_port(&self) -> u16 {
        self.http_port.wrapping_add(1)
    }

    pub fn server_cert(&self) -> Option<&[u8]> {
        self.server_cert.as_deref()
    }

    pub fn identity(&self) -> &ClientIdentity {
        &self.identity
    }

    pub fn is_pinned(&self) -> bool {
        self.server_cert.is_some()
    }

    // ------------------------------------------------------------------
    // Transport
    // ------------------------------------------------------------------

    pub(crate) fn target(&self, command: &str, args: &[(&str, String)]) -> String {
        let mut pairs = vec![
            ("uniqueid", self.identity.unique_id().to_owned()),
            ("uuid", hex::encode(&random_bytes::<16>())),
        ];
        pairs.extend(args.iter().cloned());
        format!("/{command}?{}", url::query(&pairs))
    }

    fn tls_config(&self) -> Result<Arc<ClientConfig>> {
        if let Some(cfg) = self.tls.get() {
            return Ok(cfg.clone());
        }
        let cert = self.server_cert.as_deref().ok_or(NvHttpError::NotPaired)?;
        let cfg = tls::client_config(&self.identity, cert)?;
        Ok(self.tls.get_or_init(|| cfg).clone())
    }

    pub fn http_request(&self, request: &Request<'_>, timeout: Duration) -> Result<Response> {
        let mut stream = http::connect(&self.host, self.http_port, self.timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        Ok(http::exchange(
            &mut stream,
            &http::authority(&self.host, self.http_port),
            request,
        )?)
    }

    /// HTTPS request to `port` with our client certificate and the pinned host cert.
    pub fn https_request(&self, port: u16, request: &Request<'_>, timeout: Duration) -> Result<Response> {
        let config = self.tls_config()?;
        let bare_host = self.host.trim_start_matches('[').trim_end_matches(']');
        let server_name = ServerName::try_from(bare_host.to_owned())
            .map_err(|_| NvHttpError::InvalidResponse(format!("invalid host name {bare_host}")))?;
        let conn = rustls::ClientConnection::new(config, server_name)?;
        let tcp = http::connect(&self.host, port, self.timeout)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(self.timeout))?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        http::exchange(&mut stream, &http::authority(&self.host, port), request).map_err(|e| {
            if tls::is_certificate_rejection(&e) {
                NvHttpError::CertificateMismatch
            } else if let Some(tls_err) = e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                NvHttpError::TlsHandshake(tls_err.to_string())
            } else {
                NvHttpError::Io(e)
            }
        })
    }

    fn xml_from(response: Response) -> Result<Element> {
        let text = response.text();
        let root = match xml::parse(&text) {
            Ok(root) => root,
            Err(e) if !response.is_success() => {
                debug!("non-XML error body ({e}): {text}");
                return Err(NvHttpError::HttpStatus(response.status));
            }
            Err(e) => return Err(e.into()),
        };
        verify_status(&root)?;
        Ok(root)
    }

    pub fn get_http_xml(&self, command: &str, args: &[(&str, String)], timeout: Duration) -> Result<Element> {
        let target = self.target(command, args);
        Self::xml_from(self.http_request(&Request::get(&target), timeout)?)
    }

    pub fn get_https_xml(&self, command: &str, args: &[(&str, String)], timeout: Duration) -> Result<Element> {
        let target = self.target(command, args);
        Self::xml_from(self.https_request(self.https_port, &Request::get(&target), timeout)?)
    }

    // ------------------------------------------------------------------
    // API
    // ------------------------------------------------------------------

    /// Fetches `/serverinfo`, over HTTPS when we are paired (it is the only way
    /// to learn the real pair status) and falling back to HTTP when the host no
    /// longer accepts our certificate.
    pub fn server_info(&mut self) -> Result<ServerInfo> {
        let mut secure_error = None;
        if self.is_pinned() {
            match self.get_https_xml("serverinfo", &[], self.timeout) {
                Ok(root) => return Ok(ServerInfo::from_xml(&root)),
                Err(e) if e.is_unauthorized() => {
                    warn!("{} rejected our certificate ({e}); falling back to HTTP", self.host);
                }
                Err(NvHttpError::Io(e)) => {
                    // Not a verdict on the pairing: the HTTPS port didn't
                    // answer (host busy with a launch, firewall…).
                    warn!("HTTPS serverinfo on {} failed ({e}); falling back to HTTP", self.host);
                    secure_error = Some(e.to_string());
                }
                Err(e) => return Err(e),
            }
        }

        let root = self.get_http_xml("serverinfo", &[], self.timeout)?;
        let mut info = ServerInfo::from_xml(&root);
        info.secure_error = secure_error;
        let https_port = if info.https_port == 0 {
            DEFAULT_HTTPS_PORT
        } else {
            info.https_port
        };
        if https_port != self.https_port {
            self.https_port = https_port;
            if self.is_pinned() {
                // The HTTPS attempt above used a stale port; try once more.
                if let Ok(root) = self.get_https_xml("serverinfo", &[], self.timeout) {
                    return Ok(ServerInfo::from_xml(&root));
                }
            }
        }
        Ok(info)
    }

    pub fn app_list(&self) -> Result<Vec<AppInfo>> {
        let root = self.get_https_xml("applist", &[], self.timeout)?;
        Ok(parse_app_list(&root))
    }

    /// Box art as encoded image bytes (usually PNG).
    pub fn box_art(&self, app_id: u32) -> Result<Vec<u8>> {
        let target = self.target(
            "appasset",
            &[
                ("appid", app_id.to_string()),
                ("AssetType", "2".into()),
                ("AssetIdx", "0".into()),
            ],
        );
        let response = self.https_request(self.https_port, &Request::get(&target), self.timeout)?;
        if !response.is_success() {
            return Err(NvHttpError::HttpStatus(response.status));
        }
        let looks_like_xml = response.body.starts_with(b"<?xml") || response.body.starts_with(b"<root");
        if looks_like_xml {
            // Error responses come back as XML with a status code.
            Self::xml_from(response)?;
            return Err(NvHttpError::InvalidResponse("expected an image".into()));
        }
        Ok(response.body)
    }

    pub fn launch(&self, request: &LaunchRequest) -> Result<LaunchResponse> {
        self.start("launch", request)
    }

    pub fn resume(&self, request: &LaunchRequest) -> Result<LaunchResponse> {
        self.start("resume", request)
    }

    fn start(&self, verb: &str, request: &LaunchRequest) -> Result<LaunchResponse> {
        let root = self.get_https_xml(verb, &request.query_pairs(), LAUNCH_TIMEOUT)?;
        let session_url = root.child_text("sessionUrl0").unwrap_or_default().to_owned();
        let ok = root.child_text("gamesession") == Some("1") || root.child_text("resume") == Some("1");
        if !ok && session_url.is_empty() {
            return Err(NvHttpError::InvalidResponse(format!("{verb} did not start a session")));
        }
        Ok(LaunchResponse {
            session_url,
            resumed: verb == "resume",
        })
    }

    /// Quits the running app. Only the client that started it may do this on
    /// most hosts.
    pub fn quit_app(&self) -> Result<()> {
        self.get_https_xml("cancel", &[], QUIT_TIMEOUT)?;
        Ok(())
    }
}

/// Checks `<root status_code="...">`. Hosts occasionally send 0xFFFFFFFF, so
/// the code is parsed unsigned and reinterpreted.
pub fn verify_status(root: &Element) -> Result<()> {
    if root.name != "root" {
        return Err(NvHttpError::InvalidResponse(format!("unexpected root <{}>", root.name)));
    }
    let code = root
        .attribute("status_code")
        .and_then(|c| c.trim().parse::<i64>().ok())
        .map(|c| if c > i32::MAX as i64 { c as u32 as i32 as i64 } else { c })
        .ok_or_else(|| NvHttpError::InvalidResponse("missing status_code".into()))?;
    if code == 200 {
        Ok(())
    } else {
        Err(NvHttpError::Host {
            code,
            message: root.attribute("status_message").unwrap_or_default().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes() {
        let ok = xml::parse("<root status_code=\"200\"/>").unwrap();
        assert!(verify_status(&ok).is_ok());
        let unauthorized = xml::parse("<root status_code=\"401\" status_message=\"no\"/>").unwrap();
        let err = verify_status(&unauthorized).unwrap_err();
        assert!(err.is_unauthorized());
        let weird = xml::parse("<root status_code=\"4294967295\"/>").unwrap();
        assert!(matches!(verify_status(&weird), Err(NvHttpError::Host { code: -1, .. })));
    }
}
