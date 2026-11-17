//! A keyframe for a case to replay, of a screen that was fed what the case says.
//!
//! **Taken from a screen, as a recording's are.** A keyframe is the core's account of a terminal and is not
//! something a case can write out by hand, so one is made the way a session makes one: a screen of that
//! size, fed, and written out.
//!
//! Not a test target: cargo builds the files directly under `tests/`, and a subdirectory is only reachable
//! through the `#[path]` the suite's root uses.
#![allow(dead_code)]

use apack_terminal::recording::Entry;
use apack_terminal::screen::Screen;

/// The keyframe of a `cols` by `rows` screen that was fed `drawn`.
pub fn keyframe(cols: u16, rows: u16, drawn: &[u8]) -> Entry {
    let mut screen = Screen::new(cols, rows);
    screen.feed(drawn);
    keyframe_of(&screen, cols, rows)
}

/// The keyframe of `screen` as it stands, which is `cols` by `rows`.
pub fn keyframe_of(screen: &Screen, cols: u16, rows: u16) -> Entry {
    Entry::Keyframe { cols, rows, state: screen.keyframe().expect("a screen between sequences is written out") }
}
