//! The client's long-lived identity: an RSA-2048 key pair, a self-signed
//! certificate and a unique id. The host remembers the certificate at pairing
//! time and authenticates every later HTTPS request with it.

use rand::RngCore;
use rand::rngs::OsRng;
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
use rsa::signature::{SignatureEncoding, Signer};
use sha2::Sha256;

use crate::{CryptoError, der, hex, pem};

/// Common name Moonlight clients use; hosts don't check it, but keeping it
/// identical avoids surprises with picky host software.
const CERT_COMMON_NAME: &str = "NVIDIA GameStream Client";
const CERT_VALIDITY_SECS: i64 = 60 * 60 * 24 * 365 * 20;

pub struct ClientIdentity {
    unique_id: String,
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    key: RsaPrivateKey,
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("unique_id", &self.unique_id)
            .finish_non_exhaustive()
    }
}

impl ClientIdentity {
    /// Generates a fresh key pair and self-signed certificate. RSA key
    /// generation takes a noticeable moment; call it off the UI thread.
    pub fn generate() -> Result<Self, CryptoError> {
        let key = RsaPrivateKey::new(&mut OsRng, 2048)?;
        let spki = key
            .to_public_key()
            .to_public_key_der()
            .map_err(|e| CryptoError::Encoding(e.to_string()))?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut serial = [0u8; 8];
        OsRng.fill_bytes(&mut serial);
        serial[0] &= 0x7f;

        let sha256_with_rsa = der::sequence(&[&der::oid(&[1, 2, 840, 113549, 1, 1, 11]), &der::null()]);
        let name = der::sequence(&[&der::set(&[&der::sequence(&[
            &der::oid(&[2, 5, 4, 3]),
            &der::utf8_string(CERT_COMMON_NAME),
        ])])]);
        let validity = der::sequence(&[&der::x509_time(now), &der::x509_time(now + CERT_VALIDITY_SECS)]);
        let tbs = der::sequence(&[
            &der::explicit(0, &der::integer(&[2])), // v3
            &der::integer(&serial),
            &sha256_with_rsa,
            &name,
            &validity,
            &name,
            spki.as_bytes(),
        ]);

        let signature = SigningKey::<Sha256>::new(key.clone()).sign(&tbs).to_vec();
        let cert_der = der::sequence(&[&tbs, &sha256_with_rsa, &der::bit_string(&signature)]);

        let key_der = key
            .to_pkcs8_der()
            .map_err(|e| CryptoError::Encoding(e.to_string()))?
            .as_bytes()
            .to_vec();

        let mut id = [0u8; 8];
        OsRng.fill_bytes(&mut id);

        Ok(Self {
            unique_id: hex::encode_upper(&id),
            cert_der,
            key_der,
            key,
        })
    }

    /// Restores an identity saved with [`cert_pem`](Self::cert_pem) and
    /// [`key_pem`](Self::key_pem).
    pub fn from_pem(unique_id: &str, cert_pem: &str, key_pem: &str) -> Result<Self, CryptoError> {
        let cert_der = pem::decode("CERTIFICATE", cert_pem).ok_or(CryptoError::InvalidPem("CERTIFICATE"))?;
        let key_der = pem::decode("PRIVATE KEY", key_pem).ok_or(CryptoError::InvalidPem("PRIVATE KEY"))?;
        der::parse_certificate(&cert_der)?;
        let key = RsaPrivateKey::from_pkcs8_der(&key_der).map_err(|e| CryptoError::Encoding(e.to_string()))?;
        Ok(Self {
            unique_id: unique_id.to_owned(),
            cert_der,
            key_der,
            key,
        })
    }

    pub fn unique_id(&self) -> &str {
        &self.unique_id
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    /// PKCS#8 DER of the private key (what TLS stacks want for client auth).
    pub fn key_der(&self) -> &[u8] {
        &self.key_der
    }

    pub fn cert_pem(&self) -> String {
        pem::encode("CERTIFICATE", &self.cert_der)
    }

    pub fn key_pem(&self) -> String {
        pem::encode("PRIVATE KEY", &self.key_der)
    }

    /// Raw signature bytes of our certificate, used in the pairing challenge.
    pub fn cert_signature(&self) -> Vec<u8> {
        der::parse_certificate(&self.cert_der)
            .map(|p| p.signature.to_vec())
            .unwrap_or_default()
    }

    /// RSA PKCS#1 v1.5 signature with SHA-256.
    pub fn sign_sha256(&self, message: &[u8]) -> Vec<u8> {
        SigningKey::<Sha256>::new(self.key.clone()).sign(message).to_vec()
    }
}
