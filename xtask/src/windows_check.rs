//! `check-windows`: fast local Windows type/clippy cross-check (`cargo xtask check-windows`).
//!
//! Catches `#[cfg(windows)]` compile errors and clippy `-D warnings` lints (such as missing or
//! unused imports across platform splits) locally on macOS or Linux in seconds, without waiting
//! for Windows CI or installing a full MSVC SDK.
//!
//! Because `cargo check` / `cargo clippy` only emits metadata and never invokes a native linker,
//! on non-Windows hosts this task points `ghostty-vt-rs-sys` at a temporary metadata-only stub
//! prefix inside an isolated `target/windows-check` directory (unless the caller already set
//! `GHOSTTY_VT_RS_SYS_PREBUILT_DIR` / `GHOSTTY_SOURCE_DIR` or passed `--no-stub`). That keeps the
//! main `target/` tree untouched for real cross-links while caching incremental check metadata
//! across runs.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// Default Windows target triple checked locally.
pub const DEFAULT_WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

const USAGE: &str = "usage: cargo xtask check-windows [--target TRIPLE] [--check-only] [--xwin] [--no-stub]";

/// Parsed CLI options for `cargo xtask check-windows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCheckOpts {
    /// Target triple to check (defaults to `x86_64-pc-windows-msvc`).
    pub target: String,
    /// Whether `--target` was explicitly provided on the CLI.
    pub explicit_target: bool,
    /// Run `cargo check` instead of `cargo clippy -- -D warnings`.
    pub check_only: bool,
    /// Invoke `cargo xwin` instead of plain `cargo`.
    pub use_xwin: bool,
    /// Disable the metadata-only `ghostty-vt-rs-sys` stub prefix.
    pub no_stub: bool,
}

impl Default for WindowsCheckOpts {
    fn default() -> Self {
        Self {
            target: DEFAULT_WINDOWS_TARGET.to_owned(),
            explicit_target: false,
            check_only: false,
            use_xwin: false,
            no_stub: false,
        }
    }
}

impl WindowsCheckOpts {
    /// Parses CLI arguments after `check-windows`.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut opts = Self::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--target" => {
                    let Some(value) = args.get(i + 1) else {
                        return Err(format!("--target requires a value\n{USAGE}"));
                    };
                    if !value.contains("windows") {
                        return Err(format!("--target must be a Windows triple (got `{value}`)\n{USAGE}"));
                    }
                    opts.target = value.clone();
                    opts.explicit_target = true;
                    i += 2;
                }
                "--check-only" => {
                    opts.check_only = true;
                    i += 1;
                }
                "--xwin" => {
                    opts.use_xwin = true;
                    i += 1;
                }
                "--no-stub" => {
                    opts.no_stub = true;
                    i += 1;
                }
                other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
            }
        }
        Ok(opts)
    }
}

/// Returns whether `target` appears in `rustup target list --installed` output.
pub fn target_is_installed(rustup_output: &str, target: &str) -> bool {
    rustup_output.lines().any(|line| line.trim() == target)
}

/// Builds the `cargo` argument vector for checking the workspace for Windows.
pub fn build_cargo_args(opts: &WindowsCheckOpts, native_windows: bool) -> Vec<String> {
    let mut args = Vec::new();
    if opts.use_xwin {
        args.push("xwin".to_owned());
    }
    args.push(if opts.check_only { "check".to_owned() } else { "clippy".to_owned() });
    args.extend(["--locked", "--workspace", "--all-targets"].into_iter().map(str::to_owned));
    if !native_windows || opts.explicit_target {
        args.push("--target".to_owned());
        args.push(opts.target.clone());
    }
    if !opts.check_only {
        args.extend(["--", "-D", "warnings"].into_iter().map(str::to_owned));
    }
    args
}

/// Resolves the isolated target directory used when running with the metadata-only VT stub,
/// preserving incremental check artifacts without ever writing stub outputs into the main
/// linking target directory.
pub fn resolve_check_target_dir(root: &Path, cargo_target_dir_env: Option<&OsStr>) -> PathBuf {
    let base = match cargo_target_dir_env {
        Some(dir) if !dir.is_empty() => {
            let path = PathBuf::from(dir);
            if path.is_absolute() { path } else { root.join(path) }
        }
        _ => root.join("target"),
    };
    base.join("windows-check")
}

/// Creates a scoped temporary metadata-only `ghostty-vt-rs-sys` prebuilt directory containing
/// the placeholder files checked by `ghostty-vt-rs-sys/build.rs`.
///
/// Only valid for non-linking commands (`cargo check` / `cargo clippy`).
pub fn create_vt_check_stub() -> Result<tempfile::TempDir, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("check-windows: creating temp stub dir: {e}"))?;
    let lib_dir = dir.path().join("lib");
    let include_dir = dir.path().join("include").join("ghostty");
    std::fs::create_dir_all(&lib_dir).map_err(|e| format!("check-windows: mkdir stub lib: {e}"))?;
    std::fs::create_dir_all(&include_dir).map_err(|e| format!("check-windows: mkdir stub include: {e}"))?;
    for rel in ["lib/libghostty-vt.a", "lib/ghostty-vt-static.lib", "include/ghostty/vt.h"] {
        std::fs::write(dir.path().join(rel), b"").map_err(|e| format!("check-windows: writing stub {rel}: {e}"))?;
    }
    Ok(dir)
}

fn verify_cross_toolchain(opts: &WindowsCheckOpts) -> Result<(), String> {
    if opts.use_xwin {
        let xwin_ok = Command::new("cargo").args(["xwin", "--version"]).output().is_ok_and(|out| out.status.success());
        if !xwin_ok {
            return Err("check-windows: `cargo-xwin` is not installed.\n\
                 Install it with `cargo install cargo-xwin`, or omit `--xwin` to run metadata-only \
                 cross-checking with rustup alone."
                .to_owned());
        }
    }

    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map_err(|e| format!("check-windows: failed to run `rustup target list --installed`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "check-windows: `rustup target list --installed` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !target_is_installed(&stdout, &opts.target) {
        return Err(format!(
            "check-windows: Windows target `{}` is not installed in rustup.\n\
             Install it once with:\n  rustup target add {}",
            opts.target, opts.target
        ));
    }
    Ok(())
}

/// Runs `cargo xtask check-windows`.
pub fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let opts = WindowsCheckOpts::parse(args)?;
    let native_windows = cfg!(windows) && !opts.use_xwin;

    if !native_windows || opts.explicit_target {
        verify_cross_toolchain(&opts)?;
    }

    let use_stub = !native_windows
        && !opts.no_stub
        && std::env::var_os("GHOSTTY_VT_RS_SYS_PREBUILT_DIR").is_none()
        && std::env::var_os("GHOSTTY_SOURCE_DIR").is_none();

    let _stub_guard = if use_stub { Some(create_vt_check_stub()?) } else { None };
    let cargo_args = build_cargo_args(&opts, native_windows);
    let started = Instant::now();

    println!(
        "check-windows: running `cargo {}`{}",
        cargo_args.join(" "),
        if use_stub { " (isolated metadata stub)" } else { "" }
    );

    let mut cmd = Command::new("cargo");
    cmd.args(&cargo_args).current_dir(root);
    if let Some(ref stub) = _stub_guard {
        let target_dir = resolve_check_target_dir(root, std::env::var_os("CARGO_TARGET_DIR").as_deref());
        cmd.env("CARGO_TARGET_DIR", target_dir)
            .env("GHOSTTY_VT_RS_SYS_PREBUILT_DIR", stub.path())
            .env_remove("GHOSTTY_VT_RS_SYS_BUILD")
            .env_remove("GHOSTTY_VT_RS_SYS_OPTIMIZE")
            .env_remove("GHOSTTY_VT_RS_SYS_CPU")
            .env_remove("GHOSTTY_ZIG_SYSTEM_DIR");
    }

    let status = cmd.status().map_err(|e| format!("check-windows: cargo failed to start: {e}"))?;
    if !status.success() {
        return Err(format!("check-windows: failed ({status})"));
    }
    println!("check-windows: passed in {:.1?}", started.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_flags_and_rejects_invalid_inputs() {
        let args = |xs: &[&str]| xs.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        assert_eq!(WindowsCheckOpts::parse(&[]).unwrap(), WindowsCheckOpts::default());
        assert_eq!(
            WindowsCheckOpts::parse(&args(&[
                "--target",
                "x86_64-pc-windows-gnu",
                "--check-only",
                "--xwin",
                "--no-stub",
            ]))
            .unwrap(),
            WindowsCheckOpts {
                target: "x86_64-pc-windows-gnu".to_owned(),
                explicit_target: true,
                check_only: true,
                use_xwin: true,
                no_stub: true,
            }
        );

        assert!(WindowsCheckOpts::parse(&args(&["--target"])).unwrap_err().contains("--target requires"));
        assert!(WindowsCheckOpts::parse(&args(&["--target", "x86_64-unknown-linux-gnu"])).is_err());
        assert!(WindowsCheckOpts::parse(&args(&["--bogus"])).unwrap_err().contains("unknown argument"));
    }

    #[test]
    fn detects_installed_rustup_targets_including_crlf() {
        let out = "aarch64-apple-darwin\r\nx86_64-pc-windows-msvc\r\n";
        assert!(target_is_installed(out, "x86_64-pc-windows-msvc"));
        assert!(!target_is_installed(out, "x86_64-pc-windows-gnu"));
    }

    #[test]
    fn builds_expected_cargo_args_across_modes() {
        let default_opts = WindowsCheckOpts::default();
        assert_eq!(
            build_cargo_args(&default_opts, false),
            vec![
                "clippy",
                "--locked",
                "--workspace",
                "--all-targets",
                "--target",
                DEFAULT_WINDOWS_TARGET,
                "--",
                "-D",
                "warnings",
            ]
        );
        assert_eq!(
            build_cargo_args(&default_opts, true),
            vec!["clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"]
        );

        let check_xwin = WindowsCheckOpts {
            target: "aarch64-pc-windows-msvc".to_owned(),
            explicit_target: true,
            check_only: true,
            use_xwin: true,
            no_stub: false,
        };
        assert_eq!(
            build_cargo_args(&check_xwin, true),
            vec!["xwin", "check", "--locked", "--workspace", "--all-targets", "--target", "aarch64-pc-windows-msvc"]
        );
    }

    #[test]
    fn resolves_isolated_target_dir_and_creates_vt_stub() {
        let root = Path::new("/repo");
        assert_eq!(resolve_check_target_dir(root, None), PathBuf::from("/repo/target/windows-check"));
        assert_eq!(
            resolve_check_target_dir(root, Some(OsStr::new("custom-target"))),
            PathBuf::from("/repo/custom-target/windows-check")
        );
        assert_eq!(
            resolve_check_target_dir(root, Some(OsStr::new("/abs/target"))),
            PathBuf::from("/abs/target/windows-check")
        );

        let stub = create_vt_check_stub().unwrap();
        assert!(stub.path().join("lib/libghostty-vt.a").is_file());
        assert!(stub.path().join("lib/ghostty-vt-static.lib").is_file());
        assert!(stub.path().join("include/ghostty/vt.h").is_file());
    }
}
