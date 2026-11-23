fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    // The headless UI tests inspect the actual controls through Slint's element tree.
    let config = if std::env::var("PROFILE").as_deref() == Ok("debug") {
        config.with_debug_info(true)
    } else {
        config
    };
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the Slint UI");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        // Native C++ code (OpenH264) links libc++ statically on Android (see
        // .cargo/config.toml); libc++_static needs the C++ ABI library next to it.
        println!("cargo:rustc-link-lib=c++abi");
    }
}
