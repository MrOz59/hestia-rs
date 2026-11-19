//! Cryptography for Hestia RS: the client identity (RSA key + self-signed
//! certificate) and the primitives of the GameStream pairing handshake.
//!
//! Everything here is pure Rust so the same code builds for Linux and Android
//! without dragging OpenSSL along.

pub mod der;
pub mod hex;
pub mod identity;
pub mod pairing;
pub mod pem;

pub use identity::ClientIdentity;
pub use pairing::PairingHash;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("RSA error: {0}")]
    Rsa(#[from] rsa::Error),
    #[error("key encoding error: {0}")]
    Encoding(String),
    #[error(transparent)]
    Der(#[from] der::DerError),
    #[error("invalid PEM: expected a {0} block")]
    InvalidPem(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip_and_signatures() {
        let id = ClientIdentity::generate().expect("generate identity");
        assert_eq!(id.unique_id().len(), 16);

        let parts = der::parse_certificate(id.cert_der()).expect("parse our certificate");
        assert_eq!(parts.signature.len(), 256);
        assert_eq!(id.cert_signature(), parts.signature);

        // Our certificate is self-signed: the signature must verify against itself.
        assert!(pairing::verify_sha256(id.cert_der(), parts.tbs, parts.signature).unwrap());

        let message = b"pairing secret";
        let sig = id.sign_sha256(message);
        assert!(pairing::verify_sha256(id.cert_der(), message, &sig).unwrap());
        assert!(!pairing::verify_sha256(id.cert_der(), b"tampered", &sig).unwrap());

        let restored = ClientIdentity::from_pem(id.unique_id(), &id.cert_pem(), &id.key_pem()).unwrap();
        assert_eq!(restored.cert_der(), id.cert_der());
        assert_eq!(restored.sign_sha256(message), sig);
    }
}
