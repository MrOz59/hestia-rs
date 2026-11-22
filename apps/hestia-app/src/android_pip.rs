//! Bridge between the Android Activity's PiP callbacks and the Slint window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use hestia_frontend::Controller;
use jni::objects::{GlobalRef, JObject, JValue};
use jni::sys::jboolean;
use jni::{JNIEnv, JavaVM, NativeMethod};
use slint::ComponentHandle;
use slint::android::AndroidApp;

use crate::AppWindow;

struct Callbacks {
    activity: GlobalRef,
    window: slint::Weak<AppWindow>,
    controller: Weak<Controller>,
}

static CALLBACKS: Mutex<Option<Callbacks>> = Mutex::new(None);
static IN_PIP: AtomicBool = AtomicBool::new(false);

pub(crate) fn blocks_input() -> bool {
    IN_PIP.load(Ordering::Acquire)
}

/// The Java class lives in the packaged APK. cargo-apk's quick build uses
/// the stock NativeActivity, so it can still run without this integration.
pub(crate) fn install(app: &AndroidApp, ui: &AppWindow, controller: &Arc<Controller>) -> Option<Registration> {
    match register(app, ui, controller) {
        Ok(registration) => registration,
        Err(error) => {
            log::warn!("could not register Android picture-in-picture: {error}");
            None
        }
    }
}

pub(crate) struct Registration;

impl Drop for Registration {
    fn drop(&mut self) {
        *CALLBACKS.lock().unwrap() = None;
        IN_PIP.store(false, Ordering::Release);
    }
}

fn register(
    app: &AndroidApp,
    ui: &AppWindow,
    controller: &Arc<Controller>,
) -> jni::errors::Result<Option<Registration>> {
    // SAFETY: AndroidApp owns a reference to its Activity and provides the
    // process' JavaVM. The Android UI thread calls back into Rust separately.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }?;
    let activity = {
        let mut env = vm.attach_current_thread()?;
        let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
        let class = env.get_object_class(&activity)?;
        let stock_activity = env.find_class("android/app/NativeActivity")?;
        if env.is_same_object(&class, &stock_activity)? {
            return Ok(None);
        }
        env.register_native_methods(
            &class,
            &[NativeMethod {
                name: "onNativePictureInPictureChanged".into(),
                sig: "(ZZ)V".into(),
                fn_ptr: mode_changed as *mut std::ffi::c_void,
            }],
        )?;
        env.new_global_ref(&activity)?
    };
    *CALLBACKS.lock().unwrap() = Some(Callbacks {
        activity: activity.clone(),
        window: ui.as_weak(),
        controller: Arc::downgrade(controller),
    });
    ui.on_stream_pip_config(move |active, aspect| {
        let result = vm.attach_current_thread().and_then(|mut env| {
            let result = env
                .call_method(
                    activity.as_obj(),
                    "setStreamPictureInPicture",
                    "(ZF)V",
                    &[JValue::Bool(u8::from(active)), JValue::Float(aspect)],
                )
                .map(|_| ());
            if result.is_err() && env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            result
        });
        if let Err(error) = result {
            log::warn!("could not configure Android picture-in-picture: {error}");
        }
    });
    log::info!("Android picture-in-picture callbacks registered");
    Ok(Some(Registration))
}

extern "system" fn mode_changed(env: JNIEnv<'_>, activity: JObject<'_>, in_pip: jboolean, dismissed: jboolean) {
    let in_pip = in_pip != 0;
    let (window, controller, entered) = {
        let callbacks = CALLBACKS.lock().unwrap();
        let Some(callbacks) = callbacks.as_ref() else { return };
        if !env
            .is_same_object(&activity, callbacks.activity.as_obj())
            .unwrap_or(false)
        {
            return;
        }
        let was_in_pip = IN_PIP.swap(in_pip, Ordering::AcqRel);
        let entered = in_pip && !was_in_pip;
        (callbacks.window.clone(), callbacks.controller.clone(), entered)
    };
    if dismissed != 0 {
        log::info!("Android picture-in-picture dismissed");
        // onStop may be followed immediately by Activity destruction. Start
        // the normal disconnect before the native window's event loop exits.
        if let Some(controller) = controller.upgrade() {
            controller.stream_disconnect();
        }
    } else if entered {
        log::info!("Android picture-in-picture entered");
    } else if !in_pip {
        log::info!("Android picture-in-picture expanded");
    }
    let _ = window.upgrade_in_event_loop(move |ui| {
        ui.set_stream_picture_in_picture(in_pip);
        if entered {
            ui.set_stream_keyboard_open(false);
            if let Some(controller) = controller.upgrade() {
                controller.stream_input_focus(false);
                controller.release_android_stream_gamepads();
            }
        }
    });
}
