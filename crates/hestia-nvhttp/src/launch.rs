//! `/launch` and `/resume`.

use hestia_crypto::{hex, pairing::random_bytes};

/// Speaker layout requested from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioConfig {
    #[default]
    Stereo,
    Surround51,
    Surround71,
}

impl AudioConfig {
    pub fn channel_count(self) -> u32 {
        match self {
            Self::Stereo => 2,
            Self::Surround51 => 6,
            Self::Surround71 => 8,
        }
    }

    /// WAVEFORMATEXTENSIBLE channel mask.
    pub fn channel_mask(self) -> u32 {
        match self {
            Self::Stereo => 0x3,
            Self::Surround51 => 0x3f,
            Self::Surround71 => 0x63f,
        }
    }

    /// Value of the `surroundAudioInfo` launch parameter.
    pub fn surround_audio_info(self) -> u32 {
        (self.channel_mask() << 16) | self.channel_count()
    }
}

/// Everything `/launch` (or `/resume`) needs. The remote-input key and key id
/// are also the AES-GCM key material for the RTSP/control/input streams, so
/// the streaming layer must reuse the values sent here.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub app_id: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Let the host change its display mode to match the stream ("SOPS").
    pub optimize_game_settings: bool,
    /// Keep playing audio on the host too.
    pub play_audio_on_host: bool,
    pub audio: AudioConfig,
    /// Bitmask of attached gamepads.
    pub gamepad_mask: u16,
    pub persist_gamepads_after_disconnect: bool,
    /// Requests a 10-bit HDR stream.
    pub hdr: bool,
    /// Apollo/Hermes: stream from a virtual display sized to this client.
    pub virtual_display: bool,
    /// Apollo/Hermes: desktop scale factor in percent.
    pub scale_factor: Option<u32>,
    pub remote_input_key: [u8; 16],
    pub remote_input_key_id: u32,
    /// `corever` 1 asks for encrypted RTSP (`rtspenc://`), which modern hosts
    /// may require.
    pub core_version: u32,
}

impl LaunchRequest {
    pub fn new(app_id: u32, width: u32, height: u32, fps: u32) -> Self {
        Self {
            app_id,
            width,
            height,
            fps,
            optimize_game_settings: true,
            play_audio_on_host: false,
            audio: AudioConfig::Stereo,
            gamepad_mask: 0,
            persist_gamepads_after_disconnect: false,
            hdr: false,
            virtual_display: false,
            scale_factor: None,
            remote_input_key: random_bytes::<16>(),
            remote_input_key_id: u32::from_be_bytes(random_bytes::<4>()),
            core_version: 1,
        }
    }

    pub fn query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut q = vec![
            ("appid", self.app_id.to_string()),
            ("mode", format!("{}x{}x{}", self.width, self.height, self.fps)),
            ("additionalStates", "1".into()),
            ("sops", u8::from(self.optimize_game_settings).to_string()),
            ("rikey", hex::encode(&self.remote_input_key)),
            // The host parses rikeyid as a decimal int and prepends it, big
            // endian, to the IV. Send it as the signed value Moonlight sends.
            ("rikeyid", (self.remote_input_key_id as i32).to_string()),
        ];
        if self.hdr {
            q.extend([
                ("hdrMode", "1".into()),
                ("clientHdrCapVersion", "0".into()),
                ("clientHdrCapSupportedFlagsInUint32", "0".into()),
                ("clientHdrCapMetaDataId", "NV_STATIC_METADATA_TYPE_1".into()),
                ("clientHdrCapDisplayData", "0x0x0x0x0x0x0x0x0x0x0".into()),
            ]);
        }
        q.extend([
            ("localAudioPlayMode", u8::from(self.play_audio_on_host).to_string()),
            ("surroundAudioInfo", self.audio.surround_audio_info().to_string()),
            ("remoteControllersBitmap", self.gamepad_mask.to_string()),
            ("gcmap", self.gamepad_mask.to_string()),
            (
                "gcpersist",
                u8::from(self.persist_gamepads_after_disconnect).to_string(),
            ),
        ]);
        if self.virtual_display {
            q.push(("virtualDisplay", "1".into()));
        }
        if let Some(scale) = self.scale_factor {
            q.push(("scaleFactor", scale.to_string()));
        }
        q.push(("corever", self.core_version.to_string()));
        q
    }
}

#[derive(Debug, Clone)]
pub struct LaunchResponse {
    /// `rtsp://host:48010` or `rtspenc://host:48010`.
    pub session_url: String,
    pub resumed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_info_matches_moonlight() {
        // SURROUNDAUDIOINFO_FROM_AUDIO_CONFIGURATION(AUDIO_CONFIGURATION_STEREO)
        assert_eq!(AudioConfig::Stereo.surround_audio_info(), 196_610);
        assert_eq!(AudioConfig::Surround51.surround_audio_info(), (0x3f << 16) | 6);
    }

    #[test]
    fn query_contains_required_fields() {
        let mut req = LaunchRequest::new(7, 1920, 1080, 120);
        req.remote_input_key_id = 0xffff_fffe;
        let q = req.query_pairs();
        let get = |k: &str| q.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("mode"), Some("1920x1080x120"));
        assert_eq!(get("rikeyid"), Some("-2"));
        assert_eq!(get("rikey").map(str::len), Some(32));
        assert_eq!(get("localAudioPlayMode"), Some("0"));
        assert_eq!(get("corever"), Some("1"));
        assert_eq!(get("virtualDisplay"), None);
    }
}
