use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Embed the app icon (Explorer / taskbar / console window) as a Win32 resource.
    // GNU toolchain: compile icon.rc with windres into a COFF object and link it.
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let res = out_dir.join("icon.res");
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=tapkey.ico");
    let status = Command::new("windres")
        .current_dir(&manifest_dir)
        .arg("icon.rc")
        .arg("-O")
        .arg("coff")
        .arg("-o")
        .arg(&res)
        .status()
        .expect("windres not found — mingw bin must be on PATH");
    assert!(status.success(), "windres failed");
    println!("cargo:rustc-link-arg={}", res.display());
}
