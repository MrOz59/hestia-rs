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

/// Runs `f` with a JNI environment and the Activity, clearing any Java
/// exception it leaves behind (a pending one would abort the next JNI call).
#[cfg(target_os = "android")]
fn with_activity(
    f: impl FnOnce(&mut jni::JNIEnv<'_>, &jni::objects::JObject<'_>) -> jni::errors::Result<()>,
) -> Result<(), String> {
    use jni::JavaVM;
    use jni::objects::JObject;

    let app = ANDROID_APP.get().ok_or("Android context not available")?;
    // SAFETY: android-activity hands out the process' JavaVM and a global
    // reference to the Activity, both valid for the life of the app.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }.map_err(|e| e.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|e| e.to_string())?;
    let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
    let result = f(&mut *env, &activity);
    if result.is_err() && env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    result.map_err(|e| e.to_string())
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

/// This device's display as `(width, height, refresh)`, landscape, for the
/// streaming presets. Looked up once; `None` when it can't be told.
pub fn native_display() -> Option<(u32, u32, u32)> {
    static NATIVE: std::sync::OnceLock<Option<(u32, u32, u32)>> = std::sync::OnceLock::new();
    *NATIVE.get_or_init(|| {
        let found = detect_display().map(|(w, h, hz)| (w.max(h), w.min(h), hz));
        log::info!("native display: {found:?}");
        found
    })
}

/// Largest connected screen, from the kernel's DRM connectors: the EDID's
/// preferred mode, or the first listed mode at 60 Hz without one.
#[cfg(not(target_os = "android"))]
fn detect_display() -> Option<(u32, u32, u32)> {
    let mut best: Option<(u32, u32, u32)> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let dir = entry.path();
        let connected = std::fs::read_to_string(dir.join("status")).is_ok_and(|s| s.trim() == "connected");
        if !connected {
            continue;
        }
        let from_edid = std::fs::read(dir.join("edid"))
            .ok()
            .and_then(|edid| hestia_core::presets::edid_preferred_mode(&edid));
        let mode = from_edid.or_else(|| {
            let modes = std::fs::read_to_string(dir.join("modes")).ok()?;
            let (w, h) = modes.lines().next()?.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().trim_end_matches('i').parse().ok()?, 60))
        });
        if let Some(mode) = mode
            && best.is_none_or(|b| u64::from(mode.0) * u64::from(mode.1) > u64::from(b.0) * u64::from(b.1))
        {
            best = Some(mode);
        }
    }
    best
}

/// `Display.getMode()`: the physical panel size, with the highest refresh
/// rate the panel offers at that size.
#[cfg(target_os = "android")]
fn detect_display() -> Option<(u32, u32, u32)> {
    let mut found = None;
    let result = with_activity(|env, activity| {
        let wm = env
            .call_method(activity, "getWindowManager", "()Landroid/view/WindowManager;", &[])?
            .l()?;
        let display = env
            .call_method(&wm, "getDefaultDisplay", "()Landroid/view/Display;", &[])?
            .l()?;
        let mode = env
            .call_method(&display, "getMode", "()Landroid/view/Display$Mode;", &[])?
            .l()?;
        let width = env.call_method(&mode, "getPhysicalWidth", "()I", &[])?.i()?;
        let height = env.call_method(&mode, "getPhysicalHeight", "()I", &[])?.i()?;
        let mut refresh = env.call_method(&mode, "getRefreshRate", "()F", &[])?.f()?;
        let modes = env
            .call_method(&display, "getSupportedModes", "()[Landroid/view/Display$Mode;", &[])?
            .l()?;
        let modes = jni::objects::JObjectArray::from(modes);
        for i in 0..env.get_array_length(&modes)? {
            let other = env.get_object_array_element(&modes, i)?;
            let w = env.call_method(&other, "getPhysicalWidth", "()I", &[])?.i()?;
            let h = env.call_method(&other, "getPhysicalHeight", "()I", &[])?.i()?;
            if w == width && h == height {
                refresh = refresh.max(env.call_method(&other, "getRefreshRate", "()F", &[])?.f()?);
            }
            env.delete_local_ref(other)?;
        }
        if width > 0 && height > 0 {
            found = Some((width as u32, height as u32, refresh.round().max(1.0) as u32));
        }
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("reading the display mode failed: {e}");
    }
    found
}
