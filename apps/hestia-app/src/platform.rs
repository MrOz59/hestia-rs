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

/// Called before each connection to a host (see [`crate::netroute`]): binds
/// the app to the Wi-Fi/Ethernet network that holds a LAN address when that
/// network isn't Android's default (Wi-Fi without internet → mobile data is
/// the default and LAN connections would time out). Checked at most every
/// few seconds per address.
#[cfg(target_os = "android")]
pub fn route_for(addr: &std::net::SocketAddr) {
    use std::net::IpAddr;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static LAST: Mutex<Option<(IpAddr, Instant)>> = Mutex::new(None);
    let ip = addr.ip();
    {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((prev, at)) = *last
            && prev == ip
            && at.elapsed() < Duration::from_secs(10)
        {
            return;
        }
        *last = Some((ip, Instant::now()));
    }
    if let Err(e) = apply_route(ip) {
        log::warn!("choosing the network for {ip} failed: {e}");
    }
}

#[cfg(target_os = "android")]
fn apply_route(ip: std::net::IpAddr) -> Result<(), String> {
    use jni::objects::{JByteArray, JObject, JObjectArray, JValue};

    use crate::netroute::{self, NetInfo};

    const TRANSPORT_CELLULAR: i32 = 0;
    const TRANSPORT_VPN: i32 = 4;
    const NETWORK_SIG: &str = "(Landroid/net/Network;)";

    with_activity(|env, activity| {
        let name = env.new_string("connectivity")?;
        let cm = env
            .call_method(
                activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[JValue::Object(&name)],
            )?
            .l()?;
        let active = env
            .call_method(&cm, "getActiveNetwork", "()Landroid/net/Network;", &[])?
            .l()?;
        let all = JObjectArray::from(
            env.call_method(&cm, "getAllNetworks", "()[Landroid/net/Network;", &[])?
                .l()?,
        );
        let mut infos = Vec::new();
        let mut handles = Vec::new();
        for i in 0..env.get_array_length(&all)? {
            let net = env.get_object_array_element(&all, i)?;
            let caps = env
                .call_method(
                    &cm,
                    "getNetworkCapabilities",
                    format!("{NETWORK_SIG}Landroid/net/NetworkCapabilities;"),
                    &[JValue::Object(&net)],
                )?
                .l()?;
            let props = env
                .call_method(
                    &cm,
                    "getLinkProperties",
                    format!("{NETWORK_SIG}Landroid/net/LinkProperties;"),
                    &[JValue::Object(&net)],
                )?
                .l()?;
            if caps.is_null() || props.is_null() {
                continue;
            }
            let cellular = env
                .call_method(&caps, "hasTransport", "(I)Z", &[JValue::Int(TRANSPORT_CELLULAR)])?
                .z()?;
            let vpn = env
                .call_method(&caps, "hasTransport", "(I)Z", &[JValue::Int(TRANSPORT_VPN)])?
                .z()?;
            let list = env
                .call_method(&props, "getLinkAddresses", "()Ljava/util/List;", &[])?
                .l()?;
            let mut addresses = Vec::new();
            for j in 0..env.call_method(&list, "size", "()I", &[])?.i()? {
                let link = env
                    .call_method(&list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(j)])?
                    .l()?;
                let inet = env
                    .call_method(&link, "getAddress", "()Ljava/net/InetAddress;", &[])?
                    .l()?;
                let prefix = env.call_method(&link, "getPrefixLength", "()I", &[])?.i()?;
                let raw = JByteArray::from(env.call_method(&inet, "getAddress", "()[B", &[])?.l()?);
                if let Some(address) = netroute::ip_from_bytes(&env.convert_byte_array(&raw)?) {
                    addresses.push((address, prefix.clamp(0, 128) as u8));
                }
                env.delete_local_ref(raw)?;
                env.delete_local_ref(inet)?;
                env.delete_local_ref(link)?;
            }
            let is_active = !active.is_null()
                && env
                    .call_method(&net, "equals", "(Ljava/lang/Object;)Z", &[JValue::Object(&active)])?
                    .z()?;
            infos.push(NetInfo {
                local_transport: !cellular && !vpn,
                active: is_active,
                addresses,
            });
            handles.push(net);
        }

        let null = JObject::null();
        let choice = netroute::choose(ip, &infos);
        let target = choice.map_or(&null, |i| &handles[i]);
        let bound = env
            .call_method(&cm, "getBoundNetworkForProcess", "()Landroid/net/Network;", &[])?
            .l()?;
        let unchanged = match (bound.is_null(), target.is_null()) {
            (true, true) => true,
            (false, false) => env
                .call_method(&bound, "equals", "(Ljava/lang/Object;)Z", &[JValue::Object(target)])?
                .z()?,
            _ => false,
        };
        if !unchanged {
            let ok = env
                .call_method(
                    &cm,
                    "bindProcessToNetwork",
                    format!("{NETWORK_SIG}Z"),
                    &[JValue::Object(target)],
                )?
                .z()?;
            match choice {
                Some(i) => log::info!(
                    "{ip} is on a local network that isn't Android's default (Wi-Fi without internet?); \
                     routing the app through it ({:?}): {}",
                    infos[i].addresses,
                    if ok { "ok" } else { "refused" }
                ),
                None => log::info!("routing back through Android's default network for {ip}"),
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Streaming mode and the soft keyboard (Android: HestiaHelper.java)
// ---------------------------------------------------------------------------

/// What the soft keyboard sent that isn't a plain key event.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeEvent {
    /// Committed text (accents, emoji, swipe words…).
    Text(String),
    /// Characters deleted before the cursor: that many Backspaces.
    DeleteBefore(usize),
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
static IME_SINK: std::sync::OnceLock<Box<dyn Fn(ImeEvent) + Send + Sync>> = std::sync::OnceLock::new();

/// Where keyboard text goes (the controller sends it to the stream).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn set_ime_sink(sink: impl Fn(ImeEvent) + Send + Sync + 'static) {
    let _ = IME_SINK.set(Box::new(sink));
}

/// Whether this platform has an on-screen keyboard to show.
pub fn has_soft_keyboard() -> bool {
    cfg!(target_os = "android")
}

/// The phone or tablet, for reports: "samsung SM-X710, Android 14 (SDK 34),
/// SoC QTI SM8550 (kalama)". None off Android.
pub fn device_summary() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        fn prop(name: &str) -> String {
            use std::ffi::{CStr, CString, c_char, c_int};
            unsafe extern "C" {
                fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
            }
            let Ok(name) = CString::new(name) else {
                return String::new();
            };
            // PROP_VALUE_MAX is 92 bytes, terminator included.
            let mut value = [0 as c_char; 92];
            // SAFETY: a NUL-terminated name and a PROP_VALUE_MAX buffer.
            let len = unsafe { __system_property_get(name.as_ptr(), value.as_mut_ptr()) };
            if len <= 0 {
                return String::new();
            }
            // SAFETY: bionic NUL-terminates the value within the buffer.
            unsafe { CStr::from_ptr(value.as_ptr()) }
                .to_string_lossy()
                .trim()
                .to_owned()
        }
        let join = |parts: &[String]| {
            parts
                .iter()
                .filter(|p| !p.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let device = join(&[prop("ro.product.manufacturer"), prop("ro.product.model")]);
        let soc = join(&[prop("ro.soc.manufacturer"), prop("ro.soc.model")]);
        let board = prop("ro.board.platform");
        let soc = match (soc.is_empty(), board.is_empty()) {
            (false, false) => format!(", SoC {soc} ({board})"),
            (false, true) => format!(", SoC {soc}"),
            (true, false) => format!(", SoC {board}"),
            (true, true) => String::new(),
        };
        Some(format!(
            "{device}, Android {} (SDK {}){soc}",
            prop("ro.build.version.release"),
            prop("ro.build.version.sdk")
        ))
    }
    #[cfg(not(target_os = "android"))]
    None
}

/// Streaming on/off: on Android, the chosen orientation, system bars
/// hidden, screen kept on. Nothing on desktop (the window's full screen is
/// its own toggle).
pub fn set_stream_mode(on: bool, orientation: hestia_core::StreamOrientation) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            let orientation = match orientation {
                hestia_core::StreamOrientation::Landscape => 0,
                hestia_core::StreamOrientation::Portrait => 1,
                hestia_core::StreamOrientation::Auto => 2,
            };
            env.call_static_method(
                helper,
                "setStreamMode",
                "(Landroid/app/Activity;ZI)V",
                &[
                    JValue::Object(activity),
                    JValue::Bool(u8::from(on)),
                    JValue::Int(orientation),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("switching stream mode failed: {e}");
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = (on, orientation);
}

/// Shows or hides the on-screen keyboard that types into the host.
pub fn set_keyboard_visible(visible: bool) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            env.call_static_method(
                helper,
                "setKeyboardVisible",
                "(Landroid/app/Activity;Z)V",
                &[JValue::Object(activity), JValue::Bool(u8::from(visible))],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("toggling the keyboard failed: {e}");
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = visible;
}

/// TV remotes: whether the D-pad's up/down move the focus between controls
/// (form screens) or stay arrow keys (grids, the stream).
pub fn set_form_navigation(on: bool) {
    #[cfg(target_os = "android")]
    i_slint_backend_android_activity::set_dpad_focus_navigation(on);
    #[cfg(not(target_os = "android"))]
    let _ = on;
}

// ---------------------------------------------------------------------------
// Clipboard (for syncing with the host)

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

#[cfg(target_os = "android")]
static HELPER: std::sync::OnceLock<jni::objects::GlobalRef> = std::sync::OnceLock::new();

/// Runs `f` with HestiaHelper's class, loading it on first use.
#[cfg(target_os = "android")]
fn with_helper(
    f: impl FnOnce(&mut jni::JNIEnv<'_>, &jni::objects::JObject<'_>, &jni::objects::JClass<'_>) -> jni::errors::Result<()>,
) -> Result<(), String> {
    with_activity(|env, activity| {
        if HELPER.get().is_none() {
            let class = load_helper(env, activity)?;
            let _ = HELPER.set(class);
        }
        let Some(global) = HELPER.get() else {
            return Ok(());
        };
        let class = jni::objects::JClass::from(env.new_local_ref(global.as_obj())?);
        f(env, activity, &class)
    })
}

/// Candidates ordered for Auto. Java can verify size/rate and hardware
/// support on the Android API levels covered by the app.
#[cfg(target_os = "android")]
pub fn video_decoders(width: u32, height: u32, fps: u32) -> Result<Vec<(hestia_media::VideoCodec, String)>, String> {
    use hestia_media::VideoCodec;
    use jni::objects::{JString, JValue};

    let mut decoders = Vec::new();
    with_helper(|env, _, helper| {
        for (codec, mime) in [
            (VideoCodec::Hevc, "video/hevc"),
            (VideoCodec::H264, "video/avc"),
            (VideoCodec::Av1, "video/av01"),
        ] {
            let mime = env.new_string(mime)?;
            let name = env
                .call_static_method(
                    helper,
                    "findVideoDecoder",
                    "(Ljava/lang/String;III)Ljava/lang/String;",
                    &[
                        JValue::Object(&mime),
                        JValue::Int(width as i32),
                        JValue::Int(height as i32),
                        JValue::Int(fps as i32),
                    ],
                )?
                .l()?;
            if !name.is_null() {
                let name = env.get_string(&JString::from(name))?.into();
                decoders.push((codec, name));
            }
        }
        Ok(())
    })?;
    Ok(decoders)
}

/// Loads HestiaHelper from the dex built by build.rs (embedded here) and
/// registers its native methods.
#[cfg(target_os = "android")]
fn load_helper(
    env: &mut jni::JNIEnv<'_>,
    activity: &jni::objects::JObject<'_>,
) -> jni::errors::Result<jni::objects::GlobalRef> {
    use jni::objects::{JClass, JValue};

    static DEX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/classes.dex"));
    // SAFETY: DEX is static and never written; the class loader only reads it.
    let buffer = unsafe { env.new_direct_byte_buffer(DEX.as_ptr().cast_mut(), DEX.len())? };
    let parent = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let loader = env.new_object(
        "dalvik/system/InMemoryDexClassLoader",
        "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
        &[JValue::Object(&buffer), JValue::Object(&parent)],
    )?;
    let name = env.new_string("com.mroz59.hestia.HestiaHelper")?;
    let class = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let class = JClass::from(class);
    let methods = [
        jni::NativeMethod {
            name: "nativeCommitText".into(),
            sig: "(Ljava/lang/String;)V".into(),
            fn_ptr: native_commit_text as *mut std::ffi::c_void,
        },
        jni::NativeMethod {
            name: "nativeDeleteBefore".into(),
            sig: "(I)V".into(),
            fn_ptr: native_delete_before as *mut std::ffi::c_void,
        },
    ];
    env.register_native_methods(&class, &methods)?;
    log::info!("Android helper loaded");
    env.new_global_ref(class)
}

#[cfg(target_os = "android")]
fn ime_event(event: ImeEvent) {
    if let Some(sink) = IME_SINK.get() {
        sink(event);
    }
}

#[cfg(target_os = "android")]
extern "system" fn native_commit_text(
    mut env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    text: jni::objects::JString<'_>,
) {
    if let Ok(text) = env.get_string(&text) {
        ime_event(ImeEvent::Text(text.into()));
    }
}

#[cfg(target_os = "android")]
extern "system" fn native_delete_before(
    _env: jni::JNIEnv<'_>,
    _class: jni::objects::JClass<'_>,
    count: jni::sys::jint,
) {
    if count > 0 {
        ime_event(ImeEvent::DeleteBefore(count as usize));
    }
}

// ---------------------------------------------------------------------------
// Game controllers (Android: the patched Slint backend's input hook)
// ---------------------------------------------------------------------------

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod controllers {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    use hestia_core::stream::input::GamepadState;

    use crate::gamepad::Controllers;

    pub(super) static ACTIVE: AtomicBool = AtomicBool::new(false);
    pub(super) static STATE: Mutex<Controllers> = Mutex::new(Controllers::new());
    pub(super) static SINK: OnceLock<Box<dyn Fn(GamepadState) + Send + Sync>> = OnceLock::new();

    pub(super) fn state() -> std::sync::MutexGuard<'static, Controllers> {
        STATE.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(super) fn send(update: Option<GamepadState>) {
        if let (Some(state), Some(sink)) = (update, SINK.get()) {
            sink(state);
        }
    }

    pub(super) fn active() -> bool {
        ACTIVE.load(Ordering::Relaxed)
    }
}

/// Where controller state goes (the controller sends it to the stream).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn set_gamepad_sink(sink: impl Fn(hestia_core::stream::input::GamepadState) + Send + Sync + 'static) {
    let _ = controllers::SINK.set(Box::new(sink));
}

/// Controllers drive the host only while streaming; otherwise their d-pad
/// and B button navigate the app as usual. Ending clears them and stops
/// any rumble.
pub fn set_gamepads_streaming(on: bool) {
    controllers::ACTIVE.store(on, std::sync::atomic::Ordering::Relaxed);
    if !on {
        let devices: Vec<i32> = (0..crate::gamepad::MAX_CONTROLLERS as u8)
            .filter_map(|i| controllers::state().device_of(i))
            .collect();
        controllers::state().reset();
        for device in devices {
            rumble_device(device, 0, 0);
        }
        rumble_device(-1, 0, 0);
    }
}

/// A controller's whole state, by its own `device` key: the on-screen
/// one, or a desktop controller. It takes a slot like a physical one;
/// nothing is sent unless a stream runs.
pub fn gamepad_state(device: i32, buttons: u32, left_stick: (i16, i16), right_stick: (i16, i16), triggers: (u8, u8)) {
    if !controllers::active() {
        return;
    }
    let update = controllers::state().set(device, buttons, left_stick, right_stick, triggers);
    controllers::send(update);
}

/// Controller `device` went away: release everything, free its slot.
pub fn gamepad_gone(device: i32) {
    gamepad_state(device, 0, (0, 0), (0, 0), (0, 0));
    controllers::state().remove(device);
}

/// The on-screen controller's state.
pub fn onscreen_gamepad(buttons: u32, left_stick: (i16, i16), right_stick: (i16, i16), triggers: (u8, u8)) {
    gamepad_state(crate::gamepad::ON_SCREEN, buttons, left_stick, right_stick, triggers);
}

/// The on-screen controller went away.
pub fn onscreen_gamepad_off() {
    gamepad_gone(crate::gamepad::ON_SCREEN);
}

/// Rumble for controller `index` from the host.
pub fn rumble(index: u8, low: u16, high: u16) {
    let device = controllers::state().device_of(index).unwrap_or(-1);
    rumble_device(device, low, high);
}

fn rumble_device(device: i32, low: u16, high: u16) {
    #[cfg(target_os = "android")]
    {
        let result = with_helper(|env, activity, helper| {
            use jni::objects::JValue;
            env.call_static_method(
                helper,
                "rumble",
                "(Landroid/app/Activity;III)V",
                &[
                    JValue::Object(activity),
                    JValue::Int(device),
                    JValue::Int(i32::from(low)),
                    JValue::Int(i32::from(high)),
                ],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::debug!("rumble failed: {e}");
        }
    }
    #[cfg(target_os = "linux")]
    crate::desktop_pads::rumble(device, low, high);
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    let _ = (device, low, high);
}

/// Sees every input event before Slint (patched backend). Takes game
/// controller buttons and sticks while a stream runs.
#[cfg(target_os = "android")]
pub fn android_input_hook(event: &i_slint_backend_android_activity::android_activity::input::InputEvent<'_>) -> bool {
    use i_slint_backend_android_activity::android_activity::input::{Axis, InputEvent, KeyAction, MotionAction};

    use crate::gamepad::{self, Axes};

    if !controllers::active() {
        return false;
    }
    match event {
        InputEvent::KeyEvent(key) => {
            if !gamepad::is_controller_source(u32::from(key.source())) {
                return false;
            }
            let code = u32::from(key.key_code());
            if gamepad::control_for_keycode(code).is_none() {
                return false; // e.g. BACK, which opens the stream menu
            }
            let pressed = match key.action() {
                KeyAction::Down => true,
                KeyAction::Up => false,
                _ => return true,
            };
            if pressed && key.repeat_count() > 0 {
                return true;
            }
            let update = controllers::state().key(key.device_id(), code, pressed);
            controllers::send(update);
            true
        }
        InputEvent::MotionEvent(motion) => {
            let source = u32::from(motion.source());
            if source & gamepad::SOURCE_JOYSTICK != gamepad::SOURCE_JOYSTICK || motion.action() != MotionAction::Move {
                return false;
            }
            let p = motion.pointer_at_index(0);
            let axes = Axes {
                x: p.axis_value(Axis::X),
                y: p.axis_value(Axis::Y),
                z: p.axis_value(Axis::Z),
                rz: p.axis_value(Axis::Rz),
                rx: p.axis_value(Axis::Rx),
                ry: p.axis_value(Axis::Ry),
                ltrigger: p.axis_value(Axis::Ltrigger),
                rtrigger: p.axis_value(Axis::Rtrigger),
                brake: p.axis_value(Axis::Brake),
                gas: p.axis_value(Axis::Gas),
                hat_x: p.axis_value(Axis::HatX),
                hat_y: p.axis_value(Axis::HatY),
            };
            let update = controllers::state().motion(motion.device_id(), axes);
            controllers::send(update);
            true
        }
        _ => false,
    }
}
