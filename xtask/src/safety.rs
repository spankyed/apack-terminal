//! `check-safety`: every `unsafe` the broker's source and tests write says, in a comment just above it, why it
//! holds. An `unsafe fn` may say so in its doc comment's `# Safety` section instead.
//!
//! The scan is by line, so it reads a keyword in code and not in a string or a comment, and it does not mistake
//! `allow_unsafe` (an identifier) for one. A justification counts only if it is between this site and the one
//! before it, so one comment cannot cover two blocks. A scan that found no `unsafe` at all fails, rather than
//! passing over nothing.

use std::path::{Path, PathBuf};

/// The trees and files scanned, relative to the repository root.
pub const ROOTS: [&str; 7] = ["src", "crates", "tests", "benches", "xtask", "fuzz/fuzz_targets", "build.rs"];

/// How many lines above a site a justification may sit.
const WINDOW: usize = 8;

/// Checks every `unsafe` under [`ROOTS`]: a failure names each site without a justification.
pub fn check(root: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    for tree in ROOTS {
        let path = root.join(tree);
        if path.is_file() {
            files.push(path);
        } else {
            collect(&path, &mut files)?;
        }
    }
    files.sort();
    let mut sites = 0;
    let mut missing = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|error| format!("{}: {error}", file.display()))?;
        let lines: Vec<&str> = text.lines().collect();
        let stripped = strip_comments_and_literals(&lines);
        for (index, line) in lines.iter().enumerate() {
            if !contains_word(&stripped[index].code, "unsafe") {
                continue;
            }
            sites += 1;
            if !justified(&lines, &stripped, index) {
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

/// Whether a line has `unsafe` as code: not inside a string or char literal, not in a comment, and as a whole word.
pub fn is_unsafe_site(line: &str) -> bool {
    let stripped = strip_comments_and_literals(&[line]);
    contains_word(&stripped[0].code, "unsafe")
}

struct StrippedLine<'a> {
    code: String,
    trailing_comment: Option<&'a str>,
}

/// Code portions of each line with string/raw-string/char literals and `//` or `/* ... */` comments stripped,
/// preserving a space separator for omitted tokens and capturing any real trailing `//` comment.
fn strip_comments_and_literals<'a>(lines: &[&'a str]) -> Vec<StrippedLine<'a>> {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        String,
        RawString(usize),
        BlockComment(usize),
    }

    let mut state = State::Code;
    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        let bytes = line.as_bytes();
        let mut out = String::new();
        let mut trailing_comment = None;
        let mut i = 0;
        while i < bytes.len() {
            match state {
                State::Code => {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        trailing_comment = Some(&line[i..]);
                        break;
                    }
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        out.push(' ');
                        state = State::BlockComment(1);
                        i += 2;
                        continue;
                    }
                    // Raw string: `r"..."`, `r#"..."#`, `br#"..."#`, `cr#"..."#`
                    if (bytes[i] == b'r' || ((bytes[i] == b'b' || bytes[i] == b'c') && bytes.get(i + 1) == Some(&b'r')))
                        && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
                    {
                        let r_pos = if bytes[i] == b'r' { i } else { i + 1 };
                        let mut hashes = 0;
                        let mut j = r_pos + 1;
                        while j < bytes.len() && bytes[j] == b'#' {
                            hashes += 1;
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b'"' {
                            out.push(' ');
                            state = State::RawString(hashes);
                            i = j + 1;
                            continue;
                        }
                    }
                    if bytes[i] == b'"' {
                        out.push(' ');
                        state = State::String;
                        i += 1;
                        continue;
                    }
                    // Char or byte-char literal (`'x'`, `'"'`, `'\''`, `b'"'`, `'\x1b'`, `'\u{...}'`), not a lifetime (`'a`)
                    if bytes[i] == b'\''
                        && let Some(end) = char_literal_end(bytes, i)
                    {
                        out.push(' ');
                        i = end;
                        continue;
                    }
                    let ch = line[i..].chars().next().unwrap();
                    out.push(ch);
                    i += ch.len_utf8();
                }
                State::String => {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else if bytes[i] == b'"' {
                        state = State::Code;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                State::RawString(hashes) => {
                    if bytes[i] == b'"'
                        && i + 1 + hashes <= bytes.len()
                        && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
                    {
                        state = State::Code;
                        i += 1 + hashes;
                    } else {
                        i += 1;
                    }
                }
                State::BlockComment(depth) => {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        state = State::BlockComment(depth + 1);
                        i += 2;
                    } else if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        state = if depth == 1 { State::Code } else { State::BlockComment(depth - 1) };
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
        }
        result.push(StrippedLine { code: out, trailing_comment });
    }
    result
}

/// If `bytes[start]` (`'`) begins a character literal, returns the byte index just after the closing `'`.
fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'\\' {
        i += 1;
        if i >= bytes.len() {
            return None;
        }
        i += 1;
        while i < bytes.len() && bytes[i] != b'\'' {
            if bytes[i] == b' ' || bytes[i] == b';' {
                return None;
            }
            i += 1;
        }
        (i < bytes.len() && bytes[i] == b'\'').then_some(i + 1)
    } else {
        let s = std::str::from_utf8(&bytes[i..]).ok()?;
        let ch = s.chars().next()?;
        let next = i + ch.len_utf8();
        (bytes.get(next) == Some(&b'\'')).then_some(next + 1)
    }
}

fn contains_word(text: &str, word: &str) -> bool {
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// Whether `code` declares an `unsafe fn` (including `unsafe extern "C" fn`), which `# Safety` docs may justify.
fn is_unsafe_fn_decl(code: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices("unsafe").any(|(at, _)| {
        let before_ok = !code[..at].chars().next_back().is_some_and(is_word);
        let after_pos = at + "unsafe".len();
        let after_ok = !code[after_pos..].chars().next().is_some_and(is_word);
        if !(before_ok && after_ok) {
            return false;
        }
        let header = code[after_pos..].split(['{', ';', '=']).next().unwrap_or("");
        header.match_indices("fn").any(|(fn_at, _)| {
            let fb = !header[..fn_at].chars().next_back().is_some_and(is_word);
            let after_fn = &header[fn_at + 2..];
            let fa = !after_fn.chars().next().is_some_and(is_word);
            let named = after_fn.trim_start().chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
            fb && fa && named
        })
    })
}

/// Whether the site at `index` has a justification above it: a `SAFETY:` comment, or a `# Safety` doc section
/// (for an `unsafe fn` declaration), within [`WINDOW`] lines and before the previous site.
fn justified(lines: &[&str], stripped: &[StrippedLine<'_>], index: usize) -> bool {
    if stripped[index].trailing_comment.is_some_and(|comment| comment.contains("SAFETY:")) {
        return true;
    }
    let fn_decl = is_unsafe_fn_decl(&stripped[index].code);
    let lowest = index.saturating_sub(WINDOW);
    for at in (lowest..index).rev() {
        if contains_word(&stripped[at].code, "unsafe") {
            return false;
        }
        let trimmed = lines[at].trim_start();
        if trimmed.starts_with("//")
            && (trimmed.contains("SAFETY:") || (fn_decl && trimmed.starts_with("///") && trimmed.contains("# Safety")))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_justified(lines: &[&str], index: usize) -> bool {
        let stripped = strip_comments_and_literals(lines);
        justified(lines, &stripped, index)
    }

    #[test]
    fn finds_unsafe_as_code_and_not_in_text_or_identifiers() {
        assert!(is_unsafe_site("    let x = unsafe { f() };"));
        assert!(is_unsafe_site("unsafe impl Send for T {}"));
        assert!(is_unsafe_site("pub unsafe extern \"C\" fn f() {}"));
        assert!(!is_unsafe_site("    let allow_unsafe = true;"));
        assert!(!is_unsafe_site("    // this is not unsafe"));
        assert!(!is_unsafe_site("    named(&[\"pasted\", \"unsafe\"])"));
        // Char literal `'"'` and raw strings must not hide an `unsafe` later on the line
        assert!(is_unsafe_site("    let q = '\"'; unsafe { f() };"));
        assert!(is_unsafe_site("    let s = r#\"\\\"#; unsafe { f() };"));
        assert!(is_unsafe_site("    foo/*comment*/unsafe { f() }"));
        assert!(!is_unsafe_site("    /* unsafe { f() } */ let x = 1;"));
    }

    #[test]
    fn a_comment_above_justifies_a_site_and_nothing_further_away() {
        let justified_site = ["// SAFETY: the handle is live", "let x = unsafe { f() };"];
        assert!(is_justified(&justified_site, 1));

        let trailing = ["let x = unsafe { f() }; // SAFETY: inline justification"];
        assert!(is_justified(&trailing, 0));

        let bare = ["let y = 1;", "let x = unsafe { f() };"];
        assert!(!is_justified(&bare, 1));

        // SAFETY: (including `// SAFETY:`) inside a string literal does not justify the line
        let string_safety = ["let _ = \"SAFETY: fake\"; unsafe { f() };"];
        assert!(!is_justified(&string_safety, 0));
        let string_slash_safety = ["let _ = \"// SAFETY: fake\"; unsafe { f() };"];
        assert!(!is_justified(&string_slash_safety, 0));

        // The comment above the first site does not justify the second
        let shared = ["// SAFETY: the first", "let a = unsafe { f() };", "let b = unsafe { g() };"];
        assert!(is_justified(&shared, 1));
        assert!(!is_justified(&shared, 2));

        // `# Safety` justifies an `unsafe fn` declaration, not an `unsafe {}` block inside a safe fn
        let doc = ["/// # Safety", "/// The caller holds the lock.", "pub unsafe fn f() {}"];
        assert!(is_justified(&doc, 2));
        let doc_extern = ["/// # Safety", "pub unsafe extern \"C\" fn f() {}"];
        assert!(is_justified(&doc_extern, 1));
        let doc_block = ["/// # Safety", "fn f() {", "    unsafe { g() }", "}"];
        assert!(!is_justified(&doc_block, 2));
        let single_line_safe_fn = ["/// # Safety", "fn f() { unsafe { g() } }"];
        assert!(!is_justified(&single_line_safe_fn, 1));
        let fn_ptr = ["/// # Safety", "let f: fn() = unsafe { g() };"];
        assert!(!is_justified(&fn_ptr, 1));
        let unsafe_fn_ptr = ["/// # Safety", "let f: unsafe fn() = h;"];
        assert!(!is_justified(&unsafe_fn_ptr, 1));
    }

    #[test]
    fn a_tree_with_no_unsafe_is_refused() {
        let directory = std::env::temp_dir().join(format!("check-safety-empty-{}", std::process::id()));
        for tree in ROOTS {
            let path = directory.join(tree);
            if tree.ends_with(".rs") {
                std::fs::write(&path, "fn main() {}\n").unwrap();
            } else {
                std::fs::create_dir_all(&path).unwrap();
                std::fs::write(path.join("lib.rs"), "fn f() {}\n").unwrap();
            }
        }
        let refused = check(&directory);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(refused.unwrap_err().contains("nothing was read"));
    }
}
