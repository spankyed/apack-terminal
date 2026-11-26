//! Golden v2 wire & mutual-HMAC test vectors (`node/wire-vectors-v2.json`), derived directly from
//! `apack-proto` and `apack-core` and verified byte-for-byte on both Rust and TypeScript sides.

use std::path::Path;
use std::process::Command;

/// The golden v2 wire-vector file committed inside `node/` and consumed by `view/tests/wire-vectors.spec.ts`.
pub const GENERATED: &str = "wire-vectors-v2.json";
pub const COMMITTED_DIR: &str = "node";

/// Regenerates `node/wire-vectors-v2.json` from `apack-proto` and `apack-core`.
pub fn update(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["test", "--locked", "-p", "apack-broker", "--test", "wire_vectors_v2"])
        .env("UPDATE_VECTORS", "1")
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo test wire_vectors_v2 didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "regenerating {COMMITTED_DIR}/{GENERATED} failed:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let path = root.join(COMMITTED_DIR).join(GENERATED);
    if !path.is_file() {
        return Err(format!("wire_vectors_v2 wrote no {}", path.display()));
    }
    Ok(())
}

/// Verifies committed `node/wire-vectors-v2.json` matches `apack-proto` and `apack-core` byte-for-byte.
pub fn check(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["test", "--locked", "-p", "apack-broker", "--test", "wire_vectors_v2"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo test wire_vectors_v2 didn't run: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{COMMITTED_DIR}/{GENERATED} drifted from apack-proto/apack-core; run `cargo xtask generate` and \
         commit the result:\n{}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}
