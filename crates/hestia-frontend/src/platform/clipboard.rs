//! Reads and writes the local system clipboard.

#[cfg(target_os = "android")]
use super::android::with_helper;

/// When the local clipboard last changed, where that can be told without
/// reading it: Android (reading another app's clip shows a "pasted from
/// your clipboard" notice there, so the text is read only when this moves).
/// None: read the text to find out (desktop).
pub fn clipboard_stamp() -> Option<i64> {
    #[cfg(target_os = "android")]
    {
        let mut stamp = 0;
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            stamp = env
                .call_static_method(
                    helper,
                    "clipboardStamp",
                    "(Landroid/app/Activity;)J",
                    &[JValue::Object(activity)],
                )?
                .j()?;
            Ok(())
        });
        if let Err(e) = result {
            log::debug!("clipboard state unavailable: {e}");
        }
        Some(stamp)
    }
    #[cfg(not(target_os = "android"))]
    None
}

/// The local clipboard's text, if it holds any.
pub fn clipboard_text() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        let mut text = None;
        let result = with_helper(|env, activity, helper| {
            use jni::objects::{JString, JValue};
            let value = env
                .call_static_method(
                    helper,
                    "clipboardText",
                    "(Landroid/app/Activity;)Ljava/lang/String;",
                    &[JValue::Object(activity)],
                )?
                .l()?;
            if !value.is_null() {
                text = Some(env.get_string(&JString::from(value))?.into());
            }
            Ok(())
        });
        if let Err(e) = result {
            log::debug!("reading the clipboard failed: {e}");
        }
        text
    }
    #[cfg(not(target_os = "android"))]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let output = if wayland {
            std::process::Command::new("wl-paste")
                .args(["--no-newline", "--type", "text"])
                .output()
        } else {
            std::process::Command::new("xclip")
                .args(["-selection", "clipboard", "-o"])
                .output()
        };
        match output {
            Ok(out) if out.status.success() => String::from_utf8(out.stdout).ok(),
            Ok(_) => None,
            Err(e) => {
                log::debug!("reading the clipboard failed (wl-clipboard / xclip installed?): {e}");
                None
            }
        }
    }
}

/// Puts `text` on the local clipboard.
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            let value = env.new_string(text)?;
            env.call_static_method(
                helper,
                "setClipboardText",
                "(Landroid/app/Activity;Ljava/lang/String;)V",
                &[JValue::Object(activity), JValue::Object(&value)],
            )?;
            Ok(())
        })
    }
    #[cfg(not(target_os = "android"))]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let mut command = if wayland {
            Command::new("wl-copy")
        } else {
            let mut c = Command::new("xclip");
            c.args(["-selection", "clipboard"]);
            c
        };
        // Both stay in the background serving the text; the parent exits.
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{} not available: {e}", if wayland { "wl-copy" } else { "xclip" }))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
        child.wait().map_err(|e| e.to_string())?;
        Ok(())
    }
}
