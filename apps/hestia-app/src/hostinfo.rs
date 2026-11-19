//! Turns Hermes API answers (free-form JSON, permission sets) into short
//! text for the tools screen.

use hestia_core::api::{HostDiagnostics, PermissionSet, Preflight, StreamReadiness};
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

/// "display_backend" → "Display backend".
fn humanize(id: &str) -> String {
    let text = id.replace(['_', '-'], " ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Health level for a host card: 0 unknown, 1 ready, 2 warnings, 3 not
/// ready; plus the line to show under the host name.
pub fn health(diagnostics: Option<&HostDiagnostics>) -> (i32, Option<String>) {
    let Some(preflight) = diagnostics.and_then(|d| d.preflight.as_ref()) else {
        return (0, None);
    };
    let problems = preflight.problems();
    let first = problems.first().map(|c| {
        if c.message.is_empty() {
            humanize(&c.id)
        } else {
            c.message.clone()
        }
    });
    let more = if problems.len() > 1 {
        format!(" (+{} more)", problems.len() - 1)
    } else {
        String::new()
    };
    match (preflight.ready, first) {
        (true, None) => (1, Some("Ready to stream".into())),
        (true, Some(first)) => (2, Some(format!("Ready, but: {first}{more}"))),
        (false, Some(first)) => (3, Some(format!("Not ready: {first}{more}"))),
        (false, None) => (3, Some("Not ready".into())),
    }
}

/// `(level, title, message)` rows for the tools screen, problems first.
pub fn preflight_rows(preflight: &Preflight) -> Vec<(i32, String, String)> {
    let mut rows: Vec<(i32, String, String)> = preflight
        .checks
        .iter()
        .filter(|c| !c.id.is_empty())
        .map(|c| {
            let level = if c.is_ok() {
                1
            } else if c.is_fail() {
                3
            } else {
                2
            };
            (level, humanize(&c.id), c.message.clone())
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows
}

/// Encoder, sessions, live pipeline and clipboard helper lines.
pub fn status_lines(d: &HostDiagnostics) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(p) = &d.preflight {
        out.push(if p.ready {
            "Hermes reports the host ready to stream.".into()
        } else {
            "Hermes reports the host NOT ready to stream.".into()
        });
    }
    if let Some(rt) = &d.runtime {
        if let Some(enc) = &rt.encoder {
            if enc.probed == Some(false) || enc.name.is_empty() {
                out.push("Encoder: not probed yet".into());
            } else {
                let codecs = if enc.codecs.is_empty() {
                    "unknown".into()
                } else {
                    enc.codecs.join(", ")
                };
                out.push(format!(
                    "Encoder: {} ({}), codecs {codecs}",
                    enc.name.to_uppercase(),
                    if enc.hardware { "hardware" } else { "software" }
                ));
            }
        }
        if let Some(sessions) = &rt.sessions {
            let mut line = format!(
                "Sessions: {} active ({})",
                sessions.active,
                if sessions.streaming { "streaming" } else { "idle" }
            );
            if !sessions.streaming {
                if let Some(why) = sessions.last_termination_text() {
                    line += &format!(", last one ended: {why}");
                }
            }
            out.push(line);
        }
        match &rt.pipeline {
            Some(p) => out.push(format!(
                "Live: {:.0} fps, {:.0} kbps, encode {:.2} ms, capture→encode {:.2} ms, {} encoded / {} dropped",
                p.fps, p.bitrate_kbps, p.encode_ms, p.capture_to_encode_ms, p.frames_encoded, p.frames_dropped
            )),
            None => out.push("Live pipeline metrics appear while a stream runs.".into()),
        }
    }
    if let Some(clip) = d.dependencies.as_ref().and_then(|deps| deps.clipboard.as_ref()) {
        if clip.available {
            out.push("Clipboard helper: ready".into());
        } else {
            let mut line = format!("Clipboard helper missing: {}", clip.diagnostic);
            if !clip.manual_install.is_empty() {
                line += &format!(" — install with: {}", clip.manual_install);
            }
            out.push(line);
        }
    }
    out
}

/// The provenance banner shown once the stream runs.
pub fn stream_banner(r: &StreamReadiness, video: &str, rtt_ms: Option<u32>) -> String {
    if !r.ok {
        let why = if r.failure.is_empty() {
            "unknown reason"
        } else {
            r.failure.as_str()
        };
        return format!("Hermes capture check failed: {why}");
    }
    let mut encoder = r.encoder_name();
    let gpu = r.gpu_name();
    if !gpu.is_empty() {
        encoder = format!("{encoder} on {gpu}");
    }
    let mut parts = vec![r.capture_text(), video.to_owned()];
    if !encoder.is_empty() {
        parts.push(encoder);
    }
    if let Some(rtt) = rtt_ms {
        parts.push(format!("{rtt} ms network latency"));
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
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

    fn diag(v: Value) -> HostDiagnostics {
        HostDiagnostics::from_value(&v)
    }

    #[test]
    fn health_from_preflight() {
        assert_eq!(health(None), (0, None));
        assert_eq!(health(Some(&diag(json!({"ok": true})))), (0, None));
        let ready = diag(json!({"ok": true, "preflight": {"ready": true, "checks": [{"id": "kms", "status": "ok"}]}}));
        assert_eq!(health(Some(&ready)), (1, Some("Ready to stream".into())));
        let warn = diag(json!({"ok": true, "preflight": {"ready": true, "checks": [
            {"id": "clipboard_helper", "status": "warn", "message": ""}]}}));
        assert_eq!(health(Some(&warn)), (2, Some("Ready, but: Clipboard helper".into())));
        let bad = diag(json!({"ok": true, "preflight": {"ready": false, "checks": [
            {"id": "a", "status": "warn", "message": "minor"},
            {"id": "encoder", "status": "fail", "message": "no VAAPI device"}]}}));
        assert_eq!(
            health(Some(&bad)),
            (3, Some("Not ready: no VAAPI device (+1 more)".into()))
        );
        let rows = preflight_rows(bad.preflight.as_ref().unwrap());
        assert_eq!(rows[0], (3, "Encoder".into(), "no VAAPI device".into()));
    }

    #[test]
    fn runtime_lines() {
        let d = diag(json!({"ok": true, "runtime": {
            "encoder": {"probed": true, "name": "vaapi", "hardware": true, "codecs": ["h264"]},
            "sessions": {"active": 0, "streaming": false, "last_termination": "server_stopped"}},
            "dependencies": {"clipboard": {"available": false, "diagnostic": "wl-copy missing", "manualInstall": "pacman -S wl-clipboard"}}}));
        let lines = status_lines(&d);
        assert!(
            lines.contains(&"Encoder: VAAPI (hardware), codecs h264".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"Sessions: 0 active (idle), last one ended: stopped by the host".to_string()));
        assert!(lines.iter().any(|l| l.contains("install with: pacman -S wl-clipboard")));
    }

    #[test]
    fn banner() {
        let r = StreamReadiness {
            ok: true,
            capture: "the detached desktop via KWin screencast".into(),
            encoder: "vaapi".into(),
            gpu: "/dev/dri/renderD128 (amdgpu, AMD 1002:7550)".into(),
            ..Default::default()
        };
        assert_eq!(
            stream_banner(&r, "1920×1080@60 H264", Some(3)),
            "Detached desktop via KWin screencast · 1920×1080@60 H264 · VAAPI on AMD 1002:7550 (amdgpu) · 3 ms network latency"
        );
        let failed = StreamReadiness {
            failure: "no frames".into(),
            ..Default::default()
        };
        assert_eq!(
            stream_banner(&failed, "", None),
            "Hermes capture check failed: no frames"
        );
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
