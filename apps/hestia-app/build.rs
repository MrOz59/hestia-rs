fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("failed to compile the Slint UI");

    // Native C++ code (OpenH264) links libc++ statically on Android (see
    // .cargo/config.toml); libc++_static needs the C++ ABI library next to it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        println!("cargo:rustc-link-lib=c++abi");
    }
}
