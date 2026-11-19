//! User-facing stream settings, persisted next to the host list.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodecPreference {
    #[default]
    Auto,
    H264,
    Hevc,
    Av1,
}

impl CodecPreference {
    pub const ALL: [Self; 4] = [Self::Auto, Self::H264, Self::Hevc, Self::Av1];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
            Self::Av1 => "AV1",
        }
    }

    /// Name used by the Hestia API (`h264`, `hevc`, `av1`).
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Auto | Self::Hevc => "hevc",
            Self::H264 => "h264",
            Self::Av1 => "av1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    #[default]
    Normal,
    /// Hermes: run the app inside an isolated Gamescope session.
    Gamescope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioLayout {
    #[default]
    Stereo,
    Surround51,
    Surround71,
}

/// How the video fits the window (from Hestia Android).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoScale {
    /// Whole picture, black bars if the aspect ratios differ.
    #[default]
    Fit,
    /// Fill the window, cropping the edges.
    Fill,
    /// Fill the window, distorting the picture.
    Stretch,
}

impl VideoScale {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fit => "Fit",
            Self::Fill => "Fill",
            Self::Stretch => "Stretch",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Fit => Self::Fill,
            Self::Fill => Self::Stretch,
            Self::Stretch => Self::Fit,
        }
    }

    pub fn index(self) -> i32 {
        match self {
            Self::Fit => 0,
            Self::Fill => 1,
            Self::Stretch => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: CodecPreference,
    pub hdr: bool,
    pub audio: AudioLayout,
    pub play_audio_on_host: bool,
    /// Apollo/Hermes: stream from a virtual display matching this client.
    pub virtual_display: bool,
    /// Desktop scale in percent (Hermes `scale_factor` feature).
    pub scale_factor: u32,
    pub launch_mode: LaunchMode,
    pub optimize_game_settings: bool,
    /// Keep the clipboard in sync with the host when it supports it.
    pub clipboard_sync: bool,
    /// Touch drives the host cursor like a trackpad (relative); otherwise
    /// a touch clicks where the finger lands.
    pub touch_trackpad: bool,
    /// Show the session report (stats + logs) when a stream ends.
    pub show_stream_report: bool,
    /// Preset that produced resolution/fps/bitrate (Custom once edited).
    pub preset: crate::Preset,
    pub video_scale: VideoScale,
    /// Keep the menu button on the stream (otherwise Back/Escape open the
    /// menu). Off on phones, where the button only covers the picture.
    pub show_menu_button: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            codec: CodecPreference::Auto,
            hdr: false,
            audio: AudioLayout::Stereo,
            play_audio_on_host: false,
            virtual_display: true,
            scale_factor: 100,
            launch_mode: LaunchMode::Normal,
            optimize_game_settings: true,
            clipboard_sync: false,
            touch_trackpad: true,
            show_stream_report: true,
            preset: crate::Preset::Custom,
            video_scale: VideoScale::Fit,
            show_menu_button: !cfg!(target_os = "android"),
        }
    }
}

impl StreamSettings {
    /// Adds these settings to a session report.
    pub fn add_to_report(&self, r: &mut crate::report::StreamReport) {
        r.field(
            "Settings",
            "resolution",
            format!("{}x{} @ {} fps", self.width, self.height, self.fps),
        );
        r.field("Settings", "bitrate", format!("{} kbps", self.bitrate_kbps));
        r.field("Settings", "codec preference", self.codec.label());
        r.field("Settings", "audio", format!("{:?}", self.audio));
        r.field("Settings", "HDR", self.hdr);
        r.field("Settings", "virtual display", self.virtual_display);
        r.field("Settings", "launch mode", format!("{:?}", self.launch_mode));
        r.field(
            "Settings",
            "touch",
            if self.touch_trackpad { "trackpad" } else { "direct" },
        );
        r.field("Settings", "preset", self.preset.label());
        r.field("Settings", "video scale", self.video_scale.label());
    }

    /// Rough default bitrate (kbps) for a resolution/frame rate, in the spirit
    /// of Moonlight's defaults.
    pub fn suggested_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
        let pixels = (width as u64 * height as u64).max(1);
        let base_1080p60 = 20_000f64;
        let scale = pixels as f64 / (1920.0 * 1080.0);
        let fps_factor = (fps.max(30) as f64 / 60.0).sqrt();
        ((base_1080p60 * scale * fps_factor) as u32).clamp(5_000, 150_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let s = StreamSettings::default();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<StreamSettings>(&json).unwrap(), s);
        // Missing fields fall back to defaults.
        let partial: StreamSettings = serde_json::from_str(r#"{"fps": 120, "codec": "av1"}"#).unwrap();
        assert_eq!(partial.fps, 120);
        assert_eq!(partial.codec, CodecPreference::Av1);
        assert_eq!(partial.width, 1920);
    }

    #[test]
    fn bitrate_scales() {
        assert_eq!(StreamSettings::suggested_bitrate_kbps(1920, 1080, 60), 20_000);
        assert!(StreamSettings::suggested_bitrate_kbps(3840, 2160, 60) > 60_000);
        assert!(StreamSettings::suggested_bitrate_kbps(1280, 720, 60) < 20_000);
    }
}
