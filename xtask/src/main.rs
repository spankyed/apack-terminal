//! The repository's own tasks, run as `cargo xtask <task>`:
//!
//! - `generate`: regenerates `view/src/wire/{constants,types,messages}.ts` and `node/wire-vectors-v2.json` from `apack-proto` and `apack-core`.
//! - `check-generated`: fails when committed generated TS wire files or `node/wire-vectors-v2.json` differ from what `generate` writes.
//! - `notices`: writes the release binaries' third-party notices, `THIRD-PARTY-NOTICES` and
//!   `COPYRIGHT-rust-library.html`, at the repository's root (`src/notices.rs`).
//! - `check-notices`: fails when what `notices` would write differs from what is committed.
//! - `check-exclusions`: fails when a `.cargo/mutants.toml` exclusion matches no mutant or a Windows-only file is missing.
//! - `check-cfg-hygiene`: fails when mutable `#[cfg(windows)]` / `#[cfg(not(unix))]` logic appears outside `.cargo/windows-only`.
//! - `check-windows`: runs a fast local Windows clippy/type cross-check (`x86_64-pc-windows-msvc` by default).
//! - `check-safety`: fails when any `unsafe` site lacks a `SAFETY:` comment above it.
//! - `check-versions`: fails when Rust crate versions, `node/package.json` (plus platform `optionalDependencies`),
//!   or `view/package.json` drift out of lockstep.
//! - `clear-mutants`: removes working copies left behind by an interrupted mutation pass.
//! - `pre-push`: every check CI runs on a change, the change's mutants included, cheapest first, stopping at
//!   the first that fails: what to run before a push.
//! - `bench`: builds the release broker binary and runs the reproducible `v2_bench` suite.
//! - `fuzz-smoke`: replays committed fuzz seed corpora and runs a bounded nightly libFuzzer smoke check.
//! - `vt-prebuilt`: fetches and verifies the prebuilt `libghostty-vt` archive `ghostty-vt-rs-sys` links, so the
//!   build itself reaches no network.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use xtask::{
    bench, cfg_hygiene, fuzz_smoke, generate, leftovers, mutants, notices, pre_push, safety, versions, vt_prebuilt,
    windows_check,
};

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let root = workspace_root();
    let result = match task.as_deref() {
        Some("bench") => bench::run(&root, &std::env::args().skip(2).collect::<Vec<_>>()),
        Some("fuzz-smoke") => fuzz_smoke::run(&root, &std::env::args().skip(2).collect::<Vec<_>>()),
        Some("generate") => generate::update(&root),
        Some("check-generated") => generate::check(&root),
        Some("notices") => notices::write(&root),
        Some("check-notices") => notices::check(&root),
        Some("check-exclusions") => mutants::check_exclusions(&root),
        Some("check-cfg-hygiene") => cfg_hygiene::check(&root),
        Some("check-windows") => windows_check::run(&root, &std::env::args().skip(2).collect::<Vec<_>>()),
        Some("check-safety") => safety::check(&root),
        Some("check-versions") => versions::check(&root),
        Some("clear-mutants") => leftovers::clear(&root),
        Some("vt-prebuilt") => vt_prebuilt::fetch(&root),
        Some("pre-push") => pre_push::pre_push(
            &root,
            &std::env::args().skip(2).collect::<Vec<_>>(),
            &[
                ("package version lockstep", &|| versions::check(&root)),
                ("generated protocol", &|| generate::check(&root)),
                ("mutant exclusions", &|| mutants::check_exclusions(&root)),
                ("windows cfg hygiene", &|| cfg_hygiene::check(&root)),
                ("unsafe justifications", &|| safety::check(&root)),
                ("third-party notices", &|| notices::check(&root)),
            ],
        ),
        _ => {
            Err("usage: cargo xtask <bench | fuzz-smoke | generate | check-generated | notices | check-notices | check-exclusions | check-cfg-hygiene | check-windows | check-safety | check-versions | clear-mutants | vt-prebuilt | pre-push>"
                .to_owned())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits in the workspace root").to_path_buf()
}
