#[cfg(not(target_os = "android"))]
fn main() -> Result<(), slint::PlatformError> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let data_dir = std::env::var_os("HESTIA_DATA_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            directories::ProjectDirs::from("io.github", "MrOz59", "hestia-rs").map(|d| d.data_dir().to_path_buf())
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".hestia-rs"));
    hestia_app::run(data_dir)
}

// On Android the entry point is `android_main` in the library.
#[cfg(target_os = "android")]
fn main() {}
