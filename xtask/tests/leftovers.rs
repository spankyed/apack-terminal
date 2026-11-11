//! What a pass removes of the passes before it, and what it leaves.

use std::path::Path;

use xtask::leftovers::{clear_in, copies_of};

/// A checkout named `name` of the package `package`, under `parent`
fn checkout(parent: &Path, name: &str, package: &str) -> std::path::PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("a checkout");
    std::fs::write(root.join("Cargo.toml"), format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\n"))
        .expect("a manifest");
    root
}

/// A directory in `temp` named as cargo-mutants names a copy of a tree in `directory`, holding `package`
fn copy(temp: &Path, directory: &str, suffix: &str, package: &str) -> std::path::PathBuf {
    let made = checkout(temp, &format!("cargo-mutants-{directory}-{suffix}.tmp"), package);
    std::fs::create_dir_all(made.join("target/debug")).expect("a build");
    std::fs::write(made.join("target/debug/artifact"), "built").expect("an artifact");
    made
}

#[test]
fn a_pass_removes_the_copies_earlier_passes_over_this_checkout_left() {
    let work = tempfile::tempdir().expect("a directory");
    let temp = work.path().join("temp");
    let root = checkout(&work.path().join("checkouts"), "terminal-view", "apack-terminal");
    let (first, second) = (
        copy(&temp, "terminal-view", "0GNsKD", "apack-terminal"),
        copy(&temp, "terminal-view", "8XLaVj", "apack-terminal"),
    );

    assert_eq!(clear_in(&root, &temp), Ok(2));
    assert!(!first.exists() && !second.exists());
}

#[test]
fn what_is_not_a_copy_of_this_checkout_is_left_where_it_is() {
    let work = tempfile::tempdir().expect("a directory");
    let temp = work.path().join("temp");
    let root = checkout(&work.path().join("checkouts"), "terminal-view", "apack-terminal");
    // Another repository in a directory of the same name, another checkout of this one, and somebody else's
    let same_name = copy(&temp, "terminal-view", "aaaaaa", "something-else");
    let other_checkout = copy(&temp, "other-worktree", "bbbbbb", "apack-terminal");
    let unrelated = checkout(&temp, "terminal-view-notes", "apack-terminal");
    std::fs::write(temp.join("cargo-mutants-terminal-view-cccccc.tmp.log"), "a file").expect("a file");

    assert_eq!(copies_of(&root, &temp), Vec::<std::path::PathBuf>::new());
    assert_eq!(clear_in(&root, &temp), Ok(0));
    assert!(same_name.exists() && other_checkout.exists() && unrelated.exists());
}

#[test]
fn nothing_is_removed_while_a_pass_is_running_here() {
    let work = tempfile::tempdir().expect("a directory");
    let temp = work.path().join("temp");
    let root = checkout(&work.path().join("checkouts"), "terminal-view", "apack-terminal");
    let running = copy(&temp, "terminal-view", "dddddd", "apack-terminal");
    // As a running pass does: its output directory's lock file, held
    std::fs::create_dir_all(root.join("target/mutants.out")).expect("an output directory");
    let lock = std::fs::File::create(root.join("target/mutants.out/lock.json")).expect("a lock file");
    lock.lock().expect("the lock is taken");

    assert_eq!(clear_in(&root, &temp), Ok(0));
    assert!(running.exists(), "a running pass's copy was removed from under it");

    drop(lock);
    assert_eq!(clear_in(&root, &temp), Ok(1), "and it goes once the pass has ended");
    assert!(!running.exists());
}
