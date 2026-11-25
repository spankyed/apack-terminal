//! The release binaries' third-party notices: `THIRD-PARTY-NOTICES` and `COPYRIGHT-rust-library.html`, at the
//! repository's root, which `binaries.yml` publishes in every `bin-<commit>` release beside the binaries.
//!
//! **What the binaries hold that a license asks a redistribution to credit.** The crates the broker is built from,
//! which cargo-about lists from the lockfile (`about.toml`, `about.hbs`); the Rust standard library, whose notices
//! the toolchain ships; and code no crate's metadata names, whose license files are kept verbatim under `licenses/`
//! (its README says where each is from): libghostty-vt, the static library `ghostty-vt-rs-sys` links, which is
//! Ghostty's code compiled with Zig together with the libraries Ghostty builds it with, and the original of a crate
//! that is a port. The Windows release ships Microsoft's ConPTY beside the binary, under `windows/conpty/`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The cargo-about release the notices are written with: another may render the same licenses differently, so
/// `check-notices` would fail on a file nothing changed
const CARGO_ABOUT: &str = "0.9.3";

/// The crates' licenses and notices, cargo-about's rendering of `about.hbs` followed by `BUNDLED`
const CRATE_NOTICES: &str = "THIRD-PARTY-NOTICES";

/// The Rust standard library's notices, the pinned toolchain's own `share/doc/rust/COPYRIGHT-library.html`
const RUST_LIBRARY_NOTICES: &str = "COPYRIGHT-rust-library.html";

/// The Ghostty commit libghostty-vt's licenses under `licenses/libghostty-vt/` were read at: the libraries and
/// versions its `deps/libghostty-vt-third-party.json` names as linked. `ghostty-vt-rs-sys` builds and links the
/// libghostty-vt of the commit its `build.rs` pins, `GHOSTTY_COMMIT`, whether from its prebuilt archive or from
/// source, as the Windows release does, and another fails the notices until those licenses are read again.
const GHOSTTY_COMMIT: &str = "0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec";

/// What follows the crates in `THIRD-PARTY-NOTICES`: a heading naming the code, and the license file that is its
/// notice, verbatim. Ghostty's own license is the crates' already, under `ghostty-vt-rs-sys`, which ships it.
const BUNDLED: &[(&str, &str)] = &[
    (
        "libghostty-vt: Zig's standard library and compiler_rt, as Zig 0.17.0 compiled them in (MIT)",
        "licenses/libghostty-vt/zig-LICENSE",
    ),
    ("libghostty-vt: translate-c 2.0.0 (MIT)", "licenses/libghostty-vt/translate-c-LICENSE"),
    (
        "libghostty-vt: Highway 1.2.0, taken under BSD-3-Clause of its Apache-2.0 OR BSD-3-Clause",
        "licenses/libghostty-vt/highway-LICENSE-BSD3",
    ),
    (
        "libghostty-vt: simdutf 9.0.0, taken under MIT of its Apache-2.0 OR MIT",
        "licenses/libghostty-vt/simdutf-LICENSE-MIT",
    ),
    (
        "libghostty-vt: Wuffs 0.4.0-alpha.10, taken under MIT of its Apache-2.0 OR MIT",
        "licenses/libghostty-vt/wuffs-LICENSE-MIT",
    ),
    ("libghostty-vt: uucode 0.2.0 (MIT AND Unicode-3.0)", "licenses/libghostty-vt/uucode-LICENSE"),
    (
        "libghostty-vt: uucode's UTF-8 decoder, from Bjoern Hoehrmann's",
        "licenses/libghostty-vt/uucode-LICENSE_Bjoern_Hoehrmann",
    ),
    ("libghostty-vt: uucode's Unicode data (Unicode-3.0)", "licenses/libghostty-vt/uucode-LICENSE_unicode"),
    (
        "libghostty-vt: musl's notices, for the math Zig's compiler_rt and standard library port from musl",
        "licenses/libghostty-vt/musl-COPYRIGHT",
    ),
    ("libghostty-vt: Go's license, for Zig's std/math/pow.zig, ported from Go", "licenses/libghostty-vt/go-LICENSE"),
    ("libghostty-vt: X11's rgb.txt, which Ghostty embeds (MIT/X11)", "licenses/libghostty-vt/x11-rgb-COPYING"),
    ("libghostty-vt: Foot's Kitty keymap, which Ghostty ports (MIT)", "licenses/libghostty-vt/foot-LICENSE"),
    ("The zmij crate: Victor Zverovich's zmij, of which it is a port (MIT)", "licenses/zmij-LICENSE"),
    ("The Windows release: Microsoft's conpty.dll and OpenConsole.exe (MIT)", "windows/conpty/LICENSE"),
    (
        "The Windows release: the third-party code built into conhost and OpenConsole, microsoft/terminal's NOTICE.md",
        "windows/conpty/NOTICE",
    ),
];

/// Each platform `binaries.yml` builds, by the name its matrix gives it, and the target it is built for
const PLATFORMS: &[(&str, &str)] = &[
    ("darwin-arm64", "aarch64-apple-darwin"),
    ("darwin-x64", "x86_64-apple-darwin"),
    ("linux-x64", "x86_64-unknown-linux-gnu"),
    ("linux-arm64", "aarch64-unknown-linux-gnu"),
    ("win32-x64", "x86_64-pc-windows-msvc"),
    ("win32-arm64", "aarch64-pc-windows-msvc"),
];

/// Everything `notices` writes, by file name. All of it is read before anything is written, so a run that fails
/// leaves the committed files as they were, and line endings are `\n` whatever the platform's, as `.gitattributes`
/// keeps them in a checkout, since `check-notices` compares bytes.
fn notices(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    linked_ghostty_is_read(root)?;
    let mut text = crate_notices(root)?;
    for (heading, path) in BUNDLED {
        let license = std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"))?;
        text.push_str(&format!(
            "\n================================================================================\n{heading}\n\n{}\n",
            license.trim_end()
        ));
    }
    let sysroot = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("rustc didn't run: {e}"))?;
    if !sysroot.status.success() {
        return Err(format!("rustc --print sysroot failed:\n{}", String::from_utf8_lossy(&sysroot.stderr)));
    }
    let library =
        PathBuf::from(String::from_utf8_lossy(&sysroot.stdout).trim()).join("share/doc/rust/COPYRIGHT-library.html");
    let library = std::fs::read(&library).map_err(|e| format!("{}: {e}", library.display()))?;
    Ok(vec![
        (CRATE_NOTICES.to_owned(), text.replace("\r\n", "\n").into_bytes()),
        (RUST_LIBRARY_NOTICES.to_owned(), String::from_utf8_lossy(&library).replace("\r\n", "\n").into_bytes()),
    ])
}

/// Fails unless `ghostty-vt-rs-sys`, as the lockfile resolves it, links libghostty-vt built from `GHOSTTY_COMMIT`
fn linked_ghostty_is_read(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--frozen"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo metadata failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata's output: {e}"))?;
    let manifest = metadata["packages"]
        .as_array()
        .and_then(|packages| packages.iter().find(|p| p["name"] == "ghostty-vt-rs-sys"))
        .and_then(|p| p["manifest_path"].as_str())
        .ok_or("ghostty-vt-rs-sys isn't in the dependency tree")?;
    let build = Path::new(manifest).with_file_name("build.rs");
    let source = std::fs::read_to_string(&build).map_err(|e| format!("{}: {e}", build.display()))?;
    let pinned = source
        .lines()
        .find_map(|line| line.trim().strip_prefix("const GHOSTTY_COMMIT: &str = \""))
        .and_then(|rest| rest.split('"').next())
        .ok_or_else(|| format!("{} pins no GHOSTTY_COMMIT", build.display()))?;
    if pinned == GHOSTTY_COMMIT {
        Ok(())
    } else {
        Err(format!(
            "ghostty-vt-rs-sys links libghostty-vt built from Ghostty {pinned}, and the licenses under \
             licenses/libghostty-vt/ were read at {GHOSTTY_COMMIT}: read that commit's \
             deps/libghostty-vt-third-party.json, update the files, licenses/README.md and BUNDLED, then \
             GHOSTTY_COMMIT"
        ))
    }
}

/// The targets of the platforms `binaries.yml` builds, read from its matrix's `platform:` entries
fn release_targets(root: &Path) -> Result<Vec<&'static str>, String> {
    let workflow = root.join(".github/workflows/binaries.yml");
    let text = std::fs::read_to_string(&workflow).map_err(|e| format!("{}: {e}", workflow.display()))?;
    let mut targets = Vec::new();
    for platform in text
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split("platform: ").nth(1))
    {
        let name = platform.split(|c: char| c == ',' || c.is_whitespace()).next().unwrap_or_default();
        let target =
            PLATFORMS.iter().find(|(known, _)| *known == name).map(|(_, target)| *target).ok_or_else(|| {
                format!("binaries.yml builds {name}, which PLATFORMS in xtask/src/notices.rs doesn't name")
            })?;
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    if targets.is_empty() {
        return Err(format!("{} names no platform", workflow.display()));
    }
    Ok(targets)
}

/// cargo-about's rendering of `about.hbs`: it reads the broker's dependency tree from the lockfile, without the
/// network, for each release target. It fails on a dependency whose license it can't establish or that
/// `about.toml` doesn't accept, and so does this on any warning: cargo-about only warns of a clarification whose
/// file no longer matches its checksum, and drops it.
fn crate_notices(root: &Path) -> Result<String, String> {
    let version = Command::new(env!("CARGO"))
        .args(["about", "--version"])
        .output()
        .map_err(|e| format!("cargo about didn't run: {e}"))?;
    let version = String::from_utf8_lossy(&version.stdout);
    if version.split_whitespace().last() != Some(CARGO_ABOUT) {
        return Err(format!(
            "The notices are written with cargo-about {CARGO_ABOUT}, and this machine has {}; install it with \
             `cargo install --locked --features cli cargo-about@{CARGO_ABOUT}`",
            if version.trim().is_empty() { "none" } else { version.trim() }
        ));
    }
    let rendered = tempfile::tempdir().map_err(|e| e.to_string())?;
    let out = rendered.path().join(CRATE_NOTICES);
    let mut command = Command::new(env!("CARGO"));
    command.args(["about", "generate", "--frozen", "--fail", "--manifest-path"]);
    command.arg(root.join("Cargo.toml")).arg("--config").arg(root.join("about.toml"));
    for target in release_targets(root)? {
        command.args(["--target", target]);
    }
    let output = command
        .arg("--output-file")
        .arg(&out)
        .arg(root.join("about.hbs"))
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo about didn't run: {e}"))?;
    let log = String::from_utf8_lossy(&output.stderr);
    let lowered = log.to_lowercase();
    if !output.status.success() || lowered.contains("warn") || lowered.contains("error") {
        return Err(format!(
            "cargo about failed or warned; it reads the crates offline, which `cargo fetch --locked` fetches:\n{log}"
        ));
    }
    std::fs::read_to_string(&out).map_err(|e| format!("{}: {e}", out.display()))
}

/// `cargo xtask notices`
pub fn write(root: &Path) -> Result<(), String> {
    for (name, contents) in notices(root)? {
        std::fs::write(root.join(&name), contents).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

/// `cargo xtask check-notices`: fails when a file `notices` writes differs from the committed one
pub fn check(root: &Path) -> Result<(), String> {
    let stale: Vec<String> = notices(root)?
        .into_iter()
        .filter(|(name, contents)| std::fs::read(root.join(name)).ok().as_deref() != Some(contents.as_slice()))
        .map(|(name, _)| name)
        .collect();
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "The release's notices are stale; run `cargo xtask notices` and commit what it writes:\n  {}",
            stale.join("\n  ")
        ))
    }
}
