//! The stream's keyboard bar: a text field (which brings up the soft keyboard
//! on phones) whose edits are replayed on the host, plus shortcut keys a
//! touch keyboard doesn't have.

use hestia_core::stream::InputEvent;
use hestia_core::stream::input::{Modifiers, vk};

/// What changed between two states of the text field: how many characters
/// were erased from the end of the common prefix, and what was typed after it.
pub fn text_diff(old: &str, new: &str) -> (usize, String) {
    let common = old.chars().zip(new.chars()).take_while(|(a, b)| a == b).count();
    let erased = old.chars().count() - common;
    let typed: String = new.chars().skip(common).collect();
    (erased, typed)
}

fn key(key: u16, pressed: bool, modifiers: Modifiers) -> InputEvent {
    InputEvent::Key {
        key,
        pressed,
        modifiers,
    }
}

/// Press and release of `k` (with `modifiers` reported as held).
fn tap(k: u16, modifiers: Modifiers) -> [InputEvent; 2] {
    [key(k, true, modifiers), key(k, false, modifiers)]
}

/// Events that replay a text-field edit on the host.
pub fn edit_events(old: &str, new: &str) -> Vec<InputEvent> {
    let (erased, typed) = text_diff(old, new);
    let mut out = Vec::new();
    for _ in 0..erased {
        out.extend(tap(vk::BACK, Modifiers::default()));
    }
    if !typed.is_empty() {
        out.push(InputEvent::Text(typed));
    }
    out
}

/// Shortcut buttons of the keyboard bar, by name.
pub const SPECIAL_KEYS: [(&str, &str); 7] = [
    ("esc", "Esc"),
    ("tab", "Tab"),
    ("enter", "Enter"),
    ("win", "Win"),
    ("alt-tab", "Alt+Tab"),
    ("alt-f4", "Alt+F4"),
    ("backspace", "⌫"),
];

pub fn special_key_events(name: &str) -> Vec<InputEvent> {
    let none = Modifiers::default();
    let alt = Modifiers {
        alt: true,
        ..Modifiers::default()
    };
    let with_alt = |k: u16| {
        let mut v = vec![key(vk::LMENU, true, alt)];
        v.extend(tap(k, alt));
        v.push(key(vk::LMENU, false, none));
        v
    };
    match name {
        "esc" => tap(vk::ESCAPE, none).to_vec(),
        "tab" => tap(vk::TAB, none).to_vec(),
        "enter" => tap(vk::RETURN, none).to_vec(),
        "backspace" => tap(vk::BACK, none).to_vec(),
        "win" => tap(vk::LWIN, none).to_vec(),
        "alt-tab" => with_alt(vk::TAB),
        "alt-f4" => with_alt(vk::F1 + 3),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs() {
        assert_eq!(text_diff("", "abc"), (0, "abc".into()));
        assert_eq!(text_diff("abc", "abcd"), (0, "d".into()));
        assert_eq!(text_diff("abcd", "ab"), (2, "".into()));
        assert_eq!(text_diff("abc", "abX"), (1, "X".into()));
        // Autocorrect replacing a word.
        assert_eq!(text_diff("teh ", "the "), (3, "he ".into()));
        assert_eq!(text_diff("ação", "açõ"), (2, "õ".into()));
    }

    #[test]
    fn edits_become_backspaces_and_text() {
        let ev = edit_events("abc", "abX");
        assert_eq!(ev.len(), 3);
        assert!(matches!(
            ev[0],
            InputEvent::Key {
                key: vk::BACK,
                pressed: true,
                ..
            }
        ));
        assert!(matches!(
            ev[1],
            InputEvent::Key {
                key: vk::BACK,
                pressed: false,
                ..
            }
        ));
        assert!(matches!(&ev[2], InputEvent::Text(t) if t == "X"));
        assert!(edit_events("same", "same").is_empty());
    }

    #[test]
    fn alt_tab_holds_alt_around_tab() {
        let ev = special_key_events("alt-tab");
        let keys: Vec<(u16, bool, bool)> = ev
            .iter()
            .map(|e| match e {
                InputEvent::Key {
                    key,
                    pressed,
                    modifiers,
                } => (*key, *pressed, modifiers.alt),
                _ => panic!("not a key"),
            })
            .collect();
        assert_eq!(
            keys,
            [
                (vk::LMENU, true, true),
                (vk::TAB, true, true),
                (vk::TAB, false, true),
                (vk::LMENU, false, false)
            ]
        );
        assert!(matches!(
            special_key_events("alt-f4")[1],
            InputEvent::Key { key: 0x73, .. }
        ));
    }

    #[test]
    fn every_listed_key_does_something() {
        for (name, _) in SPECIAL_KEYS {
            assert!(!special_key_events(name).is_empty(), "{name}");
        }
        assert!(special_key_events("nope").is_empty());
    }
}
