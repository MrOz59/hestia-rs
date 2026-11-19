//! The GameStream HTTP control plane, as spoken by Sunshine, Apollo and Hermes:
//! host info, PIN/OTP pairing, the app list, box art and launching or quitting
//! a session.
//!
//! The crate is blocking on purpose. Every call is a short request on its own
//! connection; callers run them on a worker thread.

pub mod apps;
pub mod client;
pub mod codecs;
pub mod http;
pub mod launch;
pub mod pairing;
pub mod serverinfo;
pub mod tls;
pub mod url;
pub mod xml;

pub use apps::AppInfo;
pub use client::{DEFAULT_HTTP_PORT, DEFAULT_HTTPS_PORT, NvHttpClient};
pub use launch::{AudioConfig, LaunchRequest, LaunchResponse};
pub use pairing::{PairingOutcome, PairingPin};
pub use serverinfo::{DisplayMode, ServerInfo};

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum NvHttpError {
    #[error("network error: {0}")]
    Io(#[from] io::Error),
    #[error("TLS setup failed: {0}")]
    Tls(#[from] rustls::Error),
    #[error("the host presented a different certificate than the one it was paired with")]
    CertificateMismatch,
    #[error("TLS handshake failed: {0}")]
    TlsHandshake(String),
    #[error("this host has not been paired yet")]
    NotPaired,
    #[error(transparent)]
    Xml(#[from] xml::XmlError),
    #[error("host returned {code}: {message}")]
    Host { code: i64, message: String },
    #[error("unexpected HTTP status {0}")]
    HttpStatus(u16),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error(transparent)]
    Crypto(#[from] hestia_crypto::CryptoError),
}

impl NvHttpError {
    /// True for errors that mean "our pinned certificate is no longer accepted",
    /// either because the host rejected our client cert (401) or because it
    /// presented a different server cert.
    pub fn is_unauthorized(&self) -> bool {
        matches!(
            self,
            Self::CertificateMismatch | Self::TlsHandshake(_) | Self::Host { code: 401, .. } | Self::HttpStatus(401)
        )
    }
}

pub type Result<T, E = NvHttpError> = std::result::Result<T, E>;
