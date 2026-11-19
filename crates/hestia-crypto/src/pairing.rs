//! Primitives of the GameStream PIN pairing handshake. The protocol flow lives
//! in `hestia-nvhttp`; this module only holds the crypto it needs.

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use rand::RngCore;
use rand::rngs::OsRng;
use rsa::RsaPublicKey;
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use sha2::{Digest, Sha256};

use crate::{CryptoError, der, hex};

/// Hash used for the challenge exchange. Hosts reporting a major app version of
/// 7 or newer (every Sunshine/Apollo/Hermes build) use SHA-256; older GFE used SHA-1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingHash {
    Sha1,
    Sha256,
}

impl PairingHash {
    pub fn for_server_major_version(major: i32) -> Self {
        if major >= 7 { Self::Sha256 } else { Self::Sha1 }
    }

    pub fn len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Self::Sha256 => Sha256::digest(data).to_vec(),
        }
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

/// AES key for the handshake: first 16 bytes of hash(salt || pin).
pub fn derive_aes_key(salt: &[u8], pin: &str, hash: PairingHash) -> [u8; 16] {
    let mut salted = salt.to_vec();
    salted.extend_from_slice(pin.as_bytes());
    let digest = hash.digest(&salted);
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

/// AES-128-ECB without padding. Input shorter than a block multiple is
/// zero-padded, which is what the handshake does with its 32-byte hash buffer.
pub fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = data.to_vec();
    buf.resize(data.len().div_ceil(16) * 16, 0);
    for block in buf.chunks_exact_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    buf
}

/// AES-128-ECB decryption; trailing bytes that don't fill a block are dropped.
pub fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut buf = data[..data.len() / 16 * 16].to_vec();
    for block in buf.chunks_exact_mut(16) {
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
    }
    buf
}

/// Raw signature bytes of a DER certificate.
pub fn cert_signature(cert_der: &[u8]) -> Result<Vec<u8>, CryptoError> {
    Ok(der::parse_certificate(cert_der)?.signature.to_vec())
}

/// Verifies an RSA PKCS#1 v1.5 / SHA-256 signature against the public key in
/// `cert_der`.
pub fn verify_sha256(cert_der: &[u8], data: &[u8], signature: &[u8]) -> Result<bool, CryptoError> {
    let parts = der::parse_certificate(cert_der)?;
    let public_key = RsaPublicKey::from_public_key_der(parts.spki).map_err(|e| CryptoError::Encoding(e.to_string()))?;
    let Ok(signature) = Signature::try_from(signature) else {
        return Ok(false);
    };
    Ok(VerifyingKey::<Sha256>::new(public_key).verify(data, &signature).is_ok())
}

/// Proof for host-initiated OTP pairing, as Apollo/Hermes expect it:
/// uppercase hex of SHA-256(pin || salt_hex || passphrase), where `salt_hex` is
/// the exact lowercase salt string sent in the request.
pub fn otp_auth(pin: &str, salt_hex: &str, passphrase: &str) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(pin.as_bytes());
    input.extend_from_slice(salt_hex.as_bytes());
    input.extend_from_slice(passphrase.as_bytes());
    hex::encode_upper(&Sha256::digest(&input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecb_roundtrip() {
        let key = [7u8; 16];
        let data = [1u8; 32];
        let enc = aes_ecb_encrypt(&key, &data);
        assert_eq!(enc.len(), 32);
        assert_ne!(&enc[..], &data[..]);
        assert_eq!(aes_ecb_decrypt(&key, &enc), data);
    }

    #[test]
    fn ecb_known_answer() {
        // FIPS-197 appendix C.1 (AES-128).
        let key: [u8; 16] = crate::hex::decode("000102030405060708090a0b0c0d0e0f")
            .unwrap()
            .try_into()
            .unwrap();
        let pt = crate::hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let ct = aes_ecb_encrypt(&key, &pt);
        assert_eq!(crate::hex::encode(&ct), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    #[test]
    fn hash_selection() {
        assert_eq!(PairingHash::for_server_major_version(7), PairingHash::Sha256);
        assert_eq!(PairingHash::for_server_major_version(6), PairingHash::Sha1);
        assert_eq!(derive_aes_key(&[0; 16], "1234", PairingHash::Sha256).len(), 16);
    }

    #[test]
    fn otp_is_uppercase_hex() {
        let proof = otp_auth("1234", "00ff", "secret");
        assert_eq!(proof.len(), 64);
        assert!(proof.chars().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()));
    }
}
