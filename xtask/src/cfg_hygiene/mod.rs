//! `check-cfg-hygiene`: verifies that mutable `#[cfg(windows)]` / `#[cfg(not(unix))]` logic
//! lives inside `.cargo/windows-only` files rather than hidden in mixed cross-platform files.
//!
//! **Why this matters for mutation testing:**
//! - Linux mutation shards skip `#[cfg(windows)]` / `#[cfg(not(unix))]` regions (not compiled on Linux).
//! - Windows mutation CI (`.github/workflows/mutants.yml`) only mutates files listed in
//!   `.cargo/windows-only` (because mixed files contain `#[cfg(unix)]` arms that Windows does not compile).
//! - Consequently, any Windows-only function, `impl`, inline `mod`, trait default method, or mutable
//!   branch/operator placed outside `.cargo/windows-only` is a silent blind spot for BOTH mutation gates.
//!
//! Thin wiring (such as `mod foo_windows;`, `use`, struct/enum fields, match-arm delegation, or
//! single-call delegation into `*_windows.rs`) is permitted in mixed files; real Windows-only logic
//! must live in `.cargo/windows-only` or be extracted into a cross-platform helper tested everywhere.

mod inspect;
mod lexer;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use inspect::{inspect_windows_construct, skip_attached_construct};
use lexer::{Tok, TokKind, tokenize};

/// Production source trees scanned relative to the repository root.
pub const ROOTS: [&str; 2] = ["src", "crates"];

/// A single `#[cfg(windows)]` / `#[cfg(not(unix))]` hygiene violation outside `.cargo/windows-only`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Repository-relative file path (normalized with `/`).
    pub path: String,
    /// 1-based line number of the offending `#[cfg(...)]` attribute.
    pub line: usize,
    /// Human-readable reason explaining what was flagged.
    pub reason: String,
}

/// Summary of scanning one Rust source file.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Total `#[cfg(...)]` / `#![cfg(...)]` attributes seen in this file.
    pub cfg_attrs_seen: usize,
    /// Violations discovered in this file.
    pub violations: Vec<Violation>,
}

/// Checks every production `.rs` file outside `.cargo/windows-only` for mutable Windows-only logic.
pub fn check(root: &Path) -> Result<(), String> {
    let windows_only: HashSet<String> =
        crate::mutants::windows_only_files(root)?.into_iter().map(|p| normalize_rel_path(Path::new(&p))).collect();

    let mut files = Vec::new();
    for tree in ROOTS {
        let dir = root.join(tree);
        collect_prod_rs_files(root, &dir, &mut files)?;
    }
    files.sort();

    let mut scanned_files = 0usize;
    let mut cfg_attrs_seen = 0usize;
    let mut violations = Vec::new();

    for file in &files {
        let rel = normalize_rel_path(file.strip_prefix(root).unwrap_or(file));
        if windows_only.contains(&rel) {
            continue;
        }
        let source = std::fs::read_to_string(file).map_err(|e| format!("{rel}: {e}"))?;
        let res = scan_source(&rel, &source);
        scanned_files += 1;
        cfg_attrs_seen += res.cfg_attrs_seen;
        violations.extend(res.violations);
    }

    if scanned_files == 0 || cfg_attrs_seen == 0 {
        return Err(format!(
            "check-cfg-hygiene scanned {scanned_files} file(s) and found {cfg_attrs_seen} #[cfg(...)] attribute(s): nothing was read"
        ));
    }

    if violations.is_empty() {
        println!(
            "check-cfg-hygiene: {scanned_files} production files clean ({} allowlisted in .cargo/windows-only)",
            windows_only.len()
        );
        return Ok(());
    }

    let rendered =
        violations.iter().map(|v| format!("  {}:{}: {}", v.path, v.line, v.reason)).collect::<Vec<_>>().join("\n");
    Err(format!(
        "check-cfg-hygiene: found {} mutable #[cfg(windows)] / #[cfg(not(unix))] site(s) outside .cargo/windows-only:\n\
         {rendered}\n\
         Move Windows-only logic into a file listed in .cargo/windows-only (or extract a cross-platform helper).",
        violations.len()
    ))
}

fn normalize_rel_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn is_skipped_path(rel: &str) -> bool {
    if rel.starts_with("crates/apack-sim/") {
        return true;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.last().is_some_and(|&name| name == "tests.rs" || name.ends_with("_tests.rs")) {
        return true;
    }
    parts.iter().any(|&comp| comp == "tests" || comp == "benches")
}

fn collect_prod_rs_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        let rel = normalize_rel_path(path.strip_prefix(root).unwrap_or(&path));
        if is_skipped_path(&rel) {
            continue;
        }
        if path.is_dir() {
            collect_prod_rs_files(root, &path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Attribute & cfg(...) predicate analysis
// ---------------------------------------------------------------------------

struct ParsedAttr<'a> {
    is_inner: bool,
    line: usize,
    inner: &'a [Tok],
    next_idx: usize,
}

fn parse_attr(tokens: &[Tok], idx: usize) -> Option<ParsedAttr<'_>> {
    if !tokens.get(idx)?.is_punct("#") {
        return None;
    }
    let line = tokens[idx].line;
    let (is_inner, bracket_idx) =
        if tokens.get(idx + 1).is_some_and(|t| t.is_punct("!")) { (true, idx + 2) } else { (false, idx + 1) };
    if !tokens.get(bracket_idx).is_some_and(|t| t.is_punct("[")) {
        return None;
    }
    let close_idx = find_matching_delim(tokens, bracket_idx, "[", "]")?;
    Some(ParsedAttr { is_inner, line, inner: &tokens[bracket_idx + 1..close_idx], next_idx: close_idx + 1 })
}

fn find_matching_delim(tokens: &[Tok], open_idx: usize, open: &str, close: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, tok) in tokens.iter().enumerate().skip(open_idx) {
        if tok.is_punct(open) {
            depth += 1;
        } else if tok.is_punct(close) {
            depth -= 1;
            if depth == 0 {
                return Some(idx);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CfgKind {
    NotCfg,
    Test,
    WindowsOnly,
    Other,
}

fn classify_attr(inner: &[Tok]) -> CfgKind {
    if inner.len() == 1 && inner[0].is_ident("test") {
        return CfgKind::Test;
    }
    if inner.len() >= 3
        && inner[0].is_ident("cfg")
        && inner[1].is_punct("(")
        && let Some(close) = find_matching_delim(inner, 1, "(", ")")
    {
        let pred = &inner[2..close];
        if pred_mentions_active_test(pred) {
            return CfgKind::Test;
        }
        if pred_is_windows_only(pred) {
            return CfgKind::WindowsOnly;
        }
        return CfgKind::Other;
    }
    CfgKind::NotCfg
}

fn split_top_level_commas(tokens: &[Tok]) -> Vec<&[Tok]> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.is_punct("(") || tok.is_punct("[") || tok.is_punct("{") {
            depth += 1;
        } else if tok.is_punct(")") || tok.is_punct("]") || tok.is_punct("}") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && tok.is_punct(",") {
            parts.push(&tokens[start..i]);
            start = i + 1;
        }
    }
    if start < tokens.len() {
        parts.push(&tokens[start..]);
    }
    parts
}

fn unwrap_call<'a>(tokens: &'a [Tok], name: &str) -> Option<&'a [Tok]> {
    if tokens.len() >= 3
        && tokens[0].is_ident(name)
        && tokens[1].is_punct("(")
        && find_matching_delim(tokens, 1, "(", ")") == Some(tokens.len() - 1)
    {
        Some(&tokens[2..tokens.len() - 1])
    } else {
        None
    }
}

fn pred_mentions_active_test(pred: &[Tok]) -> bool {
    if pred.len() == 1 && pred[0].is_ident("test") {
        return true;
    }
    if unwrap_call(pred, "not").is_some() {
        return false;
    }
    if let Some(args) = unwrap_call(pred, "all").or_else(|| unwrap_call(pred, "any")) {
        return split_top_level_commas(args).into_iter().any(pred_mentions_active_test);
    }
    false
}

/// Returns true when `pred` always evaluates to true on Unix/Linux (so `not(pred)` never compiles on Unix).
fn pred_holds_on_unix(pred: &[Tok]) -> bool {
    if pred.len() == 1 && pred[0].is_ident("unix") {
        return true;
    }
    if pred.len() == 3
        && pred[0].is_ident("target_family")
        && pred[1].is_punct("=")
        && matches!(&pred[2].kind, TokKind::StringLit(s) if s == "unix")
    {
        return true;
    }
    if let Some(args) = unwrap_call(pred, "any") {
        return split_top_level_commas(args).into_iter().any(pred_holds_on_unix);
    }
    false
}

fn pred_is_windows_only(pred: &[Tok]) -> bool {
    if pred.len() == 1 && pred[0].is_ident("windows") {
        return true;
    }
    if pred.len() == 3
        && (pred[0].is_ident("target_os") || pred[0].is_ident("target_family"))
        && pred[1].is_punct("=")
        && matches!(&pred[2].kind, TokKind::StringLit(s) if s == "windows")
    {
        return true;
    }
    if let Some(inner) = unwrap_call(pred, "not") {
        return pred_holds_on_unix(inner);
    }
    if let Some(args) = unwrap_call(pred, "all") {
        return split_top_level_commas(args).into_iter().any(pred_is_windows_only);
    }
    if let Some(args) = unwrap_call(pred, "any") {
        let parts = split_top_level_commas(args);
        return !parts.is_empty() && parts.into_iter().all(pred_is_windows_only);
    }
    false
}

/// Advances from inside an enclosing `{ ... }` block to just past its closing `}`.
fn skip_to_enclosing_brace_end(tokens: &[Tok], start: usize) -> usize {
    let mut depth = 1usize;
    let mut i = start;
    while i < tokens.len() {
        if tokens[i].is_punct("{") {
            depth += 1;
        } else if tokens[i].is_punct("}") {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    tokens.len()
}

/// Scans a single Rust source file for mutable `#[cfg(windows)]` / `#[cfg(not(unix))]` sites.
pub fn scan_source(rel_path: &str, source: &str) -> ScanResult {
    let tokens = tokenize(source);
    let mut result = ScanResult::default();
    let mut i = 0usize;
    let mut brace_depth = 0usize;

    while i < tokens.len() {
        if tokens[i].is_punct("{") {
            brace_depth += 1;
            i += 1;
            continue;
        }
        if tokens[i].is_punct("}") {
            brace_depth = brace_depth.saturating_sub(1);
            i += 1;
            continue;
        }

        let Some(attr) = parse_attr(&tokens, i) else {
            i += 1;
            continue;
        };

        let kind = classify_attr(attr.inner);
        if kind != CfgKind::NotCfg && !matches!(attr.inner, [t] if t.is_ident("test")) {
            result.cfg_attrs_seen += 1;
        }

        if attr.is_inner {
            match kind {
                CfgKind::Test => {
                    if brace_depth == 0 {
                        return result;
                    }
                    i = skip_to_enclosing_brace_end(&tokens, attr.next_idx);
                    brace_depth -= 1;
                    continue;
                }
                CfgKind::WindowsOnly => {
                    result.violations.push(Violation {
                        path: rel_path.to_owned(),
                        line: attr.line,
                        reason: "Windows-only inner attribute `#![cfg(...)]` outside .cargo/windows-only".to_owned(),
                    });
                    if brace_depth == 0 {
                        return result;
                    }
                    i = skip_to_enclosing_brace_end(&tokens, attr.next_idx);
                    brace_depth -= 1;
                    continue;
                }
                CfgKind::NotCfg | CfgKind::Other => {
                    i = attr.next_idx;
                    continue;
                }
            }
        }

        // Collect consecutive stacked outer attributes starting at `i`.
        let mut is_test_group = kind == CfgKind::Test;
        let mut win_cfg_line = if kind == CfgKind::WindowsOnly { Some(attr.line) } else { None };
        let mut cursor = attr.next_idx;

        while let Some(next_attr) = parse_attr(&tokens, cursor) {
            if next_attr.is_inner {
                break;
            }
            let next_kind = classify_attr(next_attr.inner);
            if next_kind != CfgKind::NotCfg && !matches!(next_attr.inner, [t] if t.is_ident("test")) {
                result.cfg_attrs_seen += 1;
            }
            if next_kind == CfgKind::Test {
                is_test_group = true;
            } else if next_kind == CfgKind::WindowsOnly && win_cfg_line.is_none() {
                win_cfg_line = Some(next_attr.line);
            }
            cursor = next_attr.next_idx;
        }

        if is_test_group {
            i = skip_attached_construct(&tokens, cursor);
            continue;
        }

        let Some(cfg_line) = win_cfg_line else {
            i = cursor;
            continue;
        };

        let end = skip_attached_construct(&tokens, cursor);
        if let Some(reason) = inspect_windows_construct(&tokens[cursor..end]) {
            result.violations.push(Violation { path: rel_path.to_owned(), line: cfg_line, reason });
        }
        i = end;
    }

    result
}
