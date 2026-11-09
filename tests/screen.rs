//! The screen's **reads**, driven directly: what a caller gets back once bytes have been parsed.
//!
//! `parser.rs` covers what a byte stream becomes. This covers the questions asked of the grid afterwards —
//! the replies the terminal owes the program in its pty, how much scrollback it keeps, and where a search
//! finds its text. None of them needs a pty or a socket, which is why `screen` is a library module.

use apack_terminal::screen::{HISTORY_LINES, SEARCH_MATCHES, Screen};

/// **The terminal answers for itself, and the answer has to leave the screen.**
///
/// A program that asks where the cursor is (DSR 6) waits for the reply — `vim` and `less` both ask as they
/// start — so a screen that parses the query and drops its answer is a session that hangs with nothing on
/// it. The replies are collected while the bytes are fed and handed to whoever writes to the pty, which is
/// also why taking them has to empty them: answering the same query twice writes stray input to the shell.
#[test]
fn a_query_the_terminal_answers_itself_is_collected_for_the_pty() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[6n");

    let replies = screen.take_replies();
    assert_eq!(replies.len(), 1, "one query, one answer: {replies:?}");
    assert!(replies[0].starts_with('\u{1b}'), "an escape sequence: {:?}", replies[0]);
    assert!(replies[0].ends_with('R'), "a cursor position report: {:?}", replies[0]);
    assert!(screen.take_replies().is_empty(), "and taking them leaves none to send twice");
}

/// **The scrollback is `HISTORY_LINES` and no more**, which is the grid's capacity rather than a cap applied
/// to a read.
///
/// A session nobody is drawing still parses everything its program writes, so what the grid will hold is
/// the whole of the broker's memory per session — and the `Dimensions` this screen reports is the only place
/// that is decided. A caller asking for more history than exists is answered with what there is.
#[test]
fn a_session_keeps_its_declared_scrollback_and_not_more() {
    let rows = 4;
    let mut screen = Screen::new(20, rows);
    // Past the bound on purpose: the oldest lines have to be gone, or nothing bounds a long-running session
    for line in 0..(HISTORY_LINES + usize::from(rows) + 50) {
        screen.feed(format!("line {line}\r\n").as_bytes());
    }

    let asked = screen.snapshot(u32::MAX);
    assert!(
        asked.lines.len() <= HISTORY_LINES + usize::from(rows),
        "a session holds its scrollback and the viewport, not whatever it has written: {} lines",
        asked.lines.len()
    );
    assert!(
        asked.lines.len() >= HISTORY_LINES,
        "and it does hold that much, or a caller reading back loses what scrolled off: {} lines",
        asked.lines.len()
    );
    assert!(!asked.lines.iter().any(|line| line == "line 0"), "the oldest line is gone");
}

/// **Every place the text appears on a line, with the column counted in characters.**
///
/// A caller matching on a screen counts the columns that were drawn, so a multi-byte character is one of
/// them: a byte offset would put a match in the wrong place on any line with a non-ASCII character to its
/// left, which for an agent reading a terminal is most of them.
#[test]
fn a_search_answers_every_place_on_a_line_in_characters() {
    let mut screen = Screen::new(40, 4);
    screen.feed("ab ab\r\n".as_bytes());
    screen.feed("日本語 ab\r\n".as_bytes());

    let plain = screen.search("ab", 0);
    assert_eq!(
        plain.iter().map(|found| (found.line, found.column)).collect::<Vec<_>>(),
        vec![(0, 0), (0, 3), (1, 4)],
        "both places on the first line, and past three wide characters on the second"
    );
}

/// **A search answers at most its cap**, because the question is where something is and not every place it
/// occurs: a one-character search over a full history would otherwise answer tens of thousands of
/// positions, each carrying its whole line.
#[test]
fn a_search_answers_at_most_its_cap() {
    let mut screen = Screen::new(80, 24);
    for _ in 0..(SEARCH_MATCHES + 20) {
        screen.feed(b"x\r\n");
    }

    let found = screen.search("x", u32::MAX);
    assert_eq!(found.len(), SEARCH_MATCHES, "the cap decides, not how many there were");
}

/// An empty search is not a match on every position; it is not a search.
#[test]
fn a_search_for_nothing_finds_nothing() {
    let mut screen = Screen::new(40, 4);
    screen.feed(b"something\r\n");

    assert!(screen.search("", 0).is_empty());
}
