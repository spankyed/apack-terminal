//! What a recording's keyframe costs: how long a terminal takes to write out and to put back, and how
//! many bytes it is, over a history far longer than a keyframe holds. Run with
//! `cargo bench --bench keyframe`.
//!
//! It reports and never fails. A measurement is taken on the machine it names and read against that
//! machine's load, which the caller records.
//!
//! **A keyframe is taken every megabyte a program writes** (`recording::KEYFRAME_EVERY`), under the
//! session's lock, so what it costs is added to the parse of that megabyte; and a replay puts one back
//! before it feeds anything. The history is a hundred thousand lines because that is what a session
//! keeps, and a keyframe of it must cost what a thousand lines do.

use std::time::{Duration, Instant};

use apack_terminal::screen::{KEYFRAME_BYTES, KEYFRAME_HISTORY_LINES, Screen, VIEW_HISTORY_LINES};

/// A screen of `columns` by `rows` whose history is `VIEW_HISTORY_LINES` lines of `line`.
fn screen_of(columns: u16, rows: u16, line: &str) -> Screen {
    let mut screen = Screen::new(columns, rows);
    let mut text = String::with_capacity((line.len() + 2) * 1_000);
    for _ in 0..1_000 {
        text.push_str(line);
        text.push_str("\r\n");
    }
    for _ in 0..VIEW_HISTORY_LINES / 1_000 {
        screen.feed(text.as_bytes());
    }
    screen.feed(b"prompt$ ");
    screen
}

/// The fiftieth and ninety-fifth of a hundred, and the most.
fn spread(mut took: Vec<Duration>) -> String {
    took.sort_unstable();
    let at = |hundredth: usize| took[(took.len() * hundredth / 100).min(took.len() - 1)].as_micros();
    format!("p50={}us p95={}us max={}us", at(50), at(95), at(100))
}

fn report(label: &str, columns: u16, rows: u16, line: &str) {
    let screen = screen_of(columns, rows, line);
    let (mut taking, mut putting, mut bytes) = (Vec::new(), Vec::new(), 0);
    for _ in 0..200 {
        let began = Instant::now();
        let keyframe = screen.keyframe().expect("a screen between sequences is written out");
        taking.push(began.elapsed());
        bytes = keyframe.len();
        let entries = [apack_terminal::recording::Timed {
            at: 0,
            entry: apack_terminal::recording::Entry::Keyframe { cols: columns, rows, state: keyframe },
        }];
        let mut replayed = Screen::new(columns, rows);
        let began = Instant::now();
        assert!(replayed.replay(&entries, u64::MAX), "a keyframe is put back");
        putting.push(began.elapsed());
    }
    println!("  {label}, {columns}x{rows}: {bytes} bytes");
    println!("    written out: {}", spread(taking));
    println!("    put back:    {}", spread(putting));
}

fn main() {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    println!(
        "a keyframe of a screen and {KEYFRAME_HISTORY_LINES} lines, of {KEYFRAME_BYTES} bytes at most, over a history of {VIEW_HISTORY_LINES} lines ({cores} cores)"
    );
    report(
        "a build log",
        80,
        24,
        "\x1b[32mwarning\x1b[0m: unused variable in module, see the \x1b[1;31mERROR\x1b[0m above",
    );
    report("a wide screen, full lines", 240, 60, &"wide ".repeat(47));
    report("short lines", 80, 24, "ok");
}
