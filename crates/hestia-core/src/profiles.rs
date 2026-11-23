//! Named stream profiles: the quality side of the settings (mode, bitrate,
//! codec, audio, HDR, what the host does) saved under a name, like
//! Artemis' per-host profiles. A host can be set to launch with one; the
//! device-side choices (touch mode, orientation, decoding, menu button...)
//! always come from the regular settings.

use serde::{Deserialize, Serialize};

use crate::settings::{AudioLayout, CodecPreference, LaunchMode, StreamSettings};

/// Longest profile name kept.
pub const MAX_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamProfile {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub preset: crate::Preset,
    pub codec: CodecPreference,
    pub audio: AudioLayout,
    pub hdr: bool,
    pub virtual_display: bool,
    pub scale_factor: u32,
    pub play_audio_on_host: bool,
    pub launch_mode: LaunchMode,
    pub optimize_game_settings: bool,
}

/// A name as the user typed it, tidied: trimmed, single spaces, not too
/// long. None when nothing is left.
pub fn clean_name(name: &str) -> Option<String> {
    let name: String = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    let name = name.trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

impl StreamProfile {
    /// The quality settings of `s`, named `name`.
    pub fn from_settings(name: &str, s: &StreamSettings) -> Self {
        Self {
            name: name.to_owned(),
            width: s.width,
            height: s.height,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
            preset: s.preset,
            codec: s.codec,
            audio: s.audio,
            hdr: s.hdr,
            virtual_display: s.virtual_display,
            scale_factor: s.scale_factor,
            play_audio_on_host: s.play_audio_on_host,
            launch_mode: s.launch_mode,
            optimize_game_settings: s.optimize_game_settings,
        }
    }

    /// `base` with this profile's quality settings.
    pub fn apply(&self, base: &StreamSettings) -> StreamSettings {
        StreamSettings {
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            preset: self.preset,
            codec: self.codec,
            audio: self.audio,
            hdr: self.hdr,
            virtual_display: self.virtual_display,
            scale_factor: self.scale_factor,
            play_audio_on_host: self.play_audio_on_host,
            launch_mode: self.launch_mode,
            optimize_game_settings: self.optimize_game_settings,
            ..base.clone()
        }
    }
}

/// Saves `profile` into `list`, replacing one with the same name (case
/// aside); kept sorted by name.
pub fn upsert(list: &mut Vec<StreamProfile>, profile: StreamProfile) {
    list.retain(|p| !p.name.eq_ignore_ascii_case(&profile.name));
    list.push(profile);
    list.sort_by_key(|p| p.name.to_lowercase());
}

/// The profile called `name` (case aside).
pub fn find<'a>(list: &'a [StreamProfile], name: &str) -> Option<&'a StreamProfile> {
    list.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_quality_and_leaves_device_choices() {
        let tv = StreamSettings {
            width: 3840,
            height: 2160,
            fps: 60,
            bitrate_kbps: 80_000,
            hdr: true,
            ..StreamSettings::default()
        };
        let profile = StreamProfile::from_settings("TV 4K", &tv);
        let phone = StreamSettings {
            width: 1280,
            height: 720,
            onscreen_controller: true,
            hardware_decoding: false,
            ..StreamSettings::default()
        };
        let applied = profile.apply(&phone);
        assert_eq!(
            (applied.width, applied.height, applied.bitrate_kbps),
            (3840, 2160, 80_000)
        );
        assert!(applied.hdr);
        assert!(
            applied.onscreen_controller && !applied.hardware_decoding,
            "device side untouched"
        );
    }

    #[test]
    fn names_and_replacing() {
        assert_eq!(clean_name("  Couch   TV \n"), Some("Couch TV".into()));
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name(&"x".repeat(99)).unwrap().len(), MAX_NAME_CHARS);

        let s = StreamSettings::default();
        let mut list = Vec::new();
        upsert(&mut list, StreamProfile::from_settings("phone", &s));
        upsert(&mut list, StreamProfile::from_settings("Couch TV", &s));
        upsert(
            &mut list,
            StreamProfile::from_settings("Phone", &StreamSettings { fps: 120, ..s.clone() }),
        );
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Couch TV", "Phone"], "replaced, sorted");
        assert_eq!(find(&list, "PHONE").map(|p| p.fps), Some(120));
    }
}
