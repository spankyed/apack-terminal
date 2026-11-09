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
