//! Platform file pickers return a bounded copy of the chosen ZIP.

pub type Result = std::result::Result<Option<Vec<u8>>, String>;
pub type Completion = Box<dyn FnOnce(Result) + Send>;

#[cfg(target_os = "linux")]
pub struct Picker;

#[cfg(target_os = "linux")]
impl Picker {
    pub fn new() -> Self {
        Self
    }

    pub fn open(&self, done: Completion) {
        use std::io::Read;

        std::thread::spawn(move || {
            let result = (|| {
                let path = rfd::FileDialog::new()
                    .set_title("Load Theme Pack from File")
                    .add_filter("Hestia RS theme pack", &["zip"])
                    .pick_file();
                let Some(path) = path else { return Ok(None) };
                let file = std::fs::File::open(path).map_err(|err| format!("Could not open the theme pack. {err}"))?;
                let mut bytes = Vec::new();
                let limit = hestia_core::appearance::MAX_PACK_SIZE;
                file.take((limit + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|err| format!("Could not read the theme pack. {err}"))?;
                if bytes.len() > limit {
                    return Err("Theme pack is larger than 256 KB.".into());
                }
                Ok(Some(bytes))
            })();
            done(result);
        });
    }
}

#[cfg(not(any(target_os = "android", target_os = "linux")))]
pub struct Picker;

#[cfg(not(any(target_os = "android", target_os = "linux")))]
impl Picker {
    pub fn new() -> Self {
        Self
    }
    pub fn open(&self, done: Completion) {
        done(Err("Theme pack selection is available on Android and Linux.".into()));
    }
}

#[cfg(target_os = "android")]
pub use android::Picker;

#[cfg(target_os = "android")]
mod android {
    use std::sync::Mutex;

    use jni::objects::{GlobalRef, JByteArray, JObject, JString, JValue};
    use jni::{JNIEnv, JavaVM, NativeMethod};
    use slint::android::AndroidApp;

    use super::Completion;

    static PENDING: Mutex<Option<(GlobalRef, Completion)>> = Mutex::new(None);

    pub struct Picker(Option<(JavaVM, GlobalRef)>);

    impl Picker {
        pub fn new(app: &AndroidApp) -> Self {
            match Self::register(app) {
                Ok(bridge) => Self(bridge),
                Err(error) => {
                    log::warn!("could not register Android theme picker: {error}");
                    Self(None)
                }
            }
        }

        fn register(app: &AndroidApp) -> jni::errors::Result<Option<(JavaVM, GlobalRef)>> {
            // AndroidApp owns the Activity and provides the process VM.
            let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) }?;
            let activity = {
                let mut env = vm.attach_current_thread()?;
                let activity = unsafe { JObject::from_raw(app.activity_as_ptr().cast()) };
                let class = env.get_object_class(&activity)?;
                let stock = env.find_class("android/app/NativeActivity")?;
                if env.is_same_object(&class, stock)? {
                    return Ok(None);
                }
                env.register_native_methods(
                    &class,
                    &[NativeMethod {
                        name: "onNativeThemePack".into(),
                        sig: "([BLjava/lang/String;)V".into(),
                        fn_ptr: receive as *mut std::ffi::c_void,
                    }],
                )?;
                env.new_global_ref(activity)?
            };
            Ok(Some((vm, activity)))
        }

        pub fn open(&self, done: Completion) {
            let Some((vm, activity)) = &self.0 else {
                done(Err("This Android build does not include the theme file picker.".into()));
                return;
            };
            {
                let mut pending = PENDING.lock().unwrap();
                if pending.is_some() {
                    drop(pending);
                    done(Err("The file picker is already open.".into()));
                    return;
                }
                *pending = Some((activity.clone(), done));
            }
            let result = vm.attach_current_thread().and_then(|mut env| {
                let result = env.call_method(
                    activity.as_obj(),
                    "openThemePack",
                    "(I)V",
                    &[JValue::Int(hestia_core::appearance::MAX_PACK_SIZE as i32)],
                );
                if result.is_err() && env.exception_check().unwrap_or(false) {
                    let _ = env.exception_clear();
                }
                result.map(|_| ())
            });
            if let Err(error) = result {
                log::warn!("could not open Android theme picker: {error}");
                let pending = PENDING.lock().unwrap().take();
                if let Some((_, done)) = pending {
                    done(Err("Could not open the Android file picker.".into()));
                }
            }
        }
    }

    impl Drop for Picker {
        fn drop(&mut self) {
            if self.0.is_some() {
                let _ = PENDING.lock().unwrap().take();
            }
        }
    }

    extern "system" fn receive(mut env: JNIEnv<'_>, activity: JObject<'_>, bytes: JByteArray<'_>, error: JString<'_>) {
        let done = {
            let mut pending = PENDING.lock().unwrap();
            let Some((expected, _)) = pending.as_ref() else { return };
            if !env.is_same_object(expected.as_obj(), &activity).unwrap_or(false) {
                return;
            }
            pending.take().map(|(_, done)| done)
        };
        let Some(done) = done else { return };
        let result = if !error.is_null() {
            let message = env
                .get_string(&error)
                .map(String::from)
                .unwrap_or_else(|_| "Could not read the theme pack.".into());
            Err(message)
        } else if bytes.is_null() {
            Ok(None)
        } else {
            env.convert_byte_array(&bytes)
                .map(Some)
                .map_err(|_| "Could not read the theme pack.".into())
        };
        done(result);
    }
}
