//! The screen's **reads**, driven directly: what a caller gets back once bytes have been parsed.
//!
//! `parser.rs` covers what a byte stream becomes. This covers the questions asked of the grid afterwards —
//! the replies the terminal owes the program in its pty, how much scrollback it keeps, and where a search
//! finds its text. None of them needs a pty or a socket, which is why `screen` is a library module.

use apack_terminal::screen::{Colors, HISTORY_LINES, SEARCH_MATCHES, Screen};

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
/// **The promise holds for a session that has written far more than it, which is where it first broke.**
///
/// The core's line limit is page-granular and its floor is the ask less one page, so a session asked for
/// exactly `HISTORY_LINES` settles *below* it — measured at 441 rows against a promised 1,000 after 20,000
/// lines at 80 columns. A window is told that figure in the broker's handshake so it never draws lines a
/// reattach cannot reproduce, so the floor is the half that matters and the half a short case misses:
/// feeding barely past the bound leaves one page still unpruned and passes either way.
#[test]
fn a_long_running_session_still_answers_the_scrollback_it_promised() {
    let rows = 24;
    let mut screen = Screen::new(80, rows);
    for line in 0..20_000 {
        screen.feed(format!("line {line} of a session that has been running a while\r\n").as_bytes());
    }

    let asked = screen.snapshot(u32::MAX);
    assert_eq!(
        asked.lines.len(),
        HISTORY_LINES + usize::from(rows),
        "a session that has written twenty times its scrollback still answers all of it, and no more",
    );
    // The oldest line retained is not the oldest ever written: what scrolled past the bound is gone
    assert!(
        !asked.lines[0].contains("line 0 "),
        "the first line ever written is past the bound and must not still be here: {:?}",
        asked.lines[0],
    );
}

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

/// What the terminal said back to the program, as one string.
fn answered(screen: &mut Screen, asked: &[u8]) -> String {
    screen.feed(asked);
    screen.take_replies().concat()
}

/// Where the cursor is after `written`, as the terminal reports it to a program that asks.
fn column_after(written: &str) -> String {
    let mut screen = Screen::new(80, 24);
    screen.feed(written.as_bytes());
    answered(&mut screen, b"\x1b[6n")
}

/// **An emoji made of several code points is one character here, because it is one where it is drawn.**
///
/// The window's emulator clusters by default and this screen did not, so after a family emoji the cursor
/// this screen reported was four columns to the right of where the window drew it — and the position a
/// program is told comes from here. Each of these is two cells wide, so the cursor ends in column three.
#[test]
fn a_character_of_several_code_points_takes_the_cells_of_one() {
    for (what, written) in [
        ("a family joined by zero-width joiners", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
        ("a flag", "\u{1F1EF}\u{1F1F5}"),
        ("a thumb with a skin tone", "\u{1F44D}\u{1F3FD}"),
        ("a heart with an emoji selector", "\u{2764}\u{FE0F}"),
    ] {
        assert_eq!(column_after(written), "\u{1b}[1;3R", "{what} is one two-cell character");
    }
}

/// And a program that asks whether clustering is on is told it is, so what it lays out matches.
#[test]
fn a_program_asking_about_clustering_is_told_it_is_on() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(answered(&mut screen, b"\x1b[?2027$p"), "\u{1b}[?2027;1$y");
}

/// **A reset does not turn it back off.** `reset` is something a user types, and a default set only for
/// now would leave the terminal disagreeing with its window from then on.
#[test]
fn clustering_survives_a_reset() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1bc");
    screen.feed("\u{1F1EF}\u{1F1F5}".as_bytes());
    assert_eq!(answered(&mut screen, b"\x1b[6n"), "\u{1b}[1;3R");
}

/// **A program asking whether it may send images is not told yes.** Nothing that draws a session draws
/// one, so an `OK` here has the program write image data a user sees as nothing, or as noise.
#[test]
fn a_program_asking_to_send_images_is_not_told_it_may() {
    let mut screen = Screen::new(80, 24);
    let reply = answered(&mut screen, b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    assert!(!reply.contains("OK"), "the image protocol was offered: {reply:?}");
}

/// **A program asking for the terminal's colours hears the ones the session is drawn in.**
///
/// That is how a program picks a light or a dark theme. The broker answers the question and draws
/// nothing, so before it was told it answered with a palette nobody was looking at.
#[test]
fn a_program_asking_for_a_colour_is_told_the_one_on_the_screen() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(answered(&mut screen, b"\x1b]11;?\x1b\\"), "", "with no colours given there is none to report");

    screen.set_colors(&Colors {
        foreground: [0xd4, 0xd4, 0xd4],
        background: [0x1e, 0x1e, 0x1e],
        cursor: None,
        ansi: vec![[0, 0, 0], [0xcd, 0x31, 0x31]],
    });

    assert!(answered(&mut screen, b"\x1b]11;?\x1b\\").contains("rgb:1e1e/1e1e/1e1e"), "the background");
    assert!(answered(&mut screen, b"\x1b]10;?\x1b\\").contains("rgb:d4d4/d4d4/d4d4"), "the foreground");
    assert!(answered(&mut screen, b"\x1b]4;1;?\x1b\\").contains("rgb:cdcd/3131/3131"), "palette entry one");
    // An entry the theme did not name keeps the terminal's own rather than becoming black
    assert!(!answered(&mut screen, b"\x1b]4;2;?\x1b\\").contains("rgb:0000/0000/0000"), "entry two is untouched");
}

/// **A size in pixels is reported once a window has measured a cell, and not before.** A reply of zero
/// pixels is a wrong answer where silence is an honest one.
#[test]
fn a_program_asking_for_the_size_in_pixels_is_told_once_a_cell_has_been_measured() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(answered(&mut screen, b"\x1b[16t"), "", "no cell measured, so nothing to report");

    screen.resize(100, 30, 9, 18);
    assert_eq!(answered(&mut screen, b"\x1b[16t"), "\u{1b}[6;18;9t", "one cell, height then width");
    assert_eq!(answered(&mut screen, b"\x1b[14t"), "\u{1b}[4;540;900t", "the whole grid, height then width");

    // A later resize from a caller that measures nothing keeps the cell the window gave
    screen.resize(50, 10, 0, 0);
    assert_eq!(answered(&mut screen, b"\x1b[14t"), "\u{1b}[4;180;450t");
}

/// **A line longer than the grid is one line, and is found whole.** Drawn it is two rows; read as text it
/// has to be what was written, or a path that reached the right edge is two strings and a search for it
/// finds neither.
#[test]
fn a_line_that_wrapped_is_read_and_searched_as_one_line() {
    let mut screen = Screen::new(10, 4);
    screen.feed(b"0123456789abcdef\r\nnext\r\n");

    let read = screen.snapshot(0);
    assert_eq!(&read.lines[..3], ["0123456789", "abcdef", "next"]);
    assert_eq!(&read.wrapped[..3], [true, false, false], "only the first row runs on");

    let found = screen.search("89ab", 0);
    assert_eq!(found.len(), 1, "the match spans the wrap: {found:?}");
    assert_eq!((found[0].line, found[0].column), (0, 8), "it starts on the first row, at its ninth character");
    assert_eq!(found[0].text, "0123456789abcdef", "and the line comes back joined");
}

/// A space at the end of a full row belongs to the line, so the words either side of the wrap stay apart.
#[test]
fn a_space_at_a_wrap_is_kept() {
    let mut screen = Screen::new(10, 4);
    screen.feed(b"012345678 abc\r\n");

    assert_eq!(screen.search("8 a", 0).len(), 1, "the space the row ended on is part of the line");
}

/// **A character is read whole.** A cell holds one character and a character is often several code points;
/// reading the first alone gives text nobody wrote.
#[test]
fn a_character_of_several_code_points_is_read_whole() {
    let mut screen = Screen::new(40, 4);
    screen.feed("cafe\u{301} \u{1F44D}\u{1F3FD} ok\r\n".as_bytes());

    assert_eq!(screen.snapshot(0).lines[0], "cafe\u{301} \u{1F44D}\u{1F3FD} ok");
    assert_eq!(screen.search("e\u{301}", 0).len(), 1, "the accent is there to be matched");
}

/// **A wait looks only where a read could have put its text**, and still finds a match that begins above
/// that: the rows are few, and a wrapped line is read from its start.
#[test]
fn text_is_found_in_the_rows_a_read_added_and_not_above_them() {
    let mut screen = Screen::new(10, 3);
    screen.feed(b"needle\r\n");
    for line in 0..20 {
        screen.feed(format!("line {line}\r\n").as_bytes());
    }
    assert!(!screen.shows_recently("needle", 2), "twenty rows up is outside the two rows asked for");
    assert!(screen.shows_recently("needle", 30), "and inside thirty");

    // The match starts on a row above the ones asked for, on a line that wrapped into them
    let mut screen = Screen::new(10, 2);
    screen.feed(b"abcdefghijKLMNOPQRSTuvw\r\nx\r\ny\r\n");
    assert!(screen.shows_recently("jKLM", 2), "the line is read from its start, above the rows asked for");
    assert!(!screen.shows_recently("", 2), "and nothing is not a match");
}

/// **The rows a read can have scrolled off are bounded by what was in it**: a line feed each, a wrap for
/// every grid-width of bytes, and the row the cursor was on. A wait scans that many and no more, so the
/// bound has to be a ceiling — a short one makes a wait miss text that is there.
#[test]
fn the_rows_a_read_could_add_are_counted_from_its_bytes() {
    use apack_terminal::session::rows_added;

    assert_eq!(rows_added(b"", 80), 1, "the row the cursor is on");
    assert_eq!(rows_added(b"a\nb\nc\n", 80), 4, "one per line feed");
    assert_eq!(rows_added(&[b'x'; 200], 80), 3, "two wraps in two hundred characters at eighty columns");
    assert_eq!(rows_added(&[b'\n'; 5], 0), 11, "a grid of no width is read as one column");
}

/// **A cell is known only when both of its sides are**, and a resize that knows neither changes nothing a
/// program was told. Half a cell is not a size.
#[test]
fn a_cell_with_a_side_missing_is_not_a_cell() {
    let mut screen = Screen::new(80, 24);
    // Neither given, on a screen that has never been told: still nothing to report
    screen.resize(80, 24, 0, 0);
    assert_eq!(answered(&mut screen, b"\x1b[16t"), "", "a cell of nothing was reported as a size");

    screen.resize(80, 24, 9, 18);
    for (width, height) in [(0, 20), (10, 0)] {
        screen.resize(80, 24, width, height);
        assert_eq!(answered(&mut screen, b"\x1b[16t"), "\u{1b}[6;18;9t", "half a cell replaced a whole one");
    }
}

/// The width a caller bounds a read by is the grid's, and follows a resize.
#[test]
fn the_width_is_the_grids() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(screen.cols(), 80);
    screen.resize(132, 24, 0, 0);
    assert_eq!(screen.cols(), 132);
}

/// **A wrapped line is read from its start however far above the rows asked for that is**, and with real
/// history above it: the row before the ones asked for is the one that says whether to reach further.
#[test]
fn a_wrapped_line_is_read_from_its_start_with_history_above_it() {
    let mut screen = Screen::new(10, 2);
    for line in 0..12 {
        screen.feed(format!("old {line}\r\n").as_bytes());
    }
    // Four rows of one line, then two short lines that push all but its tail above the viewport
    screen.feed(b"aaaaaaaaaaBBBBBBBBBBccccccccccDDDD\r\ny\r\n");

    assert!(screen.shows_recently("aaBB", 1), "the line's first rows are above the one row asked for");
    assert!(screen.shows_recently("ccDD", 1));
    assert!(!screen.shows_recently("old 3", 1), "and nothing above the line's start is read");
}
