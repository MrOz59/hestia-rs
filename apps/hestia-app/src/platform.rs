//! Platform bits the UI needs: where reports go and how to get them out
//! (Android share sheet, desktop file manager).

use std::path::{Path, PathBuf};
#[cfg(target_os = "android")]
use std::sync::OnceLock;

#[cfg(target_os = "android")]
static ANDROID_APP: OnceLock<slint::android::AndroidApp> = OnceLock::new();

#[cfg(target_os = "android")]
pub fn set_android_app(app: slint::android::AndroidApp) {
    let _ = ANDROID_APP.set(app);
}

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

/// `Intent.ACTION_SEND` with plain text through a chooser.
#[cfg(target_os = "android")]
fn share_text(subject: &str, text: &str) -> Result<(), String> {
    use jni::JavaVM;
    use jni::objects::{JObject, JValue};

    let app = ANDROID_APP.get().ok_or("Android context not available")?;
    // SAFETY: android-activity hands out the process' JavaVM and a global
    // reference to the Activity, both valid for the life of the app.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };

    let result = (|| -> jni::errors::Result<()> {
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
        let title = env.new_string("Share stream report")?;
        let chooser = env
            .call_static_method(
                "android/content/Intent",
                "createChooser",
                "(Landroid/content/Intent;Ljava/lang/CharSequence;)Landroid/content/Intent;",
                &[JValue::Object(&intent), JValue::Object(&title)],
            )?
            .l()?;
        env.call_method(
            &activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[JValue::Object(&chooser)],
        )?;
        Ok(())
    })();
    if let Err(e) = result {
        // A pending Java exception would abort the next JNI call.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        return Err(format!("could not open the share sheet: {e}"));
    }
    Ok(())
}
