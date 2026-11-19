//! Turns Hermes API answers (free-form JSON, permission sets) into short
//! text for the tools screen.

use hestia_core::api::PermissionSet;
use serde_json::Value;

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("–".into()),
        Value::Bool(b) => Some(if *b { "yes" } else { "no" }.into()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(if s.is_empty() { "–".into() } else { s.clone() }),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn walk(prefix: &str, value: &Value, depth: usize, out: &mut Vec<String>, max: usize) {
    if out.len() >= max {
        return;
    }
    if let Some(text) = scalar(value) {
        out.push(format!("{prefix}: {text}"));
        return;
    }
    match value {
        Value::Array(items) => {
            if items.iter().all(|v| scalar(v).is_some()) && items.len() <= 8 {
                let joined: Vec<String> = items.iter().filter_map(scalar).collect();
                out.push(format!(
                    "{prefix}: {}",
                    if joined.is_empty() {
                        "–".into()
                    } else {
                        joined.join(", ")
                    }
                ));
            } else {
                out.push(format!("{prefix}: {} items", items.len()));
            }
        }
        Value::Object(map) if depth < 3 => {
            for (key, v) in map {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk(&path, v, depth + 1, out, max);
            }
        }
        Value::Object(map) => out.push(format!("{prefix}: {} fields", map.len())),
        _ => {}
    }
}

/// `path: value` lines for a JSON document (nested objects flattened with
/// dots, long arrays summarised), at most `max` lines.
pub fn json_lines(value: &Value, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk("", value, 0, &mut out, max);
    out
}

/// One line with the first `max` fields, for status summaries.
pub fn json_summary(value: &Value, max: usize) -> String {
    let lines = json_lines(value, max);
    if lines.is_empty() {
        "No details reported".into()
    } else {
        lines.join(" · ")
    }
}

pub fn permission_summary(p: &PermissionSet) -> String {
    let all = [
        (p.view_streams, "view streams"),
        (p.list_apps, "list apps"),
        (p.launch_apps, "launch apps"),
        (p.keyboard_input, "keyboard"),
        (p.mouse_input, "mouse"),
        (p.gamepad_input, "gamepads"),
        (p.virtual_display, "virtual display"),
        (p.server_commands, "server commands"),
        (p.clipboard_sync, "clipboard"),
        (p.display_recovery, "display recovery"),
    ];
    let allowed: Vec<&str> = all.iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect();
    let denied: Vec<&str> = all.iter().filter(|(on, _)| !*on).map(|(_, n)| *n).collect();
    let mut text = if allowed.is_empty() {
        "Nothing is allowed for this device.".to_owned()
    } else {
        format!("Allowed: {}.", allowed.join(", "))
    };
    if !denied.is_empty() && !allowed.is_empty() {
        text += &format!(" Not allowed: {}.", denied.join(", "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattens_json() {
        let v = json!({
            "version": "1.4.0",
            "display": {"backend": "kms", "ok": true, "size": [1920, 1080]},
            "sessions": [{"id": 1}, {"id": 2}],
            "note": null
        });
        let lines = json_lines(&v, 10);
        assert!(lines.contains(&"version: 1.4.0".to_string()), "{lines:?}");
        assert!(lines.contains(&"display.backend: kms".to_string()));
        assert!(lines.contains(&"display.ok: yes".to_string()));
        assert!(lines.contains(&"display.size: 1920, 1080".to_string()));
        assert!(lines.contains(&"sessions: 2 items".to_string()));
        assert!(lines.contains(&"note: –".to_string()));
        assert_eq!(json_lines(&v, 2).len(), 2);
        assert_eq!(json_summary(&json!({}), 3), "No details reported");
    }

    #[test]
    fn summarises_permissions() {
        let p = PermissionSet {
            view_streams: true,
            launch_apps: true,
            ..Default::default()
        };
        let s = permission_summary(&p);
        assert!(s.starts_with("Allowed: view streams, launch apps."), "{s}");
        assert!(s.contains("Not allowed: list apps"));
        assert_eq!(
            permission_summary(&PermissionSet::default()),
            "Nothing is allowed for this device."
        );
    }
}
