use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/web-process.c");
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG");

    let package = "webkitgtk-web-process-extension-6.0";
    let flags = Command::new(env::var_os("PKG_CONFIG").unwrap_or_else(|| "pkg-config".into()))
        .args(["--cflags", "--libs", package])
        .output()
        .expect("run pkg-config for the WebKitGTK web process extension");
    assert!(
        flags.status.success(),
        "WebKitGTK web process extension headers are required: {}",
        String::from_utf8_lossy(&flags.stderr)
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"))
        .join("tillwind-web-process.so");
    let status = Command::new(env::var_os("CC").unwrap_or_else(|| "cc".into()))
        .args([
            "-shared",
            "-fPIC",
            "-Wall",
            "-Wextra",
            "src/web-process.c",
            "-o",
        ])
        .arg(output)
        .args(String::from_utf8_lossy(&flags.stdout).split_whitespace())
        .status()
        .expect("run C compiler for the WebKitGTK web process extension");
    assert!(
        status.success(),
        "build the WebKitGTK web process extension"
    );
}
