//! What a view asks about the text on screen, by line number.
//!
//! The rules — what a word is, where a wrapped line ends — are the core's. What is the broker's is the
//! numbering: a view names lines that may be far up the history, or gone, and gets its answer in the same
//! numbers.

use std::sync::mpsc::{Receiver, channel};

use apack_terminal::screen::Screen;
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::wire::{Position, Span};
use apack_terminal::vt::reads::SEARCH_MATCHES;

/// Attaches a view, which is what gives a session's lines their numbers, and answers the epoch those
/// numbers are in. The receiver is the view's end, kept so the view stays attached.
fn viewed(screen: &mut Screen) -> (u32, Receiver<Vec<u8>>) {
    let (sink, incoming) = channel();
    screen.attach_view(ViewId { connection: 1, channel: 1 }, sink).expect("a view attaches");
    (screen.view_position().expect("a position reads").0, incoming)
}

fn at(line: u64, column: u16) -> Position {
    Position { line, column }
}

fn span(start: (u64, u16), end: (u64, u16)) -> Span {
    Span { start: at(start.0, start.1), end: at(end.0, end.1) }
}

#[test]
fn the_word_at_a_cell_is_the_cells_the_word_covers() {
    let mut screen = Screen::new(40, 4);
    screen.feed(b"cargo build --release\r\n");
    let (epoch, _view) = viewed(&mut screen);
    assert_eq!(screen.word_at(epoch, at(0, 8)).expect("a word reads"), Some(span((0, 6), (0, 10))), "build");
    assert_eq!(screen.word_at(epoch, at(0, 0)).expect("a word reads"), Some(span((0, 0), (0, 4))), "cargo");
    assert_eq!(screen.word_at(epoch, at(0, 30)).expect("a word reads"), None, "past the text");
    assert_eq!(screen.word_at(epoch, at(900, 0)).expect("a word reads"), None, "a line that does not exist");
}

/// A line longer than the screen is wide is drawn over several rows and is still one line: asked for at
/// any of them, it is all of them.
#[test]
fn the_line_at_a_cell_runs_across_the_rows_it_wrapped_onto() {
    let mut screen = Screen::new(10, 5);
    screen.feed(b"short\r\nabcdefghijklmnopqrstuvwxyz\r\nlast");
    let (epoch, _view) = viewed(&mut screen);
    let whole = Some(span((1, 0), (3, 5)));
    assert_eq!(screen.line_at(epoch, at(1, 3)).expect("a line reads"), whole);
    assert_eq!(screen.line_at(epoch, at(2, 9)).expect("a line reads"), whole);
    assert_eq!(screen.line_at(epoch, at(3, 0)).expect("a line reads"), whole);
    assert_eq!(screen.line_at(epoch, at(0, 0)).expect("a line reads"), Some(span((0, 0), (0, 4))));
}

#[test]
fn the_text_of_a_span_is_what_was_written_with_wrapped_rows_joined() {
    let mut screen = Screen::new(10, 6);
    screen.feed(b"one   \r\nabcdefghijklmnopqrstuvwxyz\r\nthree");
    let (epoch, _view) = viewed(&mut screen);
    assert_eq!(
        screen.text_of(epoch, span((0, 0), (4, 4))).expect("text reads"),
        "one\nabcdefghijklmnopqrstuvwxyz\nthree"
    );
    assert_eq!(screen.text_of(epoch, span((1, 2), (2, 1))).expect("text reads"), "cdefghijkl", "from within a line");
    assert_eq!(screen.text_of(epoch, span((1, 0), (900, 0))).expect("text reads"), "", "an end that is not there");
}

/// **By line number, however far the screen has moved.** The text is read from history at the place the
/// line is now, which is not where it was when the view was told about it.
#[test]
fn a_line_keeps_its_number_as_it_scrolls_up_the_history() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"needle in line zero\r\n");
    let (epoch, _view) = viewed(&mut screen);
    let before = screen.word_at(epoch, at(0, 0)).expect("a word reads");

    let more: String = (1..40).map(|line| format!("line {line}\r\n")).collect();
    screen.feed(more.as_bytes());
    let after = screen.word_at(epoch, at(0, 0)).expect("a word reads");
    assert_eq!(before, after, "the same answer, in the same epoch");
    assert_eq!(screen.text_of(epoch, span((0, 0), (0, 18))).expect("text reads"), "needle in line zero");
    assert_eq!(screen.text_of(epoch, span((38, 0), (39, 6))).expect("text reads"), "line 38\nline 39");
}

#[test]
fn a_search_answers_every_match_newest_first_in_either_case() {
    let mut screen = Screen::new(20, 3);
    let written: String = (0..8).map(|line| format!("row {line} Needle\r\n")).collect();
    screen.feed(written.as_bytes());
    let (viewed_in, _view) = viewed(&mut screen);

    let (epoch, found) = screen.find("needle").expect("a search runs");
    assert_eq!(epoch, viewed_in);
    assert_eq!(found.len(), 8);
    assert_eq!(found[0], span((7, 6), (7, 11)), "the newest first");
    assert_eq!(found[7], span((0, 6), (0, 11)), "and the oldest, from history, last");
    assert!(screen.find("absent").expect("a search runs").1.is_empty());
    assert!(screen.find("").expect("a search runs").1.is_empty());
}

/// A match can run across the end of a row, where the line wrapped.
#[test]
fn a_search_finds_text_that_wrapped() {
    let mut screen = Screen::new(10, 4);
    screen.feed(b"0123456needle");
    let _view = viewed(&mut screen);
    let (_, found) = screen.find("needle").expect("a search runs");
    assert_eq!(found, vec![span((0, 7), (1, 2))]);
}

/// **A search goes on to the far end of the history.** The core is handed the history a part at a time,
/// newest first, and a match is found whichever part it is in: one on the oldest of thousands of lines,
/// with nothing like it in between, is answered after the one on the screen.
#[test]
fn a_search_finds_a_match_on_the_oldest_line_of_a_long_history() {
    let mut screen = Screen::with_history(40, 4, 20_000);
    screen.feed(b"the needle, first\r\n");
    let between: String = (1..8_000).map(|line| format!("line {line}\r\n")).collect();
    screen.feed(between.as_bytes());
    screen.feed(b"and a needle, last");
    let _view = viewed(&mut screen);

    let (_, found) = screen.find("needle").expect("a search runs");
    assert_eq!(found, vec![span((8_000, 6), (8_000, 11)), span((0, 4), (0, 9))]);
}

/// **A search that would match everywhere stops when it has what it answers with.** One letter looked for
/// in tens of thousands of lines of it is answered with the newest matches there are room for — the ones
/// nearest the screen, in order — and not by finding every one and keeping a few.
#[test]
fn a_search_matching_everywhere_answers_the_newest_and_no_more() {
    let mut screen = Screen::with_history(40, 4, 50_000);
    let line = format!("{}\r\n", "a".repeat(39));
    for _ in 0..40_000 {
        screen.feed(line.as_bytes());
    }
    let _view = viewed(&mut screen);
    let (_, first_active) = screen.view_position().expect("a position reads");

    let (_, found) = screen.find("a").expect("a search runs");
    assert_eq!(found.len(), SEARCH_MATCHES);
    // The newest is the last letter written, on the row above the cursor's, and they run back from there
    assert_eq!(found[0], span((first_active + 2, 38), (first_active + 2, 38)));
    assert!(
        found.is_sorted_by(
            |newer, older| (newer.start.line, newer.start.column) > (older.start.line, older.start.column)
        )
    );
    // Two thousand matches of thirty-nine a line are the last fifty-two lines, of the forty thousand
    let oldest = found.last().expect("a match").start.line;
    assert_eq!(first_active + 2 - oldest, (SEARCH_MATCHES as u64 - 1) / 39);
}
