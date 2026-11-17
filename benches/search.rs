//! What a view's search costs over a full history, measured on the screen a session holds: how long one
//! `Screen::find` takes, which is how long the session's lock is held and its program's output waits. Run
//! with `cargo bench --bench search`.
//!
//! It reports and never fails. A measurement is taken on the machine it names and read against that
//! machine's load, which the caller records.
//!
//! **Each case is the worst a needle can ask of a history that long.** A history of one letter in one case
//! gives the core a match in every cell, and a search for the other case keeps none of them, so nothing
//! lets it stop early.

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use apack_terminal::screen::{Screen, VIEW_HISTORY_LINES};
use apack_terminal::view::publish::ViewId;

const COLUMNS: u16 = 80;
const ROWS: u16 = 24;

/// A screen whose history is `lines` full rows of `line`, with a view attached so its lines have numbers.
fn screen_of(lines: usize, line: &str) -> (Screen, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut screen = Screen::with_history(COLUMNS, ROWS, VIEW_HISTORY_LINES);
    let mut text = String::with_capacity((line.len() + 2) * 1_000);
    for _ in 0..1_000 {
        text.push_str(line);
        text.push_str("\r\n");
    }
    for _ in 0..lines / 1_000 {
        screen.feed(text.as_bytes());
    }
    let (sink, incoming) = channel();
    screen.attach_view(ViewId { connection: 1, channel: 1 }, sink).expect("a view attaches");
    (screen, incoming)
}

fn timed(screen: &mut Screen, needle: &str, case_sensitive: bool) -> (Duration, usize, bool) {
    let began = Instant::now();
    let found = screen.find(needle, case_sensitive).expect("a search runs");
    (began.elapsed(), found.spans.len(), found.more)
}

fn report(label: &str, screen: &mut Screen, needle: &str, case_sensitive: bool) {
    let (took, found, more) = timed(screen, needle, case_sensitive);
    let more = if more { ", and may be more" } else { "" };
    println!("  {label}: {:>9.1} ms, {found} matches answered{more}", took.as_secs_f64() * 1_000.0);
}

fn main() {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    println!("search over a history of {VIEW_HISTORY_LINES} lines of {COLUMNS} columns ({cores} cores)");

    // A build log: every line says the word in another case, and one old line says it exactly
    let (mut log, _view) = screen_of(VIEW_HISTORY_LINES, "warning: Error in module, see the ERROR above for the cause");
    println!("a log whose every line has the word twice, in other cases");
    report("either case", &mut log, "error", false);
    report("exact case, none exact", &mut log, "error", true);
    // What every search of a needle that is not there costs, in either case: the whole history looked through
    report("a word that is not there", &mut log, "absent", false);
    drop(log);

    // The worst a needle can ask: a match in every cell, none of them exact
    let (mut dense, _view) = screen_of(VIEW_HISTORY_LINES, &"A".repeat(usize::from(COLUMNS) - 1));
    println!("a history of one letter, a match in every cell");
    report("either case", &mut dense, "a", false);
    report("exact case, none exact", &mut dense, "a", true);
    report("exact case, all exact", &mut dense, "A", true);
}
