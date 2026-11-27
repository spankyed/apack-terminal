//! Reproducible release benchmark runner (`cargo xtask bench`).
//!
//! Builds the release `apack-terminal` broker binary first so the PTY flood benchmark measures an
//! isolated broker subprocess RSS without test-harness overhead, then executes `benches/v2_bench.rs`
//! under `--release`.

use std::path::Path;
use std::process::Command;

/// Builds the release broker binary and runs the `v2_bench` harness, forwarding `args`.
pub fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let build = Command::new(env!("CARGO"))
        .args(["build", "--locked", "--release", "--bin", "apack-terminal"])
        .current_dir(root)
        .status()
        .map_err(|e| format!("failed to run cargo build --release --bin apack-terminal: {e}"))?;
    if !build.success() {
        return Err(format!("release build of apack-terminal failed with status {build}"));
    }

    let bin_name = format!("apack-terminal{}", std::env::consts::EXE_SUFFIX);
    let bin_path = root.join("target").join("release").join(bin_name);
    if !bin_path.is_file() {
        return Err(format!("expected release binary at {}", bin_path.display()));
    }

    let mut cmd = Command::new(env!("CARGO"));
    cmd.args(["bench", "--locked", "--bench", "v2_bench", "--"])
        .args(args)
        .env("APACK_TERMINAL_BIN", &bin_path)
        .current_dir(root);

    let status = cmd.status().map_err(|e| format!("failed to run cargo bench --bench v2_bench: {e}"))?;
    if !status.success() {
        return Err(format!("v2_bench exited with status {status}"));
    }
    Ok(())
}
