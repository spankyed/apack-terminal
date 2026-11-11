//! The working copies a mutation pass left behind when it was ended rather than finished.
//!
//! **cargo-mutants tests every mutant in a copy of the tree, and removes the copy when it ends itself.** A pass
//! that is interrupted, or dies with the session that started it, removes nothing: each copy is this tree and
//! a build of it, a few hundred megabytes, in the system's temporary directory where nothing looks. Twelve
//! were found on a disk with four gigabytes left (2026-11-11). So a pass begins by removing the ones before
//! it, and `cargo xtask clear-mutants` does the same by itself.
//!
//! **Only while no pass is running here**, since a running one's copies are named the same way. cargo-mutants
//! holds a lock on `lock.json` in its output directory for as long as it runs, which is asked for and not
//! waited on: a pass in progress leaves everything where it is.

use std::path::{Path, PathBuf};

/// Where a pass writes what it found, which holds the lock a running pass keeps (`output` in `.cargo/mutants.toml`)
const PASS_LOCK: &str = "target/mutants.out/lock.json";

/// The package a manifest names: its first `name = "…"`, which in a manifest that begins with `[package]`
/// or `[workspace]` then `[package]` is the package's.
fn package_named(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let line = text.lines().find(|line| line.trim_start().starts_with("name"))?;
    Some(line.split('"').nth(1)?.to_owned())
}

/// The copies in `temp` that a pass over `root` made: named for `root`'s directory as cargo-mutants names
/// them, and holding a manifest for the same package.
///
/// **Both, because a name is not an owner.** Another checkout may sit in a directory of the same name, and
/// what is about to happen to each of these is that it is removed.
pub fn copies_of(root: &Path, temp: &Path) -> Vec<PathBuf> {
    let (Some(directory), Some(package)) =
        (root.file_name().and_then(|name| name.to_str()), package_named(&root.join("Cargo.toml")))
    else {
        return Vec::new();
    };
    let prefix = format!("cargo-mutants-{directory}-");
    let Ok(entries) = std::fs::read_dir(temp) else { return Vec::new() };
    let mut copies: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".tmp"))
                && path.is_dir()
                && package_named(&path.join("Cargo.toml")).as_ref() == Some(&package)
        })
        .collect();
    copies.sort();
    copies
}

/// Removes the copies earlier passes over `root` left in `temp`, answering how many: none, and nothing
/// removed, while a pass is running here.
pub fn clear_in(root: &Path, temp: &Path) -> Result<usize, String> {
    // Held for the removal, so a pass that starts meanwhile waits for it rather than losing its copy
    let _held = match std::fs::File::open(root.join(PASS_LOCK)) {
        Ok(lock) => match lock.try_lock() {
            Ok(()) => Some(lock),
            Err(_) => return Ok(0),
        },
        // No pass has ever written here, so none is running
        Err(_) => None,
    };
    let copies = copies_of(root, temp);
    for copy in &copies {
        std::fs::remove_dir_all(copy).map_err(|e| format!("{}: {e}", copy.display()))?;
    }
    Ok(copies.len())
}

/// `cargo xtask clear-mutants`: removes what earlier passes over this checkout left behind, and says so.
pub fn clear(root: &Path) -> Result<(), String> {
    match clear_in(root, &std::env::temp_dir())? {
        0 => println!("clear-mutants: nothing an earlier pass left, or a pass is running"),
        cleared => println!(
            "clear-mutants: removed {cleared} working cop{} an earlier pass left",
            if cleared == 1 { "y" } else { "ies" }
        ),
    }
    Ok(())
}
