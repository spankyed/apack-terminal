//! What it costs a session's output that a person has said whether the cursor blinks: the parse then
//! watches every sequence that sets private modes (`vt::Terminal::watched`), which means finding each escape
//! in what a program writes. Run with `cargo bench --bench blink`.
//!
//! It reports and never fails. A measurement is taken on the machine it names and read against that
//! machine's load, which the caller records.
//!
//! **The output is the kind that has the most escapes in it**: text coloured a word at a time, links and
//! titles whose strings the parse stops at the end of, and a full-screen program's redraw, which sets
//! private modes as it goes. Plain text has none, and is parsed in
//! one piece either way.

use std::time::Instant;

use apack_terminal::protocol::{CursorBlink, CursorStyle};
use apack_terminal::screen::Screen;

const BYTES: usize = 32 << 20;

/// `written` over and over, to `BYTES` of it.
fn output(written: &str) -> Vec<u8> {
    written.as_bytes().iter().copied().cycle().take(BYTES).collect()
}

/// Megabytes a second through a screen whose blink is `blink`, fed as a pty is read: 64 KiB at a time.
fn rate(bytes: &[u8], blink: CursorBlink) -> f64 {
    let mut screen = Screen::new(120, 40);
    screen.set_cursor_defaults(CursorStyle::Bar, blink).expect("the defaults are set");
    let began = Instant::now();
    for read in bytes.chunks(64 << 10) {
        screen.feed(read);
    }
    (bytes.len() as f64 / f64::from(1 << 20)) / began.elapsed().as_secs_f64()
}

/// What each kind of output is made of, by name.
const OUTPUTS: [(&str, &str); 4] = [
    ("plain text", "the quick brown fox jumps over the lazy dog\r\n"),
    ("a colour a word", "\x1b[31mthe\x1b[0m \x1b[32mquick\x1b[0m \x1b[1;34mbrown\x1b[0m \x1b[38;5;208mfox\x1b[0m\r\n"),
    // A string ends at every word, and the parse stops at each end: what is left is looked through from there
    ("a link a word", "\x1b]8;;https://example.com/a\x1b\\word\x1b]8;;\x1b\\ and \x1b]0;a title\x07\r\n"),
    ("a redraw", "\x1b[?25l\x1b[H\x1b[2K\x1b[7mstatus\x1b[0m\x1b[5;10Hrow\x1b[?25h\x1b[?12l\x1b[?12h"),
];

fn report(label: &str, written: &str) {
    let bytes = output(written);
    // The first pass of each warms what the second measures
    let (_, _) = (rate(&bytes, CursorBlink::Program), rate(&bytes, CursorBlink::On));
    let (left, chosen) = (rate(&bytes, CursorBlink::Program), rate(&bytes, CursorBlink::On));
    println!(
        "  {label}: {left:>7.1} MB/s left to the program, {chosen:>7.1} MB/s chosen ({:+.1}%)",
        (chosen / left - 1.0) * 100.0
    );
}

/// With no arguments, every kind of output both ways, timed. **A time is the machine's as much as the
/// code's**, so one kind parsed one way can be run alone for whatever counts the work instead:
/// `/usr/bin/time -l target/release/deps/blink-* <kind> <program|on>`, and read the instructions retired.
fn main() {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let arguments: Vec<String> = std::env::args().skip(1).filter(|argument| !argument.starts_with("--")).collect();
    if let [kind, blink] = arguments.as_slice() {
        let (label, written) = OUTPUTS[kind.parse::<usize>().expect("a kind of output, by its place in the list")];
        let blink = if blink == "on" { CursorBlink::On } else { CursorBlink::Program };
        println!("{label}, {blink:?}: {:.1} MB/s", rate(&output(written), blink));
        return;
    }
    println!("{} MiB of output through a 120x40 screen ({cores} cores)", BYTES >> 20);
    for (label, written) in OUTPUTS {
        report(label, written);
    }
}
