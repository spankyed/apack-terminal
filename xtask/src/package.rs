//! `package`: the npm package in `node/`, with the broker binary in it.
//!
//! **The broker is a binary rather than a library, and this is how it reaches a packaged app.** The apack
//! monorepo resolves it as an ordinary dependency — `file:../../vendor/apack-terminal/node` through the
//! `vendor/apack-terminal` link locally, and a clone at the pinned commit in CI — so electron-builder
//! collects it with every other dependency instead of needing a rule of its own. It is the arrangement
//! elmdb's binding has there, which is the one a second native dependency should not invent differently.
//!
//! A release build, because this is the binary that ships. A development checkout does not use it: the
//! consumer prefers the debug build in `target/`, which is what a contributor editing the broker has.

use std::path::Path;
use std::process::Command;

/// What `cargo` names the broker on this platform, and what `node/package.json` ships under `files`
#[cfg(windows)]
const BINARY: &str = "apack-terminal.exe";
#[cfg(not(windows))]
const BINARY: &str = "apack-terminal";

/// Builds the broker for release and stages it in `node/`
pub fn package(root: &Path) -> Result<(), String> {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "--bin", "apack-terminal"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("cargo build --release didn't run: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build --release failed with {status}"));
    }
    let built = root.join("target/release").join(BINARY);
    if !built.exists() {
        // A build that reported success and wrote nothing under this name would stage the binary from the
        // build before, which is the failure this names rather than hides
        return Err(format!("cargo build --release wrote no {}", built.display()));
    }
    let into = root.join("node").join(BINARY);
    std::fs::copy(&built, &into).map_err(|e| format!("{} -> {}: {e}", built.display(), into.display()))?;
    println!("staged {} ({} bytes)", into.display(), std::fs::metadata(&into).map_err(|e| e.to_string())?.len());
    Ok(())
}
