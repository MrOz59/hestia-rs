//! Streaming presets, as in Hestia desktop: one choice sets resolution,
//! frame rate and bitrate from this device's display.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// The user picks everything.
    #[default]
    Custom,
    /// Up to 1080p60, slightly lower bitrate: weak decoders and networks.
    Fast,
    /// Native resolution, at most 60 fps.
    Balanced,
    /// Native resolution and refresh rate, extra bitrate.
    Quality,
    /// 720p30, lower bitrate: handhelds on battery.
    Battery,
}

impl Preset {
    pub const ALL: [Self; 5] = [Self::Custom, Self::Fast, Self::Balanced, Self::Quality, Self::Battery];

    pub fn label(self) -> &'static str {
        match self {
            Self::Custom => "Custom",
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::Quality => "Quality",
            Self::Battery => "Battery saver",
        }
    }
}

/// Default bitrate for a mode, from the table Moonlight/Hestia have used for
/// years (interpolated by pixel count; frame rate above 60 scales by √).
pub fn default_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    const TABLE: [(u64, f64); 6] = [
        (640 * 360, 1.0),
        (854 * 480, 2.0),
        (1280 * 720, 5.0),
        (1920 * 1080, 10.0),
        (2560 * 1440, 20.0),
        (3840 * 2160, 40.0),
    ];
    let fps = f64::from(fps.max(1));
    let frame_rate_factor = (if fps <= 60.0 { fps } else { (fps / 60.0).sqrt() * 60.0 }) / 30.0;
    let pixels = u64::from(width) * u64::from(height);
    let mut factor = TABLE[TABLE.len() - 1].1;
    for (i, &(px, f)) in TABLE.iter().enumerate() {
        if pixels <= px {
            factor = if pixels == px || i == 0 {
                f
            } else {
                let (prev_px, prev_f) = TABLE[i - 1];
                (pixels - prev_px) as f64 / (px - prev_px) as f64 * (f - prev_f) + prev_f
            };
            break;
        }
    }
    ((factor * frame_rate_factor).round() as u32) * 1000
}

/// Shrinks a mode to at most `max_w × max_h` pixels, keeping its aspect
/// ratio and even dimensions (encoders need them).
fn cap(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let pixels = u64::from(width) * u64::from(height);
    let max = u64::from(max_w) * u64::from(max_h);
    if pixels <= max {
        return (width, height);
    }
    let scale = (max as f64 / pixels as f64).sqrt();
    (
        (f64::from(width) * scale) as u32 & !1,
        (f64::from(height) * scale) as u32 & !1,
    )
}

/// `(width, height, fps, bitrate_kbps)` for `preset` on a display of
/// `native` = (width, height, refresh). `None` for [`Preset::Custom`].
pub fn preset_mode(preset: Preset, native: (u32, u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let (mut w, mut h, native_fps) = native;
    if w == 0 || h == 0 {
        (w, h) = (1920, 1080);
    }
    let native_fps = if native_fps == 0 { 60 } else { native_fps };
    let (mut fps, mut scale) = (native_fps, 1.0);
    match preset {
        Preset::Custom => return None,
        Preset::Quality => scale = 1.25,
        Preset::Balanced => fps = native_fps.min(60),
        Preset::Fast => {
            (w, h) = cap(w, h, 1920, 1080);
            fps = native_fps.min(60);
            scale = 0.9;
        }
        Preset::Battery => {
            (w, h) = cap(w, h, 1280, 720);
            fps = native_fps.min(30);
            scale = 0.7;
        }
    }
    let kbps = (f64::from(default_bitrate_kbps(w, h, fps)) * scale).round() as u32;
    Some((w, h, fps, kbps))
}

/// `(width, height, refresh)` of the preferred mode in an EDID blob (its
/// first detailed timing descriptor), as Linux exposes it in
/// `/sys/class/drm/*/edid`.
pub fn edid_preferred_mode(edid: &[u8]) -> Option<(u32, u32, u32)> {
    const HEADER: [u8; 8] = [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0];
    if edid.len() < 72 || edid[..8] != HEADER {
        return None;
    }
    let d = &edid[54..72];
    let clock_hz = u64::from(u16::from_le_bytes([d[0], d[1]])) * 10_000;
    let h_active = u32::from(d[2]) | (u32::from(d[4] & 0xf0) << 4);
    let h_blank = u32::from(d[3]) | (u32::from(d[4] & 0x0f) << 8);
    let v_active = u32::from(d[5]) | (u32::from(d[7] & 0xf0) << 4);
    let v_blank = u32::from(d[6]) | (u32::from(d[7] & 0x0f) << 8);
    let total = u64::from(h_active + h_blank) * u64::from(v_active + v_blank);
    if clock_hz == 0 || h_active == 0 || v_active == 0 || total == 0 {
        return None;
    }
    let hz = (clock_hz as f64 / total as f64).round() as u32;
    Some((h_active, v_active, hz))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_preferred_mode_from_edid() {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        // CEA 1080p60: 148.5 MHz, 1920+280 × 1080+45.
        edid[54..62].copy_from_slice(&[0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40]);
        assert_eq!(edid_preferred_mode(&edid), Some((1920, 1080, 60)));
        // 2560×1440@144 (CVT-RB: 586.59 MHz, 2560+160 × 1440+57).
        edid[54..62].copy_from_slice(&[0xa3, 0xe5, 0x00, 0xa0, 0xa0, 0xa0, 0x39, 0x50]);
        assert_eq!(edid_preferred_mode(&edid), Some((2560, 1440, 144)));
        assert_eq!(edid_preferred_mode(&edid[..60]), None);
        assert_eq!(edid_preferred_mode(&[0u8; 128]), None);
    }

    #[test]
    fn default_bitrates_match_the_desktop_table() {
        assert_eq!(default_bitrate_kbps(1920, 1080, 60), 20_000);
        assert_eq!(default_bitrate_kbps(1280, 720, 30), 5_000);
        assert_eq!(default_bitrate_kbps(3840, 2160, 120), 113_000);
        assert_eq!(default_bitrate_kbps(1280, 800, 90), 13_000);
        assert_eq!(default_bitrate_kbps(320, 240, 30), 1_000, "never below the table");
        assert_eq!(default_bitrate_kbps(7680, 4320, 60), 80_000, "never above it");
    }

    #[test]
    fn presets_on_a_phone_a_4k_tv_and_a_1080p_monitor() {
        let phone = (2400, 1080, 120);
        assert_eq!(preset_mode(Preset::Quality, phone), Some((2400, 1080, 120, 46_250)));
        assert_eq!(preset_mode(Preset::Balanced, phone), Some((2400, 1080, 60, 26_000)));
        assert_eq!(preset_mode(Preset::Fast, phone), Some((2146, 964, 60, 18_000)));
        assert_eq!(preset_mode(Preset::Battery, phone), Some((1430, 642, 30, 3_500)));

        let tv = (3840, 2160, 144);
        assert_eq!(preset_mode(Preset::Quality, tv), Some((3840, 2160, 144, 155_000)));
        assert_eq!(preset_mode(Preset::Balanced, tv), Some((3840, 2160, 60, 80_000)));
        assert_eq!(preset_mode(Preset::Fast, tv), Some((1920, 1080, 60, 18_000)));

        assert_eq!(
            preset_mode(Preset::Quality, (1920, 1080, 60)),
            Some((1920, 1080, 60, 25_000))
        );
        assert_eq!(preset_mode(Preset::Custom, (1920, 1080, 60)), None);
        // Unknown display: 1080p60.
        assert_eq!(preset_mode(Preset::Balanced, (0, 0, 0)), Some((1920, 1080, 60, 20_000)));
    }
}
