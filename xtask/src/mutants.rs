//! Every exclusion in `.cargo/mutants.toml` excludes something.
//!
//! **A pattern that matches no mutant is worse than no pattern**, because it reads as a decision that was
//! taken: the gate goes on reporting the survivor the exclusion was written for, and the next person reads
//! the comment above it and believes the question is settled. It has happened twice. `security.rs`'s two
//! patterns were written as `in <fn>`, which cargo-mutants prints only for a mutant *inside* a function, so
//! they matched nothing at all; and `tree.rs`'s was written with one backslash, which is not a TOML escape,
//! so the file stopped parsing and every mutants run failed instead.
//!
//! Both were found by CI rather than by the author, which is what this is for. It asks cargo-mutants itself
//! — `--re` filters by the same matching `--exclude-re` uses — so there is no second implementation of the
//! rule to disagree with the first.

use std::path::Path;
use std::process::Command;

/// Where the exclusions are declared, and the key that holds them.
const CONFIG: &str = ".cargo/mutants.toml";
const KEY: &str = "exclude_re = [";

/// Each exclusion, as cargo-mutants receives it.
///
/// The block is read rather than the whole file parsed, and **anything in it that is neither a comment nor
/// a quoted string is a refusal**: a loose scan that skipped what it did not recognise could check nothing
/// and say so in the same words as checking everything.
pub fn exclusions(config: &str) -> Result<Vec<String>, String> {
    let block = config
        .split_once(KEY)
        .ok_or_else(|| format!("{CONFIG} declares no {KEY}; this check has lost its subject"))?
        .1
        .split_once("\n]")
        .ok_or_else(|| format!("{CONFIG}'s {KEY} is never closed"))?
        .0;
    let mut patterns = Vec::new();
    for line in block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let literal = line
            .strip_suffix(',')
            .unwrap_or(line)
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .ok_or_else(|| format!("{CONFIG}: `{line}` is neither a comment nor a quoted pattern"))?;
        // A TOML basic string's `\\` is one backslash by the time cargo-mutants sees it
        patterns.push(literal.replace("\\\\", "\\"));
    }
    if patterns.is_empty() {
        return Err(format!("{CONFIG} excludes nothing, so there is nothing to check"));
    }
    Ok(patterns)
}

/// Lists the mutants matching `pattern`, with the config out of the way so an exclusion cannot hide itself.
fn matches(root: &Path, pattern: &str) -> Result<usize, String> {
    let output = Command::new("cargo")
        .args(["mutants", "--list", "--no-config", "--re", pattern])
        .current_dir(root)
        .output()
        .map_err(|error| format!("cargo mutants didn't start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo mutants --list refused `{pattern}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().filter(|line| !line.trim().is_empty()).count())
}

/// The verdict, over whatever counts the mutants are: separated from the subprocess so the decision can be
/// driven with counts of its own, which is the only way to see this check fail without a dead pattern
/// committed to the file it reads.
pub fn verdict(patterns: &[String], count: impl Fn(&str) -> Result<usize, String>) -> Result<(), String> {
    // The whole population first: a mutants run that lists nothing would make every pattern look dead
    if count(".")? == 0 {
        return Err("cargo mutants --list found no mutants at all; this check looked at nothing".to_owned());
    }
    let dead = patterns
        .iter()
        .filter_map(|pattern| match count(pattern) {
            Ok(0) => Some(Ok(pattern.clone())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, String>>()?;
    if dead.is_empty() {
        println!("{CONFIG}: {} exclusions, each excluding something", patterns.len());
        return Ok(());
    }
    Err(format!(
        "{CONFIG}: {} of {} exclusions match no mutant, so they exclude nothing and the gate still reports \
         what they were written for:\n  {}",
        dead.len(),
        patterns.len(),
        dead.join("\n  ")
    ))
}

/// Fails naming every exclusion that excludes nothing.
pub fn check_exclusions(root: &Path) -> Result<(), String> {
    let config = std::fs::read_to_string(root.join(CONFIG)).map_err(|error| format!("{CONFIG}: {error}"))?;
    verdict(&exclusions(&config)?, |pattern| matches(root, pattern))
}
