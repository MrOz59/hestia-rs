//! RTSP messages as GameStream hosts use them, plus the AES-GCM framing of
//! encrypted RTSP (`rtspenc://`, negotiated with `corever=1` at launch).
//!
//! The handshake itself (OPTIONS → DESCRIBE → SETUP ×3 → ANNOUNCE → PLAY) is
//! the next milestone; this module provides the wire format it will use.

use std::fmt;

use aes_gcm::aead::AeadInPlace;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::{Aes128Gcm, KeyInit};

pub const DEFAULT_RTSP_PORT: u16 = 48010;
/// Highest bit of the encrypted frame header's length word.
const ENCRYPTED_BIT: u32 = 0x8000_0000;
const ENC_HEADER_LEN: usize = 4 + 4 + 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtspError {
    Malformed(&'static str),
    Crypto,
}

impl fmt::Display for RtspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed RTSP message: {what}"),
            Self::Crypto => f.write_str("RTSP decryption failed"),
        }
    }
}

impl std::error::Error for RtspError {}

/// Where to connect, parsed from the `sessionUrl0` of `/launch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUrl {
    pub host: String,
    pub port: u16,
    pub encrypted: bool,
}

impl SessionUrl {
    /// Parses `rtsp://host:port` / `rtspenc://host:port`. An empty URL (very
    /// old hosts) falls back to the given host and the default port.
    pub fn parse(url: &str, fallback_host: &str) -> Result<Self, RtspError> {
        let url = url.trim();
        if url.is_empty() {
            return Ok(Self {
                host: fallback_host.to_owned(),
                port: DEFAULT_RTSP_PORT,
                encrypted: false,
            });
        }
        let (encrypted, rest) = if let Some(rest) = url.strip_prefix("rtspenc://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("rtsp://") {
            (false, rest)
        } else {
            return Err(RtspError::Malformed("unknown session URL scheme"));
        };
        let rest = rest.split('/').next().unwrap_or_default();
        let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or(RtspError::Malformed("bad IPv6 literal"))?;
            (h, after.strip_prefix(':'))
        } else if let Some((h, p)) = rest.rsplit_once(':') {
            (h, Some(p))
        } else {
            (rest, None)
        };
        let port = match port {
            Some(p) => p.parse().map_err(|_| RtspError::Malformed("bad port"))?,
            None => DEFAULT_RTSP_PORT,
        };
        let host = if host.is_empty() || host == "0.0.0.0" {
            fallback_host
        } else {
            host
        };
        Ok(Self {
            host: host.to_owned(),
            port,
            encrypted,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    /// `Some((method, target))` for requests, `None` for responses.
    pub request_line: Option<(String, String)>,
    /// Status code for responses.
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn request(method: &str, target: &str) -> Self {
        Self {
            request_line: Some((method.to_owned(), target.to_owned())),
            ..Default::default()
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.push(("Content-type".into(), content_type.to_owned()));
        self.headers.push(("Content-length".into(), body.len().to_string()));
        self.body = body;
        self
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = String::new();
        match &self.request_line {
            Some((method, target)) => out.push_str(&format!("{method} {target} RTSP/1.0\r\n")),
            None => out.push_str(&format!("RTSP/1.0 {} {}\r\n", self.status, self.reason)),
        }
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        bytes
    }

    pub fn parse(data: &[u8]) -> Result<Self, RtspError> {
        let split = data
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|i| (i, i + 4))
            .or_else(|| data.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2)))
            .ok_or(RtspError::Malformed("missing header terminator"))?;
        let head = std::str::from_utf8(&data[..split.0]).map_err(|_| RtspError::Malformed("non UTF-8 header"))?;
        let mut lines = head.lines();
        let first = lines.next().ok_or(RtspError::Malformed("empty message"))?;
        let mut msg = Message::default();
        if let Some(rest) = first.strip_prefix("RTSP/1.0 ") {
            let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
            msg.status = code
                .trim()
                .parse()
                .map_err(|_| RtspError::Malformed("bad status code"))?;
            msg.reason = reason.trim().to_owned();
        } else {
            let mut parts = first.split_whitespace();
            let method = parts.next().ok_or(RtspError::Malformed("missing method"))?;
            let target = parts.next().ok_or(RtspError::Malformed("missing target"))?;
            msg.request_line = Some((method.to_owned(), target.to_owned()));
        }
        msg.headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let body = &data[split.1..];
        let len = msg
            .get("Content-length")
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(body.len())
            .min(body.len());
        msg.body = body[..len].to_vec();
        Ok(msg)
    }
}

/// AES-128-GCM sealing of RTSP messages. Client→host frames use IV bytes
/// `seq(le32) .. 'C' 'R'`, host→client frames `'H' 'R'`.
pub struct RtspCipher {
    cipher: Aes128Gcm,
    next_seq: u32,
}

impl RtspCipher {
    /// `key` is the `rikey` sent with `/launch`.
    pub fn new(key: &[u8; 16]) -> Self {
        Self {
            cipher: Aes128Gcm::new(GenericArray::from_slice(key)),
            next_seq: 0,
        }
    }

    fn iv(seq: u32, origin: u8) -> [u8; 12] {
        let mut iv = [0u8; 12];
        iv[..4].copy_from_slice(&seq.to_le_bytes());
        iv[10] = origin;
        iv[11] = b'R';
        iv
    }

    /// Encrypts a serialized client message into a framed packet.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, RtspError> {
        self.next_seq = self.next_seq.wrapping_add(1);
        self.seal_with(plaintext, self.next_seq, b'C')
    }

    fn seal_with(&self, plaintext: &[u8], seq: u32, origin: u8) -> Result<Vec<u8>, RtspError> {
        let len = u32::try_from(plaintext.len())
            .ok()
            .filter(|l| l & ENCRYPTED_BIT == 0)
            .ok_or(RtspError::Malformed("message too large"))?;
        let mut payload = plaintext.to_vec();
        let iv = Self::iv(seq, origin);
        let tag = self
            .cipher
            .encrypt_in_place_detached(GenericArray::from_slice(&iv), b"", &mut payload)
            .map_err(|_| RtspError::Crypto)?;
        let mut out = Vec::with_capacity(ENC_HEADER_LEN + payload.len());
        out.extend_from_slice(&(ENCRYPTED_BIT | len).to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&payload);
        Ok(out)
    }

    /// Decrypts a host frame. Returns `Ok(None)` if more bytes are needed.
    pub fn unseal(&self, frame: &[u8]) -> Result<Option<Vec<u8>>, RtspError> {
        self.unseal_with(frame, b'H')
    }

    fn unseal_with(&self, frame: &[u8], origin: u8) -> Result<Option<Vec<u8>>, RtspError> {
        if frame.len() < ENC_HEADER_LEN {
            return Ok(None);
        }
        let type_and_len = u32::from_be_bytes(frame[0..4].try_into().unwrap_or_default());
        if type_and_len & ENCRYPTED_BIT == 0 {
            return Err(RtspError::Malformed("expected an encrypted frame"));
        }
        let len = (type_and_len & !ENCRYPTED_BIT) as usize;
        if frame.len() < ENC_HEADER_LEN + len {
            return Ok(None);
        }
        let seq = u32::from_be_bytes(frame[4..8].try_into().unwrap_or_default());
        let tag = GenericArray::clone_from_slice(&frame[8..24]);
        let mut payload = frame[ENC_HEADER_LEN..ENC_HEADER_LEN + len].to_vec();
        self.cipher
            .decrypt_in_place_detached(
                GenericArray::from_slice(&Self::iv(seq, origin)),
                b"",
                &mut payload,
                &tag,
            )
            .map_err(|_| RtspError::Crypto)?;
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_urls() {
        let u = SessionUrl::parse("rtspenc://192.168.0.10:48010", "x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.encrypted), ("192.168.0.10", 48010, true));
        let u = SessionUrl::parse("rtsp://[fe80::2]:48011", "x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.encrypted), ("fe80::2", 48011, false));
        let u = SessionUrl::parse("", "pc").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("pc", DEFAULT_RTSP_PORT));
        assert!(SessionUrl::parse("http://x", "x").is_err());
    }

    #[test]
    fn message_roundtrip() {
        let req = Message::request("ANNOUNCE", "streamid=control/13/0")
            .header("CSeq", "6")
            .header("X-GS-ClientVersion", "14")
            .with_body("application/sdp", b"v=0\r\n".to_vec());
        let parsed = Message::parse(&req.serialize()).unwrap();
        assert_eq!(parsed, req);

        let resp = Message::parse(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nSession: DEADBEEF;timeout = 90\r\n\r\n").unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.get("session"), Some("DEADBEEF;timeout = 90"));
        assert!(resp.request_line.is_none());
    }

    #[test]
    fn encrypted_framing() {
        let key = [3u8; 16];
        let mut client = RtspCipher::new(&key);
        let sealed = client.seal(b"OPTIONS rtsp://x RTSP/1.0\r\n\r\n").unwrap();
        assert_eq!(sealed[0] & 0x80, 0x80);
        assert_eq!(&sealed[4..8], &1u32.to_be_bytes());

        // A host frame (origin 'H') decrypts; our own 'C' frame must not.
        let host = RtspCipher::new(&key);
        let from_host = host.seal_with(b"RTSP/1.0 200 OK\r\n\r\n", 7, b'H').unwrap();
        assert_eq!(client.unseal(&from_host).unwrap().unwrap(), b"RTSP/1.0 200 OK\r\n\r\n");
        assert_eq!(client.unseal(&sealed), Err(RtspError::Crypto));
        assert_eq!(client.unseal(&from_host[..10]).unwrap(), None);
    }
}
