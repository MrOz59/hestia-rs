//! PEM encoding. GameStream exchanges certificates as hex-encoded PEM text, so
//! byte-exact PEM output matters (the host stores and compares it).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Encodes DER as PEM with 64-column lines and a trailing newline, the layout
/// OpenSSL's `PEM_write_*` produces.
pub fn encode(label: &str, der: &[u8]) -> String {
    let b64 = STANDARD.encode(der);
    let mut out = String::with_capacity(b64.len() + b64.len() / 64 + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for line in b64.as_bytes().chunks(64) {
        // base64 output is ASCII.
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

/// Decodes the first PEM block with the given label.
pub fn decode(label: &str, pem: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = start + pem[start..].find(&end)?;
    let body: String = pem[start..stop].chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD.decode(body).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let der: Vec<u8> = (0..200u8).collect();
        let pem = super::encode("CERTIFICATE", &der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.ends_with("-----END CERTIFICATE-----\n"));
        assert!(pem.lines().all(|l| l.len() <= 64));
        assert_eq!(super::decode("CERTIFICATE", &pem).unwrap(), der);
        assert!(super::decode("PRIVATE KEY", &pem).is_none());
    }
}
