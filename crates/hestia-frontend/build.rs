//! Android: compiles the Java helper the platform layer loads at run time.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        build_java_helper();
    }
}

/// Compiles java/HestiaHelper.java into OUT_DIR/classes.dex, which
/// `platform/android.rs` embeds and loads at runtime (the same approach Slint uses for its own
/// Android helper).
fn build_java_helper() {
    use android_build::{Dexer, JavaBuild};
    use std::path::PathBuf;

    const SOURCE: &str = "java/HestiaHelper.java";
    println!("cargo:rerun-if-changed={SOURCE}");
    let out_dir: PathBuf = std::env::var_os("OUT_DIR").expect("OUT_DIR").into();
    let classes = out_dir.join("hestia-java");
    let _ = std::fs::remove_dir_all(&classes);
    std::fs::create_dir_all(&classes).expect("creating the Java output directory");
    let android_jar = android_build::android_jar(None).expect("no Android platform found in the SDK");

    let javac = JavaBuild::new()
        .file(SOURCE)
        .class_path(&android_jar)
        .classes_out_dir(&classes)
        .java_source_version(8)
        .java_target_version(8)
        .command()
        .expect("building the javac command")
        .args(["-encoding", "UTF-8"])
        .output()
        .expect("running javac");
    assert!(
        javac.status.success(),
        "javac failed: {}",
        String::from_utf8_lossy(&javac.stderr)
    );

    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    let d8 = Dexer::new()
        .android_jar(&android_jar)
        .class_path(&classes)
        .collect_classes(&classes)
        .expect("collecting compiled classes")
        .release(release)
        .android_min_api(26)
        .out_dir(out_dir.clone())
        .command()
        .expect("building the d8 command")
        .output()
        .expect("running d8");
    assert!(
        d8.status.success(),
        "d8 failed: {}",
        String::from_utf8_lossy(&d8.stderr)
    );
}
