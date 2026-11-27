//! `cargo xtask fuzz-smoke`: replays every committed seed corpus and runs a bounded libFuzzer
//! smoke pass across every `[[bin]]` target declared in `fuzz/Cargo.toml`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Maximum allowed size of an individual committed seed file (64 KiB).
pub const MAX_SEED_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    seconds: u64,
    runs: Option<u64>,
    rss_limit_mb: u64,
    max_len: u64,
    target_triple: Option<String>,
    filter: Option<String>,
    replay_only: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            seconds: 10,
            runs: None,
            rss_limit_mb: 2048,
            max_len: 65_536,
            target_triple: None,
            filter: None,
            replay_only: false,
        }
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs deterministic corpus replay (`tests/fuzz_corpus.rs`) and, unless `--replay-only` is set,
/// builds all fuzz targets under nightly `cargo-fuzz` and executes both `-runs=0` ASAN seed replay
/// and a bounded `-max_total_time` libFuzzer smoke run for each target.
pub fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let config = parse_args(args)?;
    let fuzz_dir = root.join("fuzz");
    let manifest = fs::read_to_string(fuzz_dir.join("Cargo.toml"))
        .map_err(|err| format!("failed to read fuzz/Cargo.toml: {err}"))?;
    let all_targets = discover_targets(&manifest)?;
    if all_targets.is_empty() {
        return Err("no [[bin]] fuzz targets found in fuzz/Cargo.toml".to_owned());
    }

    let targets: Vec<String> = match &config.filter {
        Some(needle) => all_targets.into_iter().filter(|name| name.contains(needle.as_str())).collect(),
        None => all_targets,
    };
    if targets.is_empty() {
        return Err(format!("no fuzz targets matched --filter {:?}", config.filter.as_deref().unwrap_or("")));
    }

    // Validate seed directories before spending time compiling.
    for target in &targets {
        validate_seed_dir(&fuzz_dir.join("seeds").join(target), target)?;
    }

    // 1. Cross-platform deterministic seed replay on stable Rust (`tests/fuzz_corpus.rs`).
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .current_dir(root)
        .args(["test", "--locked", "--test", "fuzz_corpus"])
        .status()
        .map_err(|err| format!("failed to run cargo test --test fuzz_corpus: {err}"))?;
    if !status.success() {
        return Err(format!("deterministic fuzz corpus replay failed (exit {status})"));
    }

    if config.replay_only {
        return Ok(());
    }

    // 2. Build the fuzz targets with nightly cargo-fuzz.
    let mut build_cmd = Command::new("cargo");
    build_cmd.current_dir(&fuzz_dir).args(["+nightly", "fuzz", "build"]);
    if let Some(triple) = &config.target_triple {
        build_cmd.args(["--target", triple]);
    }
    if config.filter.is_some() {
        for target in &targets {
            build_cmd.arg(target);
        }
    }
    let build_status = build_cmd.status().map_err(|err| {
        format!("failed to run `cargo +nightly fuzz build` (is cargo-fuzz + nightly installed?): {err}")
    })?;
    if !build_status.success() {
        return Err(format!("`cargo +nightly fuzz build` failed (exit {build_status})"));
    }

    // 3. For each target, replay every committed seed under libFuzzer + ASAN, then fuzz briefly
    //    into a throwaway corpus directory so `fuzz/seeds/` remains strictly read-only.
    let scratch_root = std::env::temp_dir().join(format!("apack-fuzz-smoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch_root);
    fs::create_dir_all(&scratch_root)
        .map_err(|err| format!("failed to create scratch dir {}: {err}", scratch_root.display()))?;
    let _guard = TempDirGuard(scratch_root.clone());

    for target in &targets {
        let seed_dir = fuzz_dir.join("seeds").join(target);
        let work_dir = scratch_root.join(target);
        fs::create_dir_all(&work_dir)
            .map_err(|err| format!("failed to create work corpus dir {}: {err}", work_dir.display()))?;

        eprintln!("[fuzz-smoke] {target}: replaying committed seeds under libFuzzer...");
        run_libfuzzer(&fuzz_dir, &config, target, &work_dir, &seed_dir, &["-runs=0".to_owned()])?;

        if config.seconds > 0 || config.runs.is_some() {
            let mut flags = vec![format!("-max_len={}", config.max_len)];
            if config.seconds > 0 {
                flags.push(format!("-max_total_time={}", config.seconds));
            }
            if let Some(runs) = config.runs {
                flags.push(format!("-runs={runs}"));
            }
            eprintln!("[fuzz-smoke] {target}: fuzzing ({}, rss limit {} MiB)...", flags.join(" "), config.rss_limit_mb);
            run_libfuzzer(&fuzz_dir, &config, target, &work_dir, &seed_dir, &flags)?;
        }
    }

    Ok(())
}

fn run_libfuzzer(
    fuzz_dir: &Path,
    config: &Config,
    target: &str,
    work_dir: &Path,
    seed_dir: &Path,
    extra_flags: &[String],
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(fuzz_dir).args(["+nightly", "fuzz", "run"]);
    if let Some(triple) = &config.target_triple {
        cmd.args(["--target", triple]);
    }
    cmd.arg(target)
        .arg(work_dir)
        .arg(seed_dir)
        .arg("--")
        .arg(format!("-rss_limit_mb={}", config.rss_limit_mb))
        .args(extra_flags);

    let status = cmd.status().map_err(|err| format!("failed to execute `cargo +nightly fuzz run {target}`: {err}"))?;
    if !status.success() {
        return Err(format!("fuzz target `{target}` failed (exit {status}); check fuzz/artifacts/{target}/"));
    }
    Ok(())
}

/// Parses every `[[bin]]` entry's `name = "..."` from `fuzz/Cargo.toml`.
pub fn discover_targets(manifest: &str) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();
    let mut in_bin = false;
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_bin = line == "[[bin]]";
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let value = rest.trim();
                let unquoted = value
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .ok_or_else(|| format!("malformed [[bin]] name in fuzz/Cargo.toml: {line}"))?;
                if !unquoted.is_empty() {
                    targets.push(unquoted.to_owned());
                }
            }
        }
    }
    Ok(targets)
}

fn validate_seed_dir(seed_dir: &Path, target: &str) -> Result<usize, String> {
    if !seed_dir.is_dir() {
        return Err(format!("missing seed corpus directory for `{target}`: {}", seed_dir.display()));
    }
    let mut count = 0usize;
    let entries =
        fs::read_dir(seed_dir).map_err(|err| format!("failed to read seed dir {}: {err}", seed_dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|err| format!("seed dir entry error: {err}"))?.path();
        if !path.is_file() {
            continue;
        }
        let len = fs::metadata(&path).map_err(|err| format!("failed to stat seed {}: {err}", path.display()))?.len();
        if len > MAX_SEED_BYTES {
            return Err(format!(
                "seed {} is {} bytes, exceeding compact limit of {} bytes",
                path.display(),
                len,
                MAX_SEED_BYTES
            ));
        }
        count += 1;
    }
    if count == 0 {
        return Err(format!("seed corpus directory for `{target}` is empty: {}", seed_dir.display()));
    }
    Ok(count)
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seconds" => {
                i += 1;
                let raw = args.get(i).ok_or("--seconds requires a value")?;
                cfg.seconds = raw.parse().map_err(|_| format!("invalid --seconds value: {raw}"))?;
            }
            "--runs" => {
                i += 1;
                let raw = args.get(i).ok_or("--runs requires a value")?;
                let val: u64 = raw.parse().map_err(|_| format!("invalid --runs value: {raw}"))?;
                if val == 0 {
                    return Err("--runs must be > 0".to_owned());
                }
                cfg.runs = Some(val);
            }
            "--rss-limit-mb" => {
                i += 1;
                let raw = args.get(i).ok_or("--rss-limit-mb requires a value")?;
                let val: u64 = raw.parse().map_err(|_| format!("invalid --rss-limit-mb value: {raw}"))?;
                if val == 0 {
                    return Err("--rss-limit-mb must be > 0".to_owned());
                }
                cfg.rss_limit_mb = val;
            }
            "--max-len" => {
                i += 1;
                let raw = args.get(i).ok_or("--max-len requires a value")?;
                let val: u64 = raw.parse().map_err(|_| format!("invalid --max-len value: {raw}"))?;
                if val == 0 {
                    return Err("--max-len must be > 0".to_owned());
                }
                cfg.max_len = val;
            }
            "--target" => {
                i += 1;
                let raw = args.get(i).ok_or("--target requires a target triple")?;
                cfg.target_triple = Some(raw.clone());
            }
            "--filter" => {
                i += 1;
                let raw = args.get(i).ok_or("--filter requires a substring")?;
                cfg.filter = Some(raw.clone());
            }
            "--replay-only" => cfg.replay_only = true,
            other => {
                return Err(format!(
                    "unknown fuzz-smoke argument `{other}`\nusage: cargo xtask fuzz-smoke [--seconds N] [--runs N] [--rss-limit-mb M] [--max-len B] [--target TRIPLE] [--filter SUBSTR] [--replay-only]"
                ));
            }
        }
        i += 1;
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_all_bin_targets_and_parses_flags() {
        let toml = r#"
[package]
name = "apack-terminal-fuzz"

[[bin]]
name = "v2_wire"
path = "fuzz_targets/v2_wire.rs"

[[bin]]
name = "v2_kitty_apc" # comment
path = "fuzz_targets/v2_kitty_apc.rs"
"#;
        assert_eq!(discover_targets(toml).unwrap(), vec!["v2_wire", "v2_kitty_apc"]);

        let cfg = parse_args(&[
            "--seconds".into(),
            "5".into(),
            "--runs".into(),
            "1000".into(),
            "--rss-limit-mb".into(),
            "512".into(),
            "--target".into(),
            "x86_64-unknown-linux-gnu".into(),
            "--filter".into(),
            "wire".into(),
        ])
        .unwrap();
        assert_eq!(cfg.seconds, 5);
        assert_eq!(cfg.runs, Some(1000));
        assert_eq!(cfg.rss_limit_mb, 512);
        assert_eq!(cfg.target_triple.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(cfg.filter.as_deref(), Some("wire"));
        assert!(!cfg.replay_only);

        assert!(parse_args(&["--runs".into(), "0".into()]).is_err());
        assert!(parse_args(&["--rss-limit-mb".into(), "0".into()]).is_err());
        assert!(parse_args(&["--bogus".into()]).is_err());
    }
}
