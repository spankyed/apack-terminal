//! Deterministic Rust -> TypeScript wire schema codegen (`view/src/wire/{constants,types,messages}.ts`)
//! and golden v2 wire & mutual-HMAC test vectors (`node/wire-vectors-v2.json`), derived directly from
//! `apack-proto` and `apack-core` and verified byte-for-byte on both Rust and TypeScript sides.

use std::path::Path;
use std::process::{Command, Output};

use crate::wire_codegen;

/// The golden v2 wire-vector file committed inside `node/` and consumed by `view/tests/wire-vectors.spec.ts`.
pub const GENERATED: &str = "wire-vectors-v2.json";
pub const COMMITTED_DIR: &str = "node";

fn format_output(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut combined = String::new();
    if !stdout.trim().is_empty() {
        combined.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(stderr.trim());
    }
    combined
}

/// Regenerates `node/wire-vectors-v2.json` and `view/src/wire/{constants,types,messages}.ts`
/// from `apack-proto` and `apack-core`.
pub fn update(root: &Path) -> Result<(), String> {
    // Render all TS files in memory first so any generator invariant error aborts before writing.
    let rendered = wire_codegen::render_all();
    let output = Command::new(env!("CARGO"))
        .args(["test", "--locked", "-p", "apack-broker", "--test", "wire_vectors_v2"])
        .env("UPDATE_VECTORS", "1")
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo test wire_vectors_v2 didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("regenerating {COMMITTED_DIR}/{GENERATED} failed:\n{}", format_output(&output)));
    }
    let path = root.join(COMMITTED_DIR).join(GENERATED);
    if !path.is_file() {
        return Err(format!("wire_vectors_v2 wrote no {}", path.display()));
    }
    wire_codegen::write_rendered(root, &rendered)
}

/// Verifies committed generated TypeScript wire modules and `node/wire-vectors-v2.json` match
/// `apack-proto` and `apack-core` byte-for-byte.
pub fn check(root: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(err) = wire_codegen::check_all(root) {
        errors.push(err);
    }
    match Command::new(env!("CARGO"))
        .args(["test", "--locked", "-p", "apack-broker", "--test", "wire_vectors_v2"])
        .current_dir(root)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => errors.push(format!(
            "{COMMITTED_DIR}/{GENERATED} drifted from apack-proto/apack-core; run `cargo xtask generate` and \
             commit the result:\n{}",
            format_output(&output)
        )),
        Err(e) => errors.push(format!("cargo test wire_vectors_v2 didn't run: {e}")),
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n\n")) }
}
