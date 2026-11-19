//! GameStream PIN pairing, ported from Moonlight's `NvPairingManager`.
//!
//! The handshake proves both sides know the PIN without sending it, exchanges
//! certificates and has each side sign a secret so a man in the middle can't
//! swap certificates. On success the host stores our certificate and we pin
//! its certificate for every later HTTPS request.
//!
//! Two flavours exist:
//! * **Client PIN**: we show a random 4-digit PIN and the user types it into
//!   the host's web UI.
//! * **OTP** (Apollo/Hermes): the host shows a PIN + passphrase; we send a
//!   proof derived from both and the host accepts without user interaction.

use std::fmt;
use std::time::Duration;

use hestia_crypto::pairing::{self as crypto, PairingHash};
use hestia_crypto::{hex, pem};
use log::{info, warn};

use crate::client::{NvHttpClient, REQUEST_TIMEOUT};
use crate::{NvHttpError, Result};

/// How long to wait for the user to type the PIN on the host.
pub const PIN_ENTRY_TIMEOUT: Duration = Duration::from_secs(180);

/// Name the host shows for this client in its paired-devices list.
pub const DEFAULT_DEVICE_NAME: &str = "Hestia RS";

#[derive(Clone, PartialEq, Eq)]
pub struct PairingPin(String);

impl PairingPin {
    /// A random 4-digit PIN.
    pub fn random() -> Self {
        let n = u32::from_le_bytes(crypto::random_bytes::<4>()) % 10_000;
        Self(format!("{n:04}"))
    }

    /// Accepts 4+ ASCII digits.
    pub fn parse(pin: &str) -> Option<Self> {
        let pin = pin.trim();
        (pin.len() >= 4 && pin.bytes().all(|b| b.is_ascii_digit())).then(|| Self(pin.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PairingPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PairingPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingPin(****)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingOutcome {
    /// Paired; the host certificate (DER) is now pinned on the client.
    Paired {
        server_cert_der: Vec<u8>,
    },
    WrongPin,
    /// Another pairing attempt is pending on the host.
    AlreadyInProgress,
    Failed(String),
}

impl NvHttpClient {
    fn pair_step(&self, args: &[(&str, String)], https: bool, timeout: Duration) -> Result<crate::xml::Element> {
        let mut all = vec![
            ("devicename", self.device_name().to_owned()),
            ("updateState", "1".to_owned()),
        ];
        all.extend(args.iter().cloned());
        if https {
            self.get_https_xml("pair", &all, timeout)
        } else {
            self.get_http_xml("pair", &all, timeout)
        }
    }

    fn device_name(&self) -> &str {
        DEFAULT_DEVICE_NAME
    }

    /// Cancels a pending pairing attempt on the host.
    pub fn cancel_pairing(&self) {
        if let Err(e) = self.get_http_xml("unpair", &[], REQUEST_TIMEOUT) {
            warn!("failed to cancel pairing on {}: {e}", self.host());
        }
    }

    /// Runs the full handshake. `server_major_version` comes from
    /// [`ServerInfo::major_version`](crate::ServerInfo::major_version).
    /// Blocks until the user enters the PIN on the host (or the OTP proof is
    /// accepted), up to [`PIN_ENTRY_TIMEOUT`].
    pub fn pair(
        &mut self,
        server_major_version: i32,
        pin: &PairingPin,
        otp_passphrase: Option<&str>,
    ) -> Result<PairingOutcome> {
        let outcome = self.pair_inner(server_major_version, pin, otp_passphrase);
        match &outcome {
            Ok(PairingOutcome::Paired { .. }) | Ok(PairingOutcome::AlreadyInProgress) => {}
            Ok(_) | Err(_) => {
                // Leave the host in a clean state and drop the unverified pin.
                self.cancel_pairing();
                self.set_server_cert(None);
            }
        }
        outcome
    }

    fn pair_inner(
        &mut self,
        server_major_version: i32,
        pin: &PairingPin,
        otp_passphrase: Option<&str>,
    ) -> Result<PairingOutcome> {
        let hash = PairingHash::for_server_major_version(server_major_version);
        info!("pairing with {} using {hash:?}", self.host());

        let salt = crypto::random_bytes::<16>();
        let salt_hex = hex::encode(&salt);
        let aes_key = crypto::derive_aes_key(&salt, pin.as_str(), hash);

        // Stage 1: send salt + our cert, receive the host cert. On the client
        // PIN flow this blocks until the user types the PIN on the host.
        let mut args = vec![
            ("phrase", "getservercert".to_owned()),
            ("salt", salt_hex.clone()),
            ("clientcert", hex::encode(self.identity().cert_pem().as_bytes())),
        ];
        if let Some(passphrase) = otp_passphrase {
            args.push(("otpauth", crypto::otp_auth(pin.as_str(), &salt_hex, passphrase)));
        }
        let stage1 = self.pair_step(&args, false, PIN_ENTRY_TIMEOUT)?;
        if stage1.child_text("paired") != Some("1") {
            return Ok(PairingOutcome::Failed("host refused pairing (stage 1)".into()));
        }
        let Some(plaincert) = stage1.child_text("plaincert").filter(|c| !c.is_empty()) else {
            return Ok(PairingOutcome::AlreadyInProgress);
        };
        let cert_pem = hex::decode(plaincert)
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| NvHttpError::InvalidResponse("plaincert is not hex PEM".into()))?;
        let server_cert = pem::decode("CERTIFICATE", &cert_pem)
            .ok_or_else(|| NvHttpError::InvalidResponse("plaincert has no certificate".into()))?;
        let server_cert_signature = crypto::cert_signature(&server_cert)?;

        // Pin the (not yet verified) cert so the final HTTPS step can run.
        self.set_server_cert(Some(server_cert.clone()));

        // Stage 2: our encrypted random challenge.
        let client_challenge = crypto::random_bytes::<16>();
        let stage2 = self.pair_step(
            &[(
                "clientchallenge",
                hex::encode(&crypto::aes_ecb_encrypt(&aes_key, &client_challenge)),
            )],
            false,
            REQUEST_TIMEOUT,
        )?;
        if stage2.child_text("paired") != Some("1") {
            return Ok(PairingOutcome::Failed("host refused pairing (stage 2)".into()));
        }
        let response = stage2
            .child_text("challengeresponse")
            .and_then(hex::decode)
            .map(|enc| crypto::aes_ecb_decrypt(&aes_key, &enc))
            .ok_or_else(|| NvHttpError::InvalidResponse("missing challengeresponse".into()))?;
        if response.len() < hash.len() + 16 {
            return Ok(PairingOutcome::Failed("challengeresponse too short".into()));
        }
        let server_response_hash = &response[..hash.len()];
        let server_challenge = &response[hash.len()..hash.len() + 16];

        // Stage 3: answer the host's challenge with hash(challenge || our cert
        // signature || our secret).
        let client_secret = crypto::random_bytes::<16>();
        let mut answer = server_challenge.to_vec();
        answer.extend_from_slice(&self.identity().cert_signature());
        answer.extend_from_slice(&client_secret);
        let mut padded = hash.digest(&answer);
        padded.resize(32, 0);
        let stage3 = self.pair_step(
            &[(
                "serverchallengeresp",
                hex::encode(&crypto::aes_ecb_encrypt(&aes_key, &padded)),
            )],
            false,
            REQUEST_TIMEOUT,
        )?;
        if stage3.child_text("paired") != Some("1") {
            return Ok(PairingOutcome::Failed("host refused pairing (stage 3)".into()));
        }
        let pairing_secret = stage3
            .child_text("pairingsecret")
            .and_then(hex::decode)
            .ok_or_else(|| NvHttpError::InvalidResponse("missing pairingsecret".into()))?;
        if pairing_secret.len() <= 16 {
            return Ok(PairingOutcome::Failed("pairingsecret too short".into()));
        }
        let (server_secret, server_signature) = pairing_secret.split_at(16);

        // The host must have signed its secret with the key of the cert it sent.
        if !crypto::verify_sha256(&server_cert, server_secret, server_signature)? {
            return Ok(PairingOutcome::Failed(
                "host signature did not verify (possible man-in-the-middle)".into(),
            ));
        }

        // And its answer to our challenge proves it knows the PIN.
        let mut expected = client_challenge.to_vec();
        expected.extend_from_slice(&server_cert_signature);
        expected.extend_from_slice(server_secret);
        if hash.digest(&expected) != server_response_hash {
            return Ok(PairingOutcome::WrongPin);
        }

        // Stage 4: our signed secret.
        let mut client_pairing_secret = client_secret.to_vec();
        client_pairing_secret.extend_from_slice(&self.identity().sign_sha256(&client_secret));
        let stage4 = self.pair_step(
            &[("clientpairingsecret", hex::encode(&client_pairing_secret))],
            false,
            REQUEST_TIMEOUT,
        )?;
        if stage4.child_text("paired") != Some("1") {
            return Ok(PairingOutcome::Failed("host refused pairing (stage 4)".into()));
        }

        // Stage 5: the same over HTTPS proves the TLS identities line up.
        let stage5 = self.pair_step(&[("phrase", "pairchallenge".to_owned())], true, REQUEST_TIMEOUT)?;
        if stage5.child_text("paired") != Some("1") {
            return Ok(PairingOutcome::Failed("host refused pairing (stage 5)".into()));
        }

        info!("paired with {}", self.host());
        Ok(PairingOutcome::Paired {
            server_cert_der: server_cert,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins() {
        let pin = PairingPin::random();
        assert_eq!(pin.as_str().len(), 4);
        assert!(PairingPin::parse(pin.as_str()).is_some());
        assert!(PairingPin::parse("12a4").is_none());
        assert!(PairingPin::parse("123").is_none());
        assert_eq!(format!("{pin:?}"), "PairingPin(****)");
    }
}
