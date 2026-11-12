//! `check-safety`: every `unsafe` the broker's source and tests write says, in a comment just above it, why it
//! holds. An `unsafe fn` may say so in its doc comment's `# Safety` section instead.
//!
//! The scan is by line, so it reads a keyword in code and not in a string or a comment, and it does not mistake
//! `allow_unsafe` (an identifier) for one. A justification counts only if it is between this site and the one
//! before it, so one comment cannot cover two blocks. A scan that found no `unsafe` at all fails, rather than
//! passing over nothing.

use std::path::{Path, PathBuf};

/// The trees scanned, relative to the repository root.
pub const ROOTS: [&str; 2] = ["src", "tests"];

/// How many lines above a site a justification may sit.
const WINDOW: usize = 8;

/// Checks every `unsafe` under [`ROOTS`]: a failure names each site without a justification.
pub fn check(root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    for tree in ROOTS {
        collect(&root.join(tree), &mut files)?;
    }
    files.sort();
    let mut sites = 0;
    let mut missing = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|error| format!("{}: {error}", file.display()))?;
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !is_unsafe_site(line) {
                continue;
            }
            sites += 1;
            if !justified(&lines, index) {
                let shown = file.strip_prefix(root).unwrap_or(file).display();
                missing.push(format!("{shown}:{}: {}", index + 1, line.trim()));
            }
        }
    }
    if sites == 0 {
        return Err(format!("check-safety found no `unsafe` under {}: nothing was read", ROOTS.join(", ")));
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} unsafe site(s) without a SAFETY comment above them (of {sites}):\n  {}",
        missing.len(),
        missing.join("\n  ")
    ))
}

/// Every `.rs` file under `dir`, recursively. A missing tree is an error: the check does not pass over it.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|error| format!("{}: {error}", dir.display()))?.path();
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether a line has `unsafe` as code: not inside a string literal, not in a line comment, and as a whole word.
pub fn is_unsafe_site(line: &str) -> bool {
    contains_word(&code_of(line), "unsafe")
}

/// The part of a line that is code: string literal contents and everything after `//` removed.
fn code_of(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    while let Some(character) = chars.next() {
        if in_string {
            match character {
                '\\' => {
                    chars.next();
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '/' if chars.peek() == Some(&'/') => break,
            _ => out.push(character),
        }
    }
    out
}

fn contains_word(text: &str, word: &str) -> bool {
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// Whether the site at `index` has a justification above it: a `SAFETY` comment, or a `# Safety` doc section,
/// within [`WINDOW`] lines and before the previous site.
fn justified(lines: &[&str], index: usize) -> bool {
    if lines[index].contains("SAFETY:") {
        return true;
    }
    let lowest = index.saturating_sub(WINDOW);
    for above in lines[lowest..index].iter().rev() {
        if is_unsafe_site(above) {
            return false;
        }
        let trimmed = above.trim_start();
        if trimmed.starts_with("//") && (trimmed.contains("SAFETY:") || trimmed.contains("# Safety")) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_unsafe_as_code_and_not_in_text_or_identifiers() {
        assert!(is_unsafe_site("    let x = unsafe { f() };"));
        assert!(is_unsafe_site("unsafe impl Send for T {}"));
        assert!(is_unsafe_site("pub unsafe extern \"C\" fn f() {}"));
        assert!(!is_unsafe_site("    let allow_unsafe = true;"));
        assert!(!is_unsafe_site("    // this is not unsafe"));
        assert!(!is_unsafe_site("    named(&[\"pasted\", \"unsafe\"])"));
    }

    #[test]
    fn a_comment_above_justifies_a_site_and_nothing_further_away() {
        let justified_site = ["// SAFETY: the handle is live", "let x = unsafe { f() };"];
        assert!(justified(&justified_site, 1));

        let bare = ["let y = 1;", "let x = unsafe { f() };"];
        assert!(!justified(&bare, 1));

        // The comment above the first site does not justify the second
        let shared = ["// SAFETY: the first", "let a = unsafe { f() };", "let b = unsafe { g() };"];
        assert!(justified(&shared, 1));
        assert!(!justified(&shared, 2));

        let doc = ["/// # Safety", "/// The caller holds the lock.", "pub unsafe fn f() {}"];
        assert!(justified(&doc, 2));
    }

    #[test]
    fn a_tree_with_no_unsafe_is_refused() {
        let directory = std::env::temp_dir().join(format!("check-safety-empty-{}", std::process::id()));
        for tree in ROOTS {
            std::fs::create_dir_all(directory.join(tree)).unwrap();
            std::fs::write(directory.join(tree).join("lib.rs"), "fn f() {}\n").unwrap();
        }
        let refused = check(&directory);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(refused.unwrap_err().contains("nothing was read"));
    }
}
