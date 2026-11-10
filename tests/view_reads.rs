//! What a view asks about the text on screen, by line number.
//!
//! The rules — what a word is, where a wrapped line ends — are the core's. What is the broker's is the
//! numbering: a view names lines that may be far up the history, or gone, and gets its answer in the same
//! numbers.

use apack_terminal::screen::Screen;
use apack_terminal::view::wire::{Position, Span};

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
    assert_eq!(screen.word_at(at(0, 8)).expect("a word reads").1, Some(span((0, 6), (0, 10))), "build");
    assert_eq!(screen.word_at(at(0, 0)).expect("a word reads").1, Some(span((0, 0), (0, 4))), "cargo");
    assert_eq!(screen.word_at(at(0, 30)).expect("a word reads").1, None, "past the text");
    assert_eq!(screen.word_at(at(900, 0)).expect("a word reads").1, None, "a line that does not exist");
}

/// A line longer than the screen is wide is drawn over several rows and is still one line: asked for at
/// any of them, it is all of them.
#[test]
fn the_line_at_a_cell_runs_across_the_rows_it_wrapped_onto() {
    let mut screen = Screen::new(10, 5);
    screen.feed(b"short\r\nabcdefghijklmnopqrstuvwxyz\r\nlast");
    let whole = Some(span((1, 0), (3, 5)));
    assert_eq!(screen.line_at(at(1, 3)).expect("a line reads").1, whole);
    assert_eq!(screen.line_at(at(2, 9)).expect("a line reads").1, whole);
    assert_eq!(screen.line_at(at(3, 0)).expect("a line reads").1, whole);
    assert_eq!(screen.line_at(at(0, 0)).expect("a line reads").1, Some(span((0, 0), (0, 4))));
}

#[test]
fn the_text_of_a_span_is_what_was_written_with_wrapped_rows_joined() {
    let mut screen = Screen::new(10, 6);
    screen.feed(b"one   \r\nabcdefghijklmnopqrstuvwxyz\r\nthree");
    assert_eq!(screen.text_of(span((0, 0), (4, 4))).expect("text reads"), "one\nabcdefghijklmnopqrstuvwxyz\nthree");
    assert_eq!(screen.text_of(span((1, 2), (2, 1))).expect("text reads"), "cdefghijkl", "from within a line");
    assert_eq!(screen.text_of(span((1, 0), (900, 0))).expect("text reads"), "", "an end that is not there");
}

/// **By line number, however far the screen has moved.** The text is read from history at the place the
/// line is now, which is not where it was when the view was told about it.
#[test]
fn a_line_keeps_its_number_as_it_scrolls_up_the_history() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"needle in line zero\r\n");
    let before = screen.word_at(at(0, 0)).expect("a word reads");

    let more: String = (1..40).map(|line| format!("line {line}\r\n")).collect();
    screen.feed(more.as_bytes());
    let after = screen.word_at(at(0, 0)).expect("a word reads");
    assert_eq!(before, after, "the same answer, in the same epoch");
    assert_eq!(screen.text_of(span((0, 0), (0, 18))).expect("text reads"), "needle in line zero");
    assert_eq!(screen.text_of(span((38, 0), (39, 6))).expect("text reads"), "line 38\nline 39");
}

#[test]
fn a_search_answers_every_match_newest_first_in_either_case() {
    let mut screen = Screen::new(20, 3);
    let written: String = (0..8).map(|line| format!("row {line} Needle\r\n")).collect();
    screen.feed(written.as_bytes());

    let (epoch, found) = screen.find("needle").expect("a search runs");
    assert_eq!(epoch, screen.view_position().expect("a position reads").0);
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
    let (_, found) = screen.find("needle").expect("a search runs");
    assert_eq!(found, vec![span((0, 7), (1, 2))]);
}
