//! Puts the ConPTY the broker ships on Windows beside both the built binaries (`target/<profile>/`)
//! and test executables (`target/<profile>/deps/`) (`windows/conpty/README.md`).
//!
//! **Beside every executable that links `apack-os`, because that is the one place it is looked for.**
//! `portable-pty` loads `conpty.dll` by name, which Windows resolves from the executable's own
//! directory first, and that DLL starts the `OpenConsole.exe` beside it. Placing this build script in
//! `crates/apack-os` ensures standalone `apack-broker` / `apack-os` test binaries and Windows
//! `cargo mutants` runs load the bundled ConPTY rather than falling back to Windows' inbox ConPTY
//! (which strips Kitty graphics APCs, mouse mode requests, and OSC 52 clipboard queries).
//!
//! A file already present with identical bytes is left untouched: a running broker holds its DLL
//! open, and Windows refuses to replace an open file.

use std::path::{Path, PathBuf};

const FILES: [&str; 2] = ["conpty.dll", "OpenConsole.exe"];

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let workspace_root =
        manifest_dir.parent().and_then(Path::parent).expect("crates/apack-os sits two levels under workspace root");
    let conpty_root = workspace_root.join("windows/conpty");
    println!("cargo:rerun-if-changed={}", conpty_root.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let architecture = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        other => panic!("no ConPTY is carried for {other:?}; windows/conpty has x64 and arm64"),
    };
    let from = conpty_root.join(architecture);
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it"));
    let profile = out.ancestors().nth(3).expect("OUT_DIR is target/<profile>/build/<package>/out");
    let targets = [profile.to_path_buf(), profile.join("deps")];
    for dir in &targets {
        std::fs::create_dir_all(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
    }
    for file in FILES {
        let source = from.join(file);
        let bytes = std::fs::read(&source).unwrap_or_else(|error| panic!("{}: {error}", source.display()));
        for dir in &targets {
            let into = dir.join(file);
            if std::fs::read(&into).is_ok_and(|there| there == bytes) {
                continue;
            }
            std::fs::write(&into, &bytes).unwrap_or_else(|error| panic!("{}: {error}", into.display()));
        }
    }
}
