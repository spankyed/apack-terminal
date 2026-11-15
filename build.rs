//! Puts the ConPTY the broker ships on Windows beside the binary `cargo` builds (`windows/conpty/README.md`).
//!
//! **Beside the binary, because that is the one place it is looked for.** `portable-pty` loads `conpty.dll` by
//! name, which Windows resolves from the executable's own directory first, and that DLL starts the
//! `OpenConsole.exe` beside it. A broker built without them runs on the system's ConPTY, which drops images and
//! mouse reporting before the broker sees them; the suite's Windows cases for both fail on that broker.
//!
//! The binary is written to the profile's directory, three above this script's `OUT_DIR`
//! (`target/<profile>/build/<package>-<hash>/out`). A file already there with the same bytes is left alone: a
//! running broker holds its DLL open, and Windows refuses to replace an open file, so copying regardless would
//! fail the build of a checkout whose app is running.

use std::path::{Path, PathBuf};

const FILES: [&str; 2] = ["conpty.dll", "OpenConsole.exe"];

fn main() {
    println!("cargo:rerun-if-changed=windows/conpty");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let architecture = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("aarch64") => "arm64",
        other => panic!("no ConPTY is carried for {other:?}; windows/conpty has x64 and arm64"),
    };
    let from = Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"))
        .join("windows/conpty")
        .join(architecture);
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets it"));
    let profile = out.ancestors().nth(3).expect("OUT_DIR is target/<profile>/build/<package>/out");
    for file in FILES {
        let source = from.join(file);
        let bytes = std::fs::read(&source).unwrap_or_else(|error| panic!("{}: {error}", source.display()));
        let into = profile.join(file);
        if std::fs::read(&into).is_ok_and(|there| there == bytes) {
            continue;
        }
        std::fs::write(&into, &bytes).unwrap_or_else(|error| panic!("{}: {error}", into.display()));
    }
}
