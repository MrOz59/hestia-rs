//! Configurable local stream shortcuts. These are device preferences, not
//! sent to Hermes and not stored in a host's stream profile.

use serde::{Deserialize, Serialize};

use crate::stream::input::{Modifiers, vk};

/// The function keys a shortcut can use, F1 to F24.
const FUNCTION_KEYS: std::ops::RangeInclusive<u16> = vk::F1..=vk::F1 + 23;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutAction {
    Disconnect,
    Stats,
    MouseCapture,
}

impl ShortcutAction {
    /// Every action, in the order the settings page lists them.
    pub const ALL: [Self; 3] = [Self::Disconnect, Self::Stats, Self::MouseCapture];

    /// By its place in [`ShortcutAction::ALL`].
    pub fn from_index(index: i32) -> Option<Self> {
        usize::try_from(index).ok().and_then(|i| Self::ALL.get(i).copied())
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
        let modifiers = [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.meta, "Meta"),
        ];
        let mut parts: Vec<String> = modifiers
            .into_iter()
            .filter(|(held, _)| *held)
            .map(|(_, name)| name.to_owned())
            .collect();
        parts.push(if FUNCTION_KEYS.contains(&self.key) {
            format!("F{}", self.key - vk::F1 + 1)
        } else if let Some(c) = u8::try_from(self.key).ok().filter(u8::is_ascii_alphanumeric) {
            char::from(c).to_string()
        } else {
            format!("0x{:X}", self.key)
        });
        parts.join("+")
    }

    /// Reads a shortcut as [`ShortcutBinding::label`] writes it (any case,
    /// spaces allowed): modifiers and one key, `Ctrl+Alt+Shift+Q`, `F9`.
    pub fn parse(text: &str) -> Result<Self, &'static str> {
        let mut binding = Self {
            key: 0,
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        };
        let mut key = None;
        for token in text.split('+') {
            let name = token.trim().to_ascii_uppercase();
            let modifier = match name.as_str() {
                "CTRL" | "CONTROL" => Some(&mut binding.ctrl),
                "ALT" => Some(&mut binding.alt),
                "SHIFT" => Some(&mut binding.shift),
                "META" | "SUPER" | "WIN" => Some(&mut binding.meta),
                _ => None,
            };
            if let Some(held) = modifier {
                if *held {
                    return Err("each modifier can appear only once");
                }
                *held = true;
                continue;
            }
            if key.is_some() {
                return Err("a shortcut has only one key");
            }
            key = Some(match name.as_bytes() {
                [c] if c.is_ascii_alphanumeric() => u16::from(*c),
                _ => name
                    .strip_prefix('F')
                    .and_then(|n| n.parse::<u16>().ok())
                    .filter(|n| (1..=24).contains(n))
                    .map(|n| vk::F1 + n - 1)
                    .ok_or("use A-Z, 0-9 or F1-F24 with modifiers")?,
            });
        }
        binding.key = key.ok_or("a shortcut needs a key")?;
        // Shift alone would take capitals and symbols from what is typed.
        if !FUNCTION_KEYS.contains(&binding.key) && !(binding.ctrl || binding.alt || binding.meta) {
            return Err("letters and digits need Ctrl, Alt or Meta");
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
            disconnect: ShortcutBinding::default_for(u16::from(b'Q')),
            stats: ShortcutBinding::default_for(u16::from(b'S')),
            mouse_capture: ShortcutBinding::default_for(u16::from(b'Z')),
        }
    }
}

impl Shortcuts {
    /// The shortcut for `action`.
    pub fn get(&self, action: ShortcutAction) -> ShortcutBinding {
        match action {
            ShortcutAction::Disconnect => self.disconnect,
            ShortcutAction::Stats => self.stats,
            ShortcutAction::MouseCapture => self.mouse_capture,
        }
    }

    fn slot(&mut self, action: ShortcutAction) -> &mut ShortcutBinding {
        match action {
            ShortcutAction::Disconnect => &mut self.disconnect,
            ShortcutAction::Stats => &mut self.stats,
            ShortcutAction::MouseCapture => &mut self.mouse_capture,
        }
    }

    /// The action `key` runs with `mods` held, if any.
    pub fn action(&self, key: u16, mods: Modifiers) -> Option<ShortcutAction> {
        ShortcutAction::ALL
            .into_iter()
            .find(|&action| self.get(action).matches(key, mods))
    }

    /// Sets `action`'s shortcut. A shortcut is never allowed to shadow a
    /// different Hestia command.
    pub fn assign(&mut self, action: ShortcutAction, binding: ShortcutBinding) -> Result<(), &'static str> {
        if ShortcutAction::ALL
            .into_iter()
            .any(|other| other != action && self.get(other) == binding)
        {
            return Err("another Hestia action already uses that shortcut");
        }
        *self.slot(action) = binding;
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
        assert_eq!(ShortcutBinding::parse("Meta+f").unwrap().label(), "Meta+F");
        assert_eq!(ShortcutBinding::parse("Alt+7").unwrap().label(), "Alt+7");
        assert!(ShortcutBinding::parse("X").is_err());
        assert!(ShortcutBinding::parse("Shift+X").is_err());
        assert!(ShortcutBinding::parse("Ctrl+Shift").is_err());
        assert!(ShortcutBinding::parse("Ctrl+Ctrl+Q").is_err());
        assert!(ShortcutBinding::parse("Ctrl+Q+W").is_err());
        assert!(ShortcutBinding::parse("Ctrl+F25").is_err());
        assert!(ShortcutBinding::parse("Ctrl+").is_err());
        assert!(ShortcutBinding::parse("").is_err());
    }

    #[test]
    fn labels_parse_back() {
        for action in ShortcutAction::ALL {
            let binding = Shortcuts::default().get(action);
            assert_eq!(ShortcutBinding::parse(&binding.label()), Ok(binding));
        }
    }

    #[test]
    fn actions_by_index() {
        assert_eq!(ShortcutAction::from_index(0), Some(ShortcutAction::Disconnect));
        assert_eq!(ShortcutAction::from_index(2), Some(ShortcutAction::MouseCapture));
        assert_eq!(ShortcutAction::from_index(3), None);
        assert_eq!(ShortcutAction::from_index(-1), None);
    }

    #[test]
    fn no_duplicate_commands() {
        let mut keys = Shortcuts::default();
        assert!(keys.assign(ShortcutAction::Disconnect, keys.stats).is_err());
        assert_eq!(keys.disconnect.label(), "Ctrl+Alt+Shift+Q");
        // Its own shortcut again is fine.
        keys.assign(ShortcutAction::Stats, keys.stats).unwrap();
        keys.assign(ShortcutAction::Disconnect, ShortcutBinding::parse("Ctrl+F9").unwrap())
            .unwrap();
        assert_eq!(
            keys.action(
                u16::from(b'Q'),
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
