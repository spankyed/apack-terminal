//! The protocol's TypeScript, derived from the Rust types rather than compared against a file an earlier
//! command left.
//!
//! ts-rs generates one `export_bindings_*` test per exported type, and each writes into
//! `TS_RS_EXPORT_DIR`. Every type in `src/protocol.rs` names the same `export_to`, so the whole protocol
//! arrives as one `protocol.ts`. Without the variable ts-rs writes to a gitignored `bindings/`, which is a
//! second answer to the same question that nobody reads.

use std::path::Path;
use std::process::Command;

/// The one file the protocol's types generate, and where it is committed
pub const GENERATED: &str = "protocol.ts";
/// Where it is committed, relative to the workspace root
pub const COMMITTED_DIR: &str = "protocol";

/// Asks ts-rs to generate the declarations again, into `into`
pub fn write(root: &Path, into: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["test", "export_bindings"])
        .env("TS_RS_EXPORT_DIR", into)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo test export_bindings didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo test export_bindings failed:\n{}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let written = into.join(GENERATED);
    if !written.exists() {
        // A generator that wrote nothing would make `check-generated` pass by looking at nothing: every
        // exported type names `export_to = "protocol.ts"`, so an empty run means the attribute moved
        return Err(format!(
            "cargo test export_bindings wrote no {GENERATED} into {}: does every type in src/protocol.rs \
             still carry #[ts(export, export_to = \"{GENERATED}\")]?",
            into.display()
        ));
    }
    Ok(())
}

/// Reads what is committed, which is absent before the first `generate`
fn committed(root: &Path) -> Result<String, String> {
    let path = root.join(COMMITTED_DIR).join(GENERATED);
    std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes the derivation over what is committed
pub fn update(root: &Path) -> Result<(), String> {
    let fresh = tempfile::tempdir().map_err(|e| e.to_string())?;
    write(root, fresh.path())?;
    let into = root.join(COMMITTED_DIR);
    std::fs::create_dir_all(&into).map_err(|e| format!("{}: {e}", into.display()))?;
    std::fs::copy(fresh.path().join(GENERATED), into.join(GENERATED))
        .map_err(|e| format!("{}: {e}", into.join(GENERATED).display()))?;
    Ok(())
}

/// Fails when what `update` would write differs from what is committed
pub fn check(root: &Path) -> Result<(), String> {
    let fresh = tempfile::tempdir().map_err(|e| e.to_string())?;
    write(root, fresh.path())?;
    let derived = std::fs::read_to_string(fresh.path().join(GENERATED)).map_err(|e| e.to_string())?;
    if derived == committed(root)? {
        return Ok(());
    }
    Err(format!(
        "{COMMITTED_DIR}/{GENERATED} is not what src/protocol.rs generates; run `cargo xtask generate` and \
         commit the result. The apack monorepo keeps its own copy of this file and re-derives it the same way \
         (`npm run protocol:check` there), so a stale copy here is a stale copy in both"
    ))
}
