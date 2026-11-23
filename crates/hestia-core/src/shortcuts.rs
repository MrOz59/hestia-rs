//! Configurable local stream shortcuts. These are device preferences, not
//! sent to Hermes and not stored in a host's stream profile.

use serde::{Deserialize, Serialize};

use crate::stream::input::{Modifiers, vk};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    Disconnect,
    Stats,
    MouseCapture,
}

impl ShortcutAction {
    pub fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(Self::Disconnect),
            1 => Some(Self::Stats),
            2 => Some(Self::MouseCapture),
            _ => None,
        }
    }
}

/// Physical key code plus the required modifiers. Supports A-Z, 0-9, and
/// F1-F24; the key represents its position, not the typed character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortcutBinding {
    pub key: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl ShortcutBinding {
    const fn default_for(key: u16) -> Self {
        Self {
            key,
            ctrl: true,
            alt: true,
            shift: true,
            meta: false,
        }
    }

    pub fn matches(self, key: u16, mods: Modifiers) -> bool {
        self.key == key
            && self.ctrl == mods.ctrl
            && self.alt == mods.alt
            && self.shift == mods.shift
            && self.meta == mods.meta
    }

    /// Human-readable and stable across keyboard layouts.
    pub fn label(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".into());
        }
        if self.alt {
            parts.push("Alt".into());
        }
        if self.shift {
            parts.push("Shift".into());
        }
        if self.meta {
            parts.push("Meta".into());
        }
        let key = if (vk::F1..=vk::F1 + 23).contains(&self.key) {
            format!("F{}", self.key - vk::F1 + 1)
        } else if let Ok(n) = u8::try_from(self.key) {
            (n as char).to_string()
        } else {
            format!("0x{:X}", self.key)
        };
        parts.push(key);
        parts.join("+")
    }

    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let mut binding = Self {
            key: 0,
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        };
        let mut found_key = false;
        for token in text.split('+') {
            let name = token.trim().to_ascii_uppercase();
            match name.as_str() {
                "CTRL" | "CONTROL" if !binding.ctrl => binding.ctrl = true,
                "ALT" if !binding.alt => binding.alt = true,
                "SHIFT" if !binding.shift => binding.shift = true,
                "META" | "SUPER" | "WIN" if !binding.meta => binding.meta = true,
                _ => {
                    if found_key {
                        return Err("shortcut must contain only one key");
                    }
                    let key = if name.len() == 1 && name.bytes().all(|b| b.is_ascii_alphanumeric()) {
                        name.as_bytes()[0] as u16
                    } else if let Some(number) = name
                        .strip_prefix('F')
                        .and_then(|n| n.parse::<u16>().ok())
                        .filter(|n| (1..=24).contains(n))
                    {
                        vk::F1 + number - 1
                    } else {
                        return Err("use A-Z, 0-9 or F1-F24 with modifiers");
                    };
                    binding.key = key;
                    found_key = true;
                }
            }
        }
        if !found_key {
            return Err("shortcut needs a key");
        }
        if !(binding.ctrl || binding.alt || binding.shift || binding.meta)
            && !(vk::F1..=vk::F1 + 23).contains(&binding.key)
        {
            return Err("letter and number shortcuts require a modifier");
        }
        Ok(binding)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shortcuts {
    pub disconnect: ShortcutBinding,
    pub stats: ShortcutBinding,
    pub mouse_capture: ShortcutBinding,
}

impl Default for Shortcuts {
    fn default() -> Self {
        Self {
            disconnect: ShortcutBinding::default_for(b'Q' as u16),
            stats: ShortcutBinding::default_for(b'S' as u16),
            mouse_capture: ShortcutBinding::default_for(b'Z' as u16),
        }
    }
}

impl Shortcuts {
    pub fn action(self, key: u16, mods: Modifiers) -> Option<ShortcutAction> {
        if self.disconnect.matches(key, mods) {
            Some(ShortcutAction::Disconnect)
        } else if self.stats.matches(key, mods) {
            Some(ShortcutAction::Stats)
        } else if self.mouse_capture.matches(key, mods) {
            Some(ShortcutAction::MouseCapture)
        } else {
            None
        }
    }

    /// A shortcut is never allowed to shadow a different Hestia command.
    pub fn assign(&mut self, action: ShortcutAction, binding: ShortcutBinding) -> Result<(), &'static str> {
        let duplicate = match action {
            ShortcutAction::Disconnect => binding == self.stats || binding == self.mouse_capture,
            ShortcutAction::Stats => binding == self.disconnect || binding == self.mouse_capture,
            ShortcutAction::MouseCapture => binding == self.disconnect || binding == self.stats,
        };
        if duplicate {
            return Err("another Hestia action already uses that shortcut");
        }
        match action {
            ShortcutAction::Disconnect => self.disconnect = binding,
            ShortcutAction::Stats => self.stats = binding,
            ShortcutAction::MouseCapture => self.mouse_capture = binding,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_shortcuts() {
        let expected = Shortcuts::default().disconnect;
        assert_eq!(ShortcutBinding::parse("ctrl + alt+ shift +q"), Ok(expected));
        assert_eq!(expected.label(), "Ctrl+Alt+Shift+Q");
        assert_eq!(ShortcutBinding::parse("Ctrl+F12").unwrap().label(), "Ctrl+F12");
        assert_eq!(ShortcutBinding::parse("F24").unwrap().label(), "F24");
        assert!(ShortcutBinding::parse("X").is_err());
        assert!(ShortcutBinding::parse("Ctrl+Shift").is_err());
        assert!(ShortcutBinding::parse("Ctrl+Ctrl+Q").is_err());
    }

    #[test]
    fn no_duplicate_commands() {
        let mut keys = Shortcuts::default();
        assert!(keys.assign(ShortcutAction::Disconnect, keys.stats).is_err());
        assert_eq!(keys.disconnect.label(), "Ctrl+Alt+Shift+Q");
        keys.assign(ShortcutAction::Disconnect, ShortcutBinding::parse("Ctrl+F9").unwrap())
            .unwrap();
        assert_eq!(
            keys.action(
                b'Q' as u16,
                Modifiers {
                    ctrl: true,
                    alt: true,
                    shift: true,
                    meta: false
                }
            ),
            None
        );
        assert_eq!(
            keys.action(
                vk::F1 + 8,
                Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                }
            ),
            Some(ShortcutAction::Disconnect)
        );
    }
}
