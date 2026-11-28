//! Portable palette packs and device-wide appearance preferences.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use serde::{Deserialize, Serialize};

use crate::store::AppearanceTheme;

pub const MAX_PACK_SIZE: usize = 256 * 1024;
const MAX_MANIFEST_SIZE: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchStyle {
    Pulse,
    Orbit,
    Still,
    #[default]
    #[serde(other)]
    Flame,
}

impl LaunchStyle {
    pub const ALL: [Self; 4] = [Self::Flame, Self::Pulse, Self::Orbit, Self::Still];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&style| style == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: AppearanceTheme,
    pub active_pack: Option<String>,
    pub packs: Vec<ThemePack>,
    pub launch_style: LaunchStyle,
    pub hidden_themes: Vec<AppearanceTheme>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemePack {
    format_version: u32,
    pub id: String,
    pub name: String,
    version: String,
    #[serde(deserialize_with = "pack_base")]
    pub base: AppearanceTheme,
    components: Components,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Components {
    palette: BTreeMap<PaletteRole, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PaletteRole {
    Bg,
    Surface,
    Card,
    CardHover,
    Border,
    Text,
    Muted,
    Faint,
    Accent,
    AccentHover,
    AccentPressed,
    OnAccent,
    Glow,
    Ok,
    Warn,
    Danger,
    Info,
    Scrim,
}

impl PaletteRole {
    /// Matches the palette roles in the shell.
    pub const ALL: [Self; 18] = [
        Self::Bg,
        Self::Surface,
        Self::Card,
        Self::CardHover,
        Self::Border,
        Self::Text,
        Self::Muted,
        Self::Faint,
        Self::Accent,
        Self::AccentHover,
        Self::AccentPressed,
        Self::OnAccent,
        Self::Glow,
        Self::Ok,
        Self::Warn,
        Self::Danger,
        Self::Info,
        Self::Scrim,
    ];

    fn allows_alpha(self) -> bool {
        matches!(
            self,
            Self::Surface | Self::Card | Self::CardHover | Self::Glow | Self::Scrim
        )
    }
}

/// A theme pack is data only. Read just the small root manifest in memory.
pub fn read_theme_pack(bytes: &[u8]) -> Result<ThemePack, String> {
    if bytes.len() > MAX_PACK_SIZE {
        return Err("Theme pack is larger than 256 KB.".into());
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| "Choose a valid Hestia RS theme ZIP.")?;
    if zip.len() != 1 {
        return Err("The ZIP must contain only manifest.json at its root.".into());
    }
    let file = zip
        .by_name("manifest.json")
        .map_err(|_| "The ZIP must contain manifest.json at its root.")?;
    if !file.is_file() || file.is_symlink() || file.size() > MAX_MANIFEST_SIZE as u64 {
        return Err("The theme manifest must be a regular file up to 32 KB.".into());
    }
    let mut contents = Vec::new();
    file.take((MAX_MANIFEST_SIZE + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(|_| "Could not read the theme manifest. The ZIP may be damaged.")?;
    if contents.len() > MAX_MANIFEST_SIZE {
        return Err("The theme manifest is larger than 32 KB.".into());
    }
    let pack: ThemePack =
        serde_json::from_slice(&contents).map_err(|error| format!("Invalid theme manifest. {error}"))?;
    pack.validate()?;
    Ok(pack)
}

impl ThemePack {
    pub fn validate(&self) -> Result<(), String> {
        if self.format_version != 1 {
            return Err("This version of Hestia RS supports theme format 1.".into());
        }
        let segment =
            |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let valid_id = self.id.len() <= 64 && self.id.contains(['.', '-']) && self.id.split(['.', '-']).all(segment);
        if !valid_id {
            return Err("Use a lowercase theme ID such as your-name.seaglass.".into());
        }
        if self.name.trim().is_empty() || self.name.chars().count() > 80 || self.name.chars().any(char::is_control) {
            return Err("Give the theme a name of up to 80 characters.".into());
        }
        let version: Vec<_> = self.version.split('.').collect();
        if version.len() != 3
            || version
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err("Use a theme version such as 1.0.0.".into());
        }
        if self.components.palette.is_empty() {
            return Err("The theme manifest needs at least one palette color.".into());
        }
        for (&role, value) in &self.components.palette {
            if decode_color(role, value).is_none() {
                return Err(format!(
                    "Invalid color for {role:?}. Use a six- or eight-digit hex color as allowed for this role."
                ));
            }
        }
        Ok(())
    }

    pub fn color(&self, role: PaletteRole) -> Option<[u8; 4]> {
        decode_color(role, self.components.palette.get(&role)?)
    }
}

fn decode_color(role: PaletteRole, color: &str) -> Option<[u8; 4]> {
    let hex = color.strip_prefix('#')?;
    if !(hex.len() == 6 || hex.len() == 8 && role.allows_alpha()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut rgba = [255; 4];
    for (out, byte) in rgba.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *out = u8::from_str_radix(std::str::from_utf8(byte).ok()?, 16).ok()?;
    }
    Some(rgba)
}

fn pack_base<'de, D: serde::Deserializer<'de>>(d: D) -> Result<AppearanceTheme, D::Error> {
    match String::deserialize(d)?.as_str() {
        "hearth" => Ok(AppearanceTheme::Hearth),
        "daylight" => Ok(AppearanceTheme::Daylight),
        "midnight" => Ok(AppearanceTheme::Midnight),
        "electric" => Ok(AppearanceTheme::Electric),
        "frosted-glass" => Ok(AppearanceTheme::FrostedGlass),
        "blue" => Ok(AppearanceTheme::Blue),
        "dark-rainbow" => Ok(AppearanceTheme::DarkRainbow),
        _ => Err(serde::de::Error::custom("unknown built-in theme base")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    const MANIFEST: &str = r##"{
        "format_version":1,"id":"community.ice","name":"Community Ice","version":"1.0.0",
        "base":"frosted-glass","components":{"palette":{"card":"#12345680","accent":"#67cfff"}}
    }"##;

    fn archive(entry: &str, manifest: &str) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file(
            entry,
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn every_downloadable_starter_is_a_valid_pack() {
        let starters: [&[u8]; 7] = [
            include_bytes!("../../../tools/theme-packs/hearth.zip"),
            include_bytes!("../../../tools/theme-packs/daylight.zip"),
            include_bytes!("../../../tools/theme-packs/midnight.zip"),
            include_bytes!("../../../tools/theme-packs/electric.zip"),
            include_bytes!("../../../tools/theme-packs/frosted-glass.zip"),
            include_bytes!("../../../tools/theme-packs/blue.zip"),
            include_bytes!("../../../tools/theme-packs/dark-rainbow.zip"),
        ];
        for (bytes, base) in starters.into_iter().zip(AppearanceTheme::ALL) {
            let pack = read_theme_pack(bytes).unwrap();
            assert_eq!(pack.base, base);
            assert_eq!(pack.components.palette.len(), PaletteRole::ALL.len());
        }
        read_theme_pack(include_bytes!("../../../tools/theme-packs/harbor.zip")).unwrap();
    }

    #[test]
    fn deflated_pack_loads_and_malformed_packs_fail() {
        let pack = read_theme_pack(&archive("manifest.json", MANIFEST)).unwrap();
        assert_eq!(pack.color(PaletteRole::Card), Some([18, 52, 86, 128]));
        assert_eq!(pack.color(PaletteRole::Text), None);
        assert!(read_theme_pack(&archive("nested/manifest.json", MANIFEST)).is_err());
        assert!(read_theme_pack(&archive("manifest.json", &MANIFEST.replace("frosted-glass", "future"))).is_err());
        assert!(read_theme_pack(&archive("manifest.json", &MANIFEST.replace("#67cfff", "#67cfff33"))).is_err());
        assert!(read_theme_pack(&archive("manifest.json", &" ".repeat(MAX_MANIFEST_SIZE + 1))).is_err());
        assert!(read_theme_pack(b"broken archive").is_err());
    }
}
