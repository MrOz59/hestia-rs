//! Platform actions for exporting reports and opening links.

use std::path::{Path, PathBuf};

#[cfg(target_os = "android")]
use super::android::{ANDROID_APP, with_activity};

/// Where stream reports are saved. On Android that is the app's external
/// files directory (`Android/data/com.mroz59.hestia.rs/files/reports`,
/// reachable over USB); elsewhere `<data dir>/reports`.
pub fn reports_dir(data_dir: &Path) -> PathBuf {
    #[cfg(target_os = "android")]
    if let Some(dir) = ANDROID_APP.get().and_then(|app| app.external_data_path()) {
        return dir.join("reports");
    }
    data_dir.join("reports")
}

/// Label of the button that gets a report out of the app.
pub fn export_label() -> &'static str {
    if cfg!(target_os = "android") {
        "Share"
    } else {
        "Open folder"
    }
}

/// Opens the system share sheet (Android) with `text`, or the folder that
/// holds `file` (desktop).
pub fn export_report(subject: &str, text: &str, file: Option<&Path>) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let _ = file;
        share_text(subject, text)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (subject, text);
        let dir = file
            .and_then(Path::parent)
            .ok_or("the report was not saved to a file")?;
        open_path(dir)
    }
}

#[cfg(not(target_os = "android"))]
fn open_path(path: &Path) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(path)
        .spawn()
        .map(drop)
        .map_err(|e| format!("could not run {opener}: {e}"))
}

/// Opens `url` in the browser.
pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        with_activity(|env, activity| {
            use jni::objects::JValue;
            let text = env.new_string(url)?;
            let uri = env
                .call_static_method(
                    "android/net/Uri",
                    "parse",
                    "(Ljava/lang/String;)Landroid/net/Uri;",
                    &[JValue::Object(&text)],
                )?
                .l()?;
            let action = env.new_string("android.intent.action.VIEW")?;
            let intent = env.new_object(
                "android/content/Intent",
                "(Ljava/lang/String;Landroid/net/Uri;)V",
                &[JValue::Object(&action), JValue::Object(&uri)],
            )?;
            env.call_method(
                activity,
                "startActivity",
                "(Landroid/content/Intent;)V",
                &[JValue::Object(&intent)],
            )?;
            Ok(())
        })
        .map_err(|e| format!("could not open the browser: {e}"))
    }
    #[cfg(not(target_os = "android"))]
    {
        open_path(Path::new(url))
    }
}

/// `Intent.ACTION_SEND` with plain text through a chooser.
#[cfg(target_os = "android")]
fn share_text(subject: &str, text: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        use jni::objects::JValue;
        let action = env.new_string("android.intent.action.SEND")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&action)],
        )?;
        let mime = env.new_string("text/plain")?;
        env.call_method(
            &intent,
            "setType",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&mime)],
        )?;
        for (key, value) in [
            ("android.intent.extra.SUBJECT", subject),
            ("android.intent.extra.TEXT", text),
        ] {
            let key = env.new_string(key)?;
            let value = env.new_string(value)?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                &[JValue::Object(&key), JValue::Object(&value)],
            )?;
        }
        let title = env.new_string("Share report")?;
        let chooser = env
            .call_static_method(
                "android/content/Intent",
                "createChooser",
                "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
                &[JValue::Object(&intent), JValue::Object(&title)],
            )?
            .l()?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[JValue::Object(&chooser)],
        )?;
        Ok(())
    })
    .map_err(|e| format!("could not open the share sheet: {e}"))
}
