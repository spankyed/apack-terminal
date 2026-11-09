//! `check-exclusions` watched failing, which is the only thing that says it can.
//!
//! The decision it makes — which patterns exclude nothing — is driven here with counts of its own, because
//! the alternative is committing a dead pattern to `.cargo/mutants.toml` to watch the gate fire and then
//! remembering to take it out again.

use xtask::mutants::{exclusions, verdict};

/// Counts by a table, so a case says exactly which patterns match what.
fn counting<'a>(table: &'a [(&'a str, usize)]) -> impl Fn(&str) -> Result<usize, String> + 'a {
    move |pattern: &str| Ok(table.iter().find(|(name, _)| *name == pattern).map(|(_, count)| *count).unwrap_or(0))
}

#[test]
fn an_exclusion_that_matches_nothing_is_named() {
    let patterns = vec!["lives.rs:.*a".to_owned(), "dead.rs:.*b".to_owned()];
    let error = verdict(&patterns, counting(&[(".", 40), ("lives.rs:.*a", 1)]))
        .expect_err("a pattern matching nothing fails the check");
    assert!(error.contains("dead.rs:.*b"), "the dead pattern is named: {error}");
    assert!(!error.contains("lives.rs:.*a"), "the live one is not: {error}");
    assert!(error.contains("1 of 2"), "and how many of how many: {error}");
}

#[test]
fn exclusions_that_all_match_pass() {
    let patterns = vec!["a.rs:.*one".to_owned()];
    verdict(&patterns, counting(&[(".", 40), ("a.rs:.*one", 3)])).expect("every exclusion excludes something");
}

/// **A run that listed no mutants would make every pattern look dead**, which is the shape of a check that
/// reports nothing because it looked at nothing. It refuses instead.
#[test]
fn no_mutants_at_all_is_a_refusal_rather_than_a_pass() {
    let patterns = vec!["a.rs:.*one".to_owned()];
    let error = verdict(&patterns, counting(&[])).expect_err("an empty population is not a pass");
    assert!(error.contains("looked at nothing"), "{error}");
}

/// The file's own escaping: a TOML basic string's `\\` is one backslash by the time cargo-mutants sees it,
/// and writing one was what stopped the file parsing at all.
#[test]
fn a_patterns_backslashes_are_unescaped_as_toml_does_it() {
    let config = "
exclude_re = [
    # A comment, and a blank line, are not patterns
    \"screen.rs:.*replace \\\\| with \\\\^ in row_text\",

    \"main.rs:.*replace detach with \\\\(\\\\)\",
]
";
    assert_eq!(
        exclusions(config).expect("the block parses"),
        vec![
            r"screen.rs:.*replace \| with \^ in row_text".to_owned(),
            r"main.rs:.*replace detach with \(\)".to_owned(),
        ]
    );
}

/// Anything in the block that is neither a comment nor a quoted pattern is a refusal: a scan that skipped
/// what it did not recognise could check nothing and say so in the words of checking everything.
#[test]
fn something_that_is_not_a_pattern_refuses_rather_than_being_skipped() {
    let error = exclusions("exclude_re = [\n    skip_calls = 1,\n]\n").expect_err("that is not a pattern");
    assert!(error.contains("neither a comment nor a quoted pattern"), "{error}");
}

/// And the real file is the subject the check exists for, so it has to be readable by this parser.
#[test]
fn the_repositorys_own_exclusions_parse() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("the workspace root");
    let config = std::fs::read_to_string(root.join(".cargo/mutants.toml")).expect("the config is readable");
    let patterns = exclusions(&config).expect("the repository's own block parses");
    assert!(patterns.len() >= 5, "every exclusion is read: {patterns:?}");
    assert!(patterns.iter().all(|p| !p.contains("\\\\")), "no pattern is left double-escaped: {patterns:?}");
}
