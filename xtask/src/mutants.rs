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
        let entry = line.strip_suffix(',').unwrap_or(line);
        let pattern = if let Some(literal) = entry.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')) {
            literal.to_owned()
        } else if let Some(basic) = entry.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
            unescape_toml_basic(basic)
        } else {
            return Err(format!("{CONFIG}: `{line}` is neither a comment nor a quoted pattern"));
        };
        patterns.push(pattern);
    }
    if patterns.is_empty() {
        return Err(format!("{CONFIG} excludes nothing, so there is nothing to check"));
    }
    Ok(patterns)
}

fn unescape_toml_basic(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// A pattern no mutant's name can match: what cargo-mutants lists for it, it lists whatever it is asked.
const MATCHES_NOTHING: &str = r"[^\s\S]";

/// Lists the mutants cargo-mutants answers `pattern` with, with the config out of the way so an exclusion
/// cannot hide itself.
fn listed(root: &Path, pattern: &str) -> Result<Vec<String>, String> {
    let output = Command::new("cargo")
        .args(["mutants", "--workspace", "--list", "--no-config", "--re", pattern])
        .current_dir(root)
        .output()
        .map_err(|error| format!("cargo mutants didn't start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo mutants --list refused `{pattern}`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// How many of the mutants `listing` names a pattern chose: the ones not in `unfiltered`, which is what
/// cargo-mutants lists for a pattern that can match nothing.
///
/// **cargo-mutants 27.1 lists some mutants whatever it is asked** (its `delete field … from struct`
/// mutants pass `--re` and `--exclude-re` alike), so a count of every line it prints is never zero and no
/// pattern would ever read as dead. Those mutants are not a pattern's doing, and an exclusion written for
/// one excludes nothing, so they count for no pattern.
pub fn chosen(listing: &[String], unfiltered: &[String]) -> usize {
    listing.iter().filter(|mutant| !unfiltered.contains(mutant)).count()
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

/// The files only Windows compiles (`.cargo/windows-only`), whose mutants a run elsewhere leaves out.
pub const WINDOWS_ONLY: &str = ".cargo/windows-only";

/// The files `WINDOWS_ONLY` lists: each line that is neither blank nor a comment.
pub fn windows_only(listed: &str) -> Vec<String> {
    listed.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).map(str::to_owned).collect()
}

/// The same, read from where they are listed, refusing a list that names nothing or a file that is not
/// there: a run told to leave out a file that has gone leaves out nothing, and says it did.
pub fn windows_only_files(root: &Path) -> Result<Vec<String>, String> {
    let listed =
        std::fs::read_to_string(root.join(WINDOWS_ONLY)).map_err(|error| format!("{WINDOWS_ONLY}: {error}"))?;
    let files = windows_only(&listed);
    if files.is_empty() {
        return Err(format!("{WINDOWS_ONLY} lists no file, so there is nothing to leave out"));
    }
    let gone: Vec<&String> = files.iter().filter(|file| !root.join(file).is_file()).collect();
    if !gone.is_empty() {
        return Err(format!("{WINDOWS_ONLY} lists files that are not there: {gone:?}"));
    }
    Ok(files)
}

/// Fails naming every exclusion that excludes nothing, and every Windows-only file that is not there.
pub fn check_exclusions(root: &Path) -> Result<(), String> {
    let config = std::fs::read_to_string(root.join(CONFIG)).map_err(|error| format!("{CONFIG}: {error}"))?;
    windows_only_files(root)?;
    let unfiltered = listed(root, MATCHES_NOTHING)?;
    verdict(&exclusions(&config)?, |pattern| Ok(chosen(&listed(root, pattern)?, &unfiltered)))
}
