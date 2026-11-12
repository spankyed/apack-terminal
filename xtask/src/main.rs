//! The repository's own tasks, run as `cargo xtask <task>`:
//!
//! - `generate`: writes the protocol's TypeScript declarations into `protocol/`, derived from `src/protocol.rs`.
//! - `check-generated`: fails when what `generate` would write differs from what is committed.
//! - `package`: builds the broker for release and stages it in the npm package (`node/`), which is how it
//!   reaches a packaged app: the apack monorepo resolves that package as an ordinary dependency.
//! - `pre-push`: every check CI runs on a change, the change's mutants included, cheapest first, stopping at
//!   the first that fails: what to run before a push.
//! - `vt-prebuilt`: fetches and verifies the `libghostty-vt` archive the `ghostty` feature links, so the
//!   build itself reaches no network.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use xtask::{generate, leftovers, mutants, package, pre_push, safety, vt_prebuilt};

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let root = workspace_root();
    let result = match task.as_deref() {
        Some("generate") => generate::update(&root),
        Some("check-generated") => generate::check(&root),
        Some("check-exclusions") => mutants::check_exclusions(&root),
        Some("check-safety") => safety::check(&root),
        Some("clear-mutants") => leftovers::clear(&root),
        Some("package") => package::package(&root),
        Some("vt-prebuilt") => vt_prebuilt::fetch(&root),
        Some("pre-push") => pre_push::pre_push(
            &root,
            &std::env::args().skip(2).collect::<Vec<_>>(),
            &[
                ("generated protocol", &|| generate::check(&root)),
                ("mutant exclusions", &|| mutants::check_exclusions(&root)),
                ("unsafe justifications", &|| safety::check(&root)),
            ],
        ),
        _ => {
            Err("usage: cargo xtask <generate | check-generated | check-exclusions | check-safety | clear-mutants | package | vt-prebuilt | pre-push>"
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
