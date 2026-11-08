//! `pre-push`: every check CI fails a change on, in one command, cheapest first, stopping at the first that
//! fails. What a push would have CI find is found here instead, the change's mutants included, which nothing
//! else runs before a push.
//!
//! What it cannot run is CI's alone: the Windows and Linux suites, and the stage of this platform's binary
//! on each of the others. Everything in `src/tree.rs` and every
//! ConPTY claim is Windows-only by construction, so a change there is proven by a Windows run or not at all
//! (`CLAUDE.md`, "A platform claim is made on that platform").

use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// Where the diff a mutants run reads goes
const CHANGE_DIFF: &str = "target/pre-push/change.diff";

/// One check: what it is called, the command it runs from the workspace root, and what it adds to the
/// environment
struct Step {
    name: &'static str,
    command: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

const fn step(name: &'static str, command: &'static [&'static str]) -> Step {
    Step { name, command, env: &[] }
}

/// As CI runs them (`.github/workflows/ci.yml`): the cheap checks, then the suites
const FIRST: &[Step] = &[
    step("locked dependencies", &["cargo", "fetch", "--locked"]),
    step("format", &["cargo", "fmt", "--all", "--check"]),
    step("clippy", &["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
];

const THEN: &[Step] = &[
    step("tests", &["cargo", "nextest", "run", "--workspace"]),
    step("doc tests", &["cargo", "test", "--workspace", "--doc"]),
];

/// A repository check this task runs in-process, by the name it prints
pub type Check<'a> = (&'a str, &'a dyn Fn() -> Result<(), String>);

fn run(root: &Path, step: &Step) -> Result<(), String> {
    let (program, args) = step.command.split_first().expect("a step runs something");
    let started = Instant::now();
    println!("pre-push: {}", step.name);
    let status = Command::new(program)
        .args(args.iter().copied())
        .envs(step.env.iter().copied())
        .current_dir(root)
        .status()
        .map_err(|e| format!("pre-push: {} didn't start: {e}", step.name))?;
    if !status.success() {
        return Err(format!("pre-push: {} failed ({status}): the push would fail CI", step.name));
    }
    println!("pre-push: {} passed in {:.0?}", step.name, started.elapsed());
    Ok(())
}

/// The tracked files with changes not committed: what is checked has to be what is pushed, and the change's
/// mutants are read from what is committed alone
fn uncommitted(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git status: {e}"))?;
    if !output.status.success() {
        return Err(format!("git status failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

/// The change against `base`, as cargo-mutants reads it: a mutant outside what changed is not this push's
/// to answer for, and a whole-repository run is minutes of mutants nobody edited
fn mutants_of_the_change(root: &Path, base: &str) -> Result<(), String> {
    let diff = root.join(CHANGE_DIFF);
    std::fs::create_dir_all(diff.parent().expect("a parent")).map_err(|e| format!("{CHANGE_DIFF}: {e}"))?;
    let output = Command::new("git")
        .args(["diff", "--merge-base", base, "--", "*.rs"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git diff against {base}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff against {base} failed: {}; is {base} fetched?",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    if output.stdout.is_empty() {
        println!("pre-push: mutants of the change: the change touches no Rust");
        return Ok(());
    }
    std::fs::write(&diff, &output.stdout).map_err(|e| format!("{CHANGE_DIFF}: {e}"))?;
    run(
        root,
        &Step { name: "mutants of the change", command: &["cargo", "mutants", "--in-diff", CHANGE_DIFF], env: &[] },
    )
}

/// `cargo xtask pre-push [--base REF]`: REF is what the change merges into, origin/master unless given,
/// against which its mutants are read
pub fn pre_push(root: &Path, args: &[String], checks: &[Check<'_>]) -> Result<(), String> {
    let base = match args {
        [] => "origin/master".to_owned(),
        [flag, base] if flag == "--base" => base.clone(),
        _ => return Err("usage: cargo xtask pre-push [--base REF]".to_owned()),
    };
    let dirty = uncommitted(root)?;
    if !dirty.is_empty() {
        return Err(format!("pre-push checks what is committed; commit or set aside first:\n{}", dirty.join("\n")));
    }
    let started = Instant::now();
    for step in FIRST {
        run(root, step)?;
    }
    for (name, check) in checks {
        println!("pre-push: {name}");
        check().map_err(|e| format!("pre-push: {name} failed: {e}"))?;
    }
    for step in THEN {
        run(root, step)?;
    }
    // The release build and the stage, which is how the broker reaches a packaged app. Last of the checks
    // because it is the longest, and before the mutants because a mutants run that cannot build is a
    // failure about the build
    println!("pre-push: npm package");
    crate::package::package(root)?;
    mutants_of_the_change(root, &base)?;
    println!("pre-push: everything passed in {:.0?}", started.elapsed());
    Ok(())
}
