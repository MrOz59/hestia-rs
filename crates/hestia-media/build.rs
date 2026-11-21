//! Warns when OpenH264 is about to be built without its x86 SIMD code.
//!
//! The `openh264` crate assembles OpenH264's SSE2/SSSE3 routines (motion
//! compensation, IDCT, deblocking) with `nasm` and silently builds the plain
//! C versions when `nasm` is missing; decoding is then about a quarter
//! slower. ARM builds compile their NEON code with the C compiler and need
//! nothing extra. 32-bit x86 is left alone: its OpenH264 assembly is not
//! position independent as the `openh264` crate builds it, which Android
//! refuses to load. At run time the decoder reports what it actually got
//! (see `software_h264`), so this is only an early hint.

use std::env;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=OPENH264_NO_ASM");
    println!("cargo::rerun-if-env-changed=NASM");
    if env::var_os("CARGO_FEATURE_OPENH264").is_none() {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64") {
        return;
    }
    if env::var_os("OPENH264_NO_ASM").is_some() {
        println!("cargo::warning=OPENH264_NO_ASM is set: OpenH264 decodes without SIMD (about 25% slower)");
        return;
    }
    let nasm = env::var_os("NASM").unwrap_or_else(|| "nasm".into());
    let found = Command::new(&nasm).arg("-v").output().is_ok_and(|o| o.status.success());
    if !found {
        println!(
            "cargo::warning=nasm not found: OpenH264 will decode without its x86 SIMD code (about 25% slower). \
             Install nasm, then rebuild it with `cargo clean -p openh264-sys2`."
        );
    }
}
