//! `libghostty-vt` against the parser this broker ships, which is Phase 0a of apack's one-VT-core goal.
//!
//! **The question is whether the two produce the same screen**, because the goal buys one implementation
//! parsing a session's bytes instead of two that can disagree — and that is only worth doing if the one
//! that would replace ours agrees with it first. A difference here is not automatically a defect in either:
//! it is a decision, recorded in the goal's evidence, about which parser is right.
//!
//! Behind the `ghostty` feature, so the default build acquires neither the dependency nor its archive.
//! `cargo xtask vt-prebuilt` prints the two variables a build needs.
#![cfg(feature = "ghostty")]

use apack_terminal::vt::Terminal;

/// The link, which is the first thing Phase 0a has to prove and the cheapest thing to lose.
///
/// Everything else in this suite reads the grid, and a grid read that returns nothing looks the same
/// whether the library is absent, the archive is for another target, or the parse genuinely produced an
/// empty screen. So this case does the three things that cannot be mistaken for each other — construct,
/// write, read back a number the terminal must answer — and does them before any comparison runs.
#[test]
fn the_library_is_linked_and_answers() {
    let mut terminal = Terminal::new(20, 4).expect("a 20x4 terminal");

    assert_eq!(terminal.cols().expect("cols"), 20, "the terminal answers the width it was built with");
    assert_eq!(terminal.rows().expect("rows"), 4, "and the height");

    // A write the parser has to do something with, rather than an empty one it could ignore: text, a CSI
    // cursor move and an SGR reset, which is three different paths through the state machine
    terminal.feed(b"hello\x1b[2;3Hworld\x1b[0m");

    // Still answering afterwards, which is what distinguishes a parse from a crash in the library. The
    // grid itself is read by the cases that compare it; this one is about whether the call works at all.
    assert_eq!(terminal.cols().expect("cols after a write"), 20, "the terminal survived a parse");
}

#[path = "_support/streams.rs"]
mod support;
use apack_terminal::screen::{Screen, Snapshot};
use apack_terminal::vt::GhosttyScreen;
use support::streams;

/// How much history a comparison asks for: the same figure `parser.rs` snapshots with.
const HISTORY: u32 = 20;

/// The grid both implementations produce for one delivery of a stream.
fn both(chunks: &[&[u8]], cols: u16, rows: u16) -> (Snapshot, Snapshot) {
    let mut ours = Screen::new(cols, rows);
    let mut theirs = GhosttyScreen::new(cols, rows).expect("a ghostty terminal");
    for chunk in chunks {
        ours.feed(chunk);
        theirs.feed(chunk);
    }
    (ours.snapshot(HISTORY), theirs.snapshot(HISTORY).expect("a ghostty snapshot"))
}

/// Every way two snapshots differ, named well enough to decide from without rerunning.
fn differences(what: &str, ours: &Snapshot, theirs: &Snapshot) -> Vec<String> {
    let mut found = Vec::new();
    if ours.alt_screen != theirs.alt_screen {
        found.push(format!("{what}: alt_screen ours={} theirs={}", ours.alt_screen, theirs.alt_screen));
    }
    if (ours.cursor_row, ours.cursor_column) != (theirs.cursor_row, theirs.cursor_column) {
        found.push(format!(
            "{what}: cursor ours={},{} theirs={},{}",
            ours.cursor_row, ours.cursor_column, theirs.cursor_row, theirs.cursor_column,
        ));
    }
    if ours.lines.len() != theirs.lines.len() {
        found.push(format!("{what}: rows ours={} theirs={}", ours.lines.len(), theirs.lines.len()));
    }
    for (row, (ours, theirs)) in ours.lines.iter().zip(&theirs.lines).enumerate() {
        if ours != theirs {
            found.push(format!("{what}: row {row} ours={ours:?} theirs={theirs:?}"));
        }
    }
    found
}

/// **Phase 0a's question.** Every difference is reported, not the first: the deliverable is a decided list.
#[test]
fn the_two_parsers_produce_the_same_grid() {
    let mut found = Vec::new();
    for (name, stream) in streams() {
        let (ours, theirs) = both(&[&stream], 20, 4);
        found.extend(differences(name, &ours, &theirs));
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// The same, one byte at a time: where a wrapper that buffers or resets per call stops agreeing.
#[test]
fn the_two_parsers_agree_byte_by_byte() {
    let mut found = Vec::new();
    for (name, stream) in streams() {
        let singles: Vec<&[u8]> = stream.iter().map(std::slice::from_ref).collect();
        let (ours, theirs) = both(&singles, 20, 4);
        found.extend(differences(&format!("{name} (one byte at a time)"), &ours, &theirs));
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// **Not a comparison**: the new wrapper must agree with *itself* however the stream is cut.
///
/// This is the property `parser.rs` was built for, and the one a wrapper breaks on its own. It can fail
/// while every whole-stream grid above matches, so passing those is not a substitute for it.
#[test]
fn ghostty_agrees_with_itself_at_every_boundary() {
    let mut found = Vec::new();
    for (name, stream) in streams() {
        let whole = {
            let mut screen = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
            screen.feed(&stream);
            screen.snapshot(HISTORY).expect("a snapshot")
        };
        for at in 1..stream.len() {
            let mut screen = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
            screen.feed(&stream[..at]);
            screen.feed(&stream[at..]);
            let split = screen.snapshot(HISTORY).expect("a snapshot");
            if split != whole {
                found.extend(differences(&format!("{name} split at {at}"), &whole, &split));
            }
        }
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// **The grids compared above are not empty**, which a comparison of two blank screens cannot tell you.
///
/// This repo's own rule is that a check reporting nothing may have looked at nothing, and two
/// implementations that both answered an empty grid would agree perfectly. So the corpus is asserted to
/// put text on the screen: every stream produces at least one non-blank row, and together they produce
/// more rows than the viewport, which is what proves history is being read rather than skipped.
#[test]
fn the_streams_being_compared_put_something_on_the_screen() {
    let mut drawn = 0;
    let mut rows = 0;
    for (name, stream) in streams() {
        let (_, theirs) = both(&[&stream], 20, 4);
        let text: Vec<&String> = theirs.lines.iter().filter(|line| !line.is_empty()).collect();
        assert!(!text.is_empty(), "{name} drew nothing: {:?}", theirs.lines);
        drawn += text.len();
        rows += theirs.lines.len();
    }
    assert!(drawn >= 8, "the corpus drew {drawn} non-blank rows, which is too few to be comparing anything");
    assert!(rows > 8 * 4, "{rows} rows over eight streams means no history was read past the viewport");
}

/// **The comparison can fail**, proven by planting a difference rather than by trusting that it would.
///
/// Three, one per field the comparison reads, because a reporter that only noticed rows would have passed
/// every case above just as quietly.
#[test]
fn a_planted_difference_is_reported() {
    let text = {
        let (ours, _) = both(&[b"hello"], 20, 4);
        let (mut theirs, _) = both(&[b"hello"], 20, 4);
        theirs.lines[0] = "goodbye".to_owned();
        differences("planted text", &ours, &theirs)
    };
    assert_eq!(text.len(), 1, "a changed row is one difference: {text:?}");
    assert!(text[0].contains("row 0"), "it names the row: {}", text[0]);

    let cursor = {
        let (ours, _) = both(&[b"hello"], 20, 4);
        let (mut theirs, _) = both(&[b"hello"], 20, 4);
        theirs.cursor_column += 1;
        differences("planted cursor", &ours, &theirs)
    };
    assert_eq!(cursor.len(), 1, "a moved cursor is one difference: {cursor:?}");
    assert!(cursor[0].contains("cursor"), "it says so: {}", cursor[0]);

    let screen = {
        let (ours, _) = both(&[b"hello"], 20, 4);
        let (mut theirs, _) = both(&[b"hello"], 20, 4);
        theirs.alt_screen = !theirs.alt_screen;
        differences("planted screen", &ours, &theirs)
    };
    assert_eq!(screen.len(), 1, "the wrong screen is one difference: {screen:?}");
    assert!(screen[0].contains("alt_screen"), "it says so: {}", screen[0]);
}
