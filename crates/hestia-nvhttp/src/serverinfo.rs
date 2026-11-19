//! `/serverinfo`.

use crate::xml::Element;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub hostname: String,
    /// e.g. `7.1.431.-1`; the first component selects the pairing hash.
    pub app_version: String,
    pub gfe_version: String,
    pub unique_id: String,
    pub https_port: u16,
    pub external_port: u16,
    pub mac: Option<String>,
    pub local_ip: Option<String>,
    pub codec_mode_support: u32,
    pub max_luma_pixels_hevc: u64,
    /// Only meaningful when fetched over HTTPS with our client certificate.
    pub paired: bool,
    pub state: String,
    pub current_game: u32,
    pub current_game_uuid: Option<String>,
    pub display_modes: Vec<DisplayMode>,
    /// Apollo/Hermes per-client permission bitmask (HTTPS only).
    pub permission: Option<u32>,
    pub virtual_display_capable: bool,
    pub virtual_display_driver_ready: bool,
}

fn opt(root: &Element, name: &str) -> Option<String> {
    root.child_text(name).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn flag(root: &Element, name: &str) -> bool {
    matches!(root.child_text(name), Some("1" | "true"))
}

impl ServerInfo {
    pub fn from_xml(root: &Element) -> Self {
        let num = |name: &str| root.child_text(name).and_then(|s| s.parse::<i64>().ok());
        let state = root.child_text("state").unwrap_or_default().to_owned();
        let busy = state.ends_with("_SERVER_BUSY");

        let display_modes = root
            .find("SupportedDisplayMode")
            .map(|list| {
                list.children_named("DisplayMode")
                    .filter_map(|m| {
                        Some(DisplayMode {
                            width: m.child_text("Width")?.parse().ok()?,
                            height: m.child_text("Height")?.parse().ok()?,
                            refresh_rate: m.child_text("RefreshRate").and_then(|r| r.parse().ok()).unwrap_or(60),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            hostname: root.child_text("hostname").unwrap_or_default().to_owned(),
            app_version: root.child_text("appversion").unwrap_or_default().to_owned(),
            gfe_version: root.child_text("GfeVersion").unwrap_or_default().to_owned(),
            unique_id: root.child_text("uniqueid").unwrap_or_default().to_owned(),
            https_port: num("HttpsPort").and_then(|p| u16::try_from(p).ok()).unwrap_or(0),
            external_port: num("ExternalPort").and_then(|p| u16::try_from(p).ok()).unwrap_or(0),
            mac: opt(root, "mac").filter(|m| m != "00:00:00:00:00:00"),
            local_ip: opt(root, "LocalIP"),
            codec_mode_support: num("ServerCodecModeSupport").map(|v| v as u32).unwrap_or(0),
            max_luma_pixels_hevc: num("MaxLumaPixelsHEVC").map(|v| v.max(0) as u64).unwrap_or(0),
            paired: root.child_text("PairStatus") == Some("1"),
            current_game: if busy {
                num("currentgame").map(|v| v as u32).unwrap_or(0)
            } else {
                0
            },
            current_game_uuid: opt(root, "currentgameuuid"),
            state,
            display_modes,
            permission: num("Permission").map(|v| v as u32),
            virtual_display_capable: flag(root, "VirtualDisplayCapable"),
            virtual_display_driver_ready: flag(root, "VirtualDisplayDriverReady"),
        }
    }

    /// Components of the dotted app version (`7.1.431.-1` → `[7, 1, 431, -1]`).
    pub fn version_quad(&self) -> Vec<i32> {
        self.app_version
            .split('.')
            .map(|p| p.trim().parse().unwrap_or(0))
            .collect()
    }

    pub fn major_version(&self) -> i32 {
        self.version_quad().first().copied().unwrap_or(0)
    }

    pub fn is_busy(&self) -> bool {
        self.current_game != 0
    }

    /// Sunshine-family hosts (Sunshine, Apollo, Hermes) report SUNSHINE_* states.
    pub fn is_sunshine_family(&self) -> bool {
        self.state.starts_with("SUNSHINE")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hermes_serverinfo() {
        let doc = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200"><hostname>cachy</hostname><appversion>7.1.431.-1</appversion><GfeVersion>3.23.0.74</GfeVersion><uniqueid>ABCDEF</uniqueid><HttpsPort>47984</HttpsPort><ExternalPort>47989</ExternalPort><MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC><mac>00:00:00:00:00:00</mac><Permission>0</Permission><VirtualDisplayCapable>true</VirtualDisplayCapable><VirtualDisplayDriverReady>true</VirtualDisplayDriverReady><LocalIP>192.168.0.10</LocalIP><ServerCodecModeSupport>197377</ServerCodecModeSupport><PairStatus>0</PairStatus><currentgame>5</currentgame><currentgameuuid></currentgameuuid><state>SUNSHINE_SERVER_FREE</state></root>"#;
        let info = ServerInfo::from_xml(&crate::xml::parse(doc).unwrap());
        assert_eq!(info.hostname, "cachy");
        assert_eq!(info.major_version(), 7);
        assert_eq!(info.https_port, 47984);
        assert_eq!(info.mac, None);
        assert_eq!(info.local_ip.as_deref(), Some("192.168.0.10"));
        assert!(!info.paired);
        assert_eq!(info.current_game, 0, "currentgame only counts while busy");
        assert!(info.virtual_display_capable);
        assert!(info.is_sunshine_family());
        assert_eq!(
            info.codec_mode_support & crate::codecs::SCM_HEVC,
            crate::codecs::SCM_HEVC
        );
    }
}
