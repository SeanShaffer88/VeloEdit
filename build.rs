//! Build script: embed the program icon (assets/veloedit.ico) into the
//! Windows executable as a compiled resource.
//!
//! Standard library only — the crate allows no dependencies beyond
//! windows-sys, and a build script needs none. On non-Windows targets
//! this script does nothing at all, so host builds and tests (Linux CI
//! included) are completely unaffected.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/veloedit.rc");
    println!("cargo:rerun-if-changed=assets/veloedit.ico");

    let target = env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let assets_dir = manifest_dir.join("assets");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));

    if target.contains("msvc") {
        compile_with_rc(&assets_dir, &out_dir);
    } else if target.contains("gnu") {
        compile_with_windres(&assets_dir, &out_dir);
    }
}

/// MSVC route: compile veloedit.rc with the Windows SDK resource
/// compiler (rc.exe) and pass the resulting .res to the linker. A
/// Windows release build must never silently ship without the icon,
/// so every failure on a Windows host is a hard panic with a clear
/// message. On a non-Windows host (a cross `cargo check` from Linux)
/// rc.exe cannot run at all: warn and skip instead — the real MSVC
/// builds happen on Windows machines and in Windows CI, where the
/// panic applies.
fn compile_with_rc(assets_dir: &std::path::Path, out_dir: &std::path::Path) {
    if env::consts::OS != "windows" {
        println!(
            "cargo:warning=non-Windows host: skipping the MSVC icon resource \
             (it is compiled by rc.exe on Windows builds)"
        );
        return;
    }
    let rc = find_rc_exe().unwrap_or_else(|| {
        panic!(
            "rc.exe not found: set the RC environment variable to its path, \
             or install the Windows 10/11 SDK (which provides rc.exe). \
             The icon resource cannot be compiled without it."
        )
    });
    let res_path = out_dir.join("veloedit.res");
    let status = Command::new(&rc)
        .current_dir(assets_dir)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg("veloedit.rc")
        .status()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", rc.display()));
    if !status.success() {
        panic!(
            "rc.exe failed to compile assets/veloedit.rc (exit status {status}); \
             the icon resource was not built."
        );
    }
    println!("cargo:rustc-link-arg-bins={}", res_path.display());
}

/// Locate rc.exe: the RC environment variable wins, then the highest
/// versioned Windows Kits install, then plain `rc.exe` on PATH.
fn find_rc_exe() -> Option<PathBuf> {
    if let Ok(rc) = env::var("RC") {
        if !rc.is_empty() {
            return Some(PathBuf::from(rc));
        }
    }
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = std::fs::read_dir(&kits) {
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("x64").join("rc.exe"))
            .filter(|p| p.is_file())
            .collect();
        // Version folder names sort correctly as strings (10.0.x).
        versions.sort();
        if let Some(highest) = versions.pop() {
            return Some(highest);
        }
    }
    // Last resort: rc.exe somewhere on PATH.
    let probe = Command::new("rc.exe").arg("/?").output();
    if probe.is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    None
}

/// GNU route: compile veloedit.rc with MinGW windres into a COFF object
/// and pass it to the linker. windres is a local cross-build tool; if
/// it is missing we warn and build without the embedded icon instead of
/// failing a build that may not be a release build.
fn compile_with_windres(assets_dir: &std::path::Path, out_dir: &std::path::Path) {
    let obj_path = out_dir.join("veloedit.o");
    // Short -i/-o spellings: GNU windres rejects the single-dash long
    // forms (-input/-output) on some binutils builds.
    let status = Command::new("x86_64-w64-mingw32-windres")
        .current_dir(assets_dir)
        .arg("-O")
        .arg("coff")
        .arg("-i")
        .arg("veloedit.rc")
        .arg("-o")
        .arg(&obj_path)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg-bins={}", obj_path.display());
        }
        Ok(s) => {
            println!("cargo:warning=windres failed ({s}); building without the embedded icon");
        }
        Err(_) => {
            println!("cargo:warning=windres not found; building without the embedded icon");
        }
    }
}
