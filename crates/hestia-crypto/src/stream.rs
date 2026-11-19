//! Symmetric crypto used by the streaming protocol. All of it is keyed with
//! the 16-byte `rikey` sent in `/launch`:
//!
//! * AES-128-GCM with 12-byte IVs for encrypted RTSP, the control stream and
//!   (optionally) video packets;
//! * AES-128-CBC with PKCS#7 padding for (optional) audio packets.

use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes_gcm::Aes128Gcm;
use aes_gcm::aead::AeadInPlace;

pub const GCM_TAG_LEN: usize = 16;

/// Encrypts `plaintext`, returning `(ciphertext, tag)`. `iv` must be 12 bytes.
pub fn gcm_encrypt(key: &[u8; 16], iv: &[u8; 12], plaintext: &[u8]) -> (Vec<u8>, [u8; GCM_TAG_LEN]) {
    let cipher = Aes128Gcm::new(GenericArray::from_slice(key));
    let mut buf = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(GenericArray::from_slice(iv), b"", &mut buf)
        .expect("AES-GCM encryption of an in-memory buffer cannot fail");
    let mut tag_bytes = [0u8; GCM_TAG_LEN];
    tag_bytes.copy_from_slice(&tag);
    (buf, tag_bytes)
}

/// Decrypts and authenticates; `None` if the tag doesn't match.
pub fn gcm_decrypt(key: &[u8; 16], iv: &[u8; 12], ciphertext: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    if tag.len() != GCM_TAG_LEN {
        return None;
    }
    let cipher = Aes128Gcm::new(GenericArray::from_slice(key));
    let mut buf = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(
            GenericArray::from_slice(iv),
            b"",
            &mut buf,
            GenericArray::from_slice(tag),
        )
        .ok()?;
    Some(buf)
}

/// AES-128-CBC decryption with PKCS#7 padding removal.
pub fn cbc_decrypt_pkcs7(key: &[u8; 16], iv: &[u8; 16], ciphertext: &[u8]) -> Option<Vec<u8>> {
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = ciphertext.to_vec();
    let mut prev = *iv;
    for block in out.chunks_exact_mut(16) {
        let mut this = [0u8; 16];
        this.copy_from_slice(block);
        cipher.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        prev = this;
    }
    let pad = *out.last()? as usize;
    if pad == 0 || pad > 16 || out[out.len() - pad..].iter().any(|&b| b as usize != pad) {
        return None;
    }
    out.truncate(out.len() - pad);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hex;

    #[test]
    fn gcm_known_answer() {
        // NIST GCM test case 2: zero key, zero IV, one zero block.
        let (ct, tag) = gcm_encrypt(&[0; 16], &[0; 12], &[0; 16]);
        assert_eq!(hex::encode(&ct), "0388dace60b6a392f328c2b971b2fe78");
        assert_eq!(hex::encode(&tag), "ab6e47d42cec13bdf53a67b21257bddf");
        assert_eq!(gcm_decrypt(&[0; 16], &[0; 12], &ct, &tag).unwrap(), vec![0; 16]);
        let mut bad = tag;
        bad[0] ^= 1;
        assert!(gcm_decrypt(&[0; 16], &[0; 12], &ct, &bad).is_none());
    }

    #[test]
    fn cbc_known_answer() {
        // Generated with: printf 'hestia audio packet!' | openssl enc -aes-128-cbc -K <key> -iv <iv>
        let key: [u8; 16] = hex::decode("2b7e151628aed2a6abf7158809cf4f3c")
            .unwrap()
            .try_into()
            .unwrap();
        let iv: [u8; 16] = hex::decode("000102030405060708090a0b0c0d0e0f")
            .unwrap()
            .try_into()
            .unwrap();
        let ct = hex::decode("5ab83a45f251caa6c70ba71254369f5c2f8d4ea8e1905dff8fa05c4e583d7e7a").unwrap();
        assert_eq!(cbc_decrypt_pkcs7(&key, &iv, &ct).unwrap(), b"hestia audio packet!");
        // NIST SP 800-38A F.2.2 first block decrypts to a non-PKCS#7 plaintext.
        let nist = hex::decode("7649abac8119b246cee98e9b12e9197d").unwrap();
        assert!(cbc_decrypt_pkcs7(&key, &iv, &nist).is_none());
        assert!(cbc_decrypt_pkcs7(&key, &iv, &[0; 15]).is_none());
    }
}
