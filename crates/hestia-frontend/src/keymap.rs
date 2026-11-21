//! Slint key events → GameStream virtual-key codes.
//!
//! Slint reports keys as text: printable characters as themselves, named
//! keys as control characters or private-use code points (the `Key`
//! namespace, following the NSEvent function-key convention). Modifier keys
//! arrive as separate events, so a shifted character maps to its unshifted
//! key and the host applies the Shift it already received.
//!
//! Characters are mapped by their US-layout key; anything without a key
//! (accented letters, IME output) is sent as text on key press.

use hestia_core::stream::input::vk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapped {
    Key(u16),
    Text(String),
}

fn special(c: char) -> Option<u16> {
    Some(match c {
        '\u{0008}' => vk::BACK,
        '\u{0009}' | '\u{0019}' => vk::TAB, // Tab, Backtab (Shift+Tab)
        '\u{000a}' | '\u{000d}' => vk::RETURN,
        '\u{001b}' => vk::ESCAPE,
        '\u{007f}' => vk::DELETE,
        '\u{0010}' => vk::LSHIFT,
        '\u{0011}' => vk::LCONTROL,
        '\u{0012}' => vk::LMENU,
        '\u{0013}' => vk::RMENU, // AltGr
        '\u{0014}' => vk::CAPITAL,
        '\u{0015}' => vk::RSHIFT,
        '\u{0016}' => vk::RCONTROL,
        '\u{0017}' => vk::LWIN,
        '\u{0018}' => vk::RWIN,
        '\u{F700}' => vk::UP,
        '\u{F701}' => vk::DOWN,
        '\u{F702}' => vk::LEFT,
        '\u{F703}' => vk::RIGHT,
        // F1–F24
        c @ '\u{F704}'..='\u{F71B}' => vk::F1 + (c as u16 - 0xF704),
        '\u{F727}' => vk::INSERT,
        '\u{F729}' => vk::HOME,
        '\u{F72B}' => vk::END,
        '\u{F72C}' => vk::PRIOR,
        '\u{F72D}' => vk::NEXT,
        '\u{F72E}' => vk::SNAPSHOT,
        '\u{F72F}' => 0x91, // VK_SCROLL
        '\u{F730}' => vk::PAUSE,
        '\u{F735}' => vk::APPS, // Menu
        _ => return None,
    })
}

/// Maps the `text` of a Slint key event.
pub fn map(text: &str) -> Option<Mapped> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        // Composed input (dead keys, IME).
        return Some(Mapped::Text(text.to_owned()));
    }
    if let Some(key) = special(first) {
        return Some(Mapped::Key(key));
    }
    if let Some((key, _implied_shift)) = vk::from_char(first) {
        return Some(Mapped::Key(key));
    }
    if first.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&first) {
        return None;
    }
    Some(Mapped::Text(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_named_keys() {
        assert_eq!(map("\u{F702}"), Some(Mapped::Key(vk::LEFT)));
        assert_eq!(map("\u{F704}"), Some(Mapped::Key(vk::F1)));
        assert_eq!(map("\u{F70F}"), Some(Mapped::Key(vk::F1 + 11)));
        assert_eq!(map("\n"), Some(Mapped::Key(vk::RETURN)));
        assert_eq!(map("\u{1b}"), Some(Mapped::Key(vk::ESCAPE)));
        assert_eq!(map("\u{10}"), Some(Mapped::Key(vk::LSHIFT)));
    }

    #[test]
    fn maps_characters_to_unshifted_keys() {
        assert_eq!(map("a"), Some(Mapped::Key(b'A' as u16)));
        assert_eq!(map("A"), Some(Mapped::Key(b'A' as u16)));
        assert_eq!(map("!"), Some(Mapped::Key(b'1' as u16)));
        assert_eq!(map(" "), Some(Mapped::Key(vk::SPACE)));
        assert_eq!(map("?"), Some(Mapped::Key(vk::OEM_2)));
    }

    #[test]
    fn falls_back_to_text() {
        assert_eq!(map("ç"), Some(Mapped::Text("ç".into())));
        assert_eq!(map("é"), Some(Mapped::Text("é".into())));
        assert_eq!(map("ab"), Some(Mapped::Text("ab".into())));
        assert_eq!(map(""), None);
        assert_eq!(map("\u{F7FF}"), None);
    }
}
