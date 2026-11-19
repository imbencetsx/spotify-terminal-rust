use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/now-playing.m");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        _ => panic!("unsupported macOS target architecture"),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"));
    let deployment_target = env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
    let status = Command::new("xcrun")
        .args([
            "--sdk",
            "macosx",
            "clang",
            "-arch",
            arch,
            "-fobjc-arc",
            "-O2",
        ])
        .arg(format!("-mmacosx-version-min={deployment_target}"))
        .args([
            "-framework",
            "AppKit",
            "-framework",
            "MediaPlayer",
            "-framework",
            "AVFoundation",
        ])
        .arg("native/now-playing.m")
        .arg("-o")
        .arg(out.join("str-now-playing"))
        .status()
        .expect("run clang from the macOS SDK");
    assert!(
        status.success(),
        "could not build the macOS Now Playing helper"
    );
}
