//! A quiet session's history is compressed, and a busy one's is left alone.
//!
//! What compression *is* belongs to the core. What is the broker's is when: after a session has been
//! written to and then left alone, never while a program is still writing, and with nothing changed about
//! what the history holds.

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apack_terminal::housekeeping::Housekeeping;
use apack_terminal::screen::Screen;
use apack_terminal::vt::rows::Space;

/// How long a case waits for the housekeeper to get to a session. A bound, so a broken one fails rather
/// than hangs, and generous, so a busy machine does not.
const PATIENCE: Duration = Duration::from_secs(30);

fn lines(count: usize) -> Vec<u8> {
    (0..count)
        .map(|line| {
            format!("\x1b[3{}mline {line} of some output that is long enough to fill a row\x1b[0m\r\n", line % 8)
        })
        .collect::<String>()
        .into_bytes()
}

#[test]
fn a_session_left_alone_is_compressed_and_holds_the_same_rows() {
    let (compressed, said) = channel();
    let housekeeping = Housekeeping::start_with(Duration::from_millis(50), Some(compressed));
    let screen = Arc::new(Mutex::new(Screen::new(80, 24)));
    let quiet = housekeeping.for_session("s-1", &screen);

    let (before, row) = {
        let mut held = screen.lock().expect("the screen locks");
        held.feed(&lines(30_000));
        (held.resident_bytes(), held.row(Space::Screen, 100).expect("a row reads"))
    };
    quiet.wrote();

    assert_eq!(said.recv_timeout(PATIENCE).as_deref(), Ok("s-1"));
    let held = screen.lock().expect("the screen locks");
    let after = held.resident_bytes();
    assert!(after * 4 < before, "compressing a quiet session freed little: {before} bytes before, {after} after");
    assert_eq!(held.row(Space::Screen, 100).expect("a row reads"), row, "a row changed by being compressed");
}

/// A program that is still writing is not quiet, however long ago it started: its history is left alone
/// until it stops, and compressed then.
#[test]
fn a_session_still_being_written_to_is_left_alone_until_it_stops() {
    let (compressed, said) = channel();
    let housekeeping = Housekeeping::start_with(Duration::from_millis(100), Some(compressed));
    let screen = Arc::new(Mutex::new(Screen::new(80, 24)));
    let quiet = housekeeping.for_session("busy", &screen);

    // Written to for several times the quiet period, a little at a time
    for _ in 0..40 {
        screen.lock().expect("the screen locks").feed(&lines(400));
        quiet.wrote();
        std::thread::sleep(Duration::from_millis(15));
        assert!(said.try_recv().is_err(), "a session was compressed while it was being written to");
    }

    assert_eq!(said.recv_timeout(PATIENCE).as_deref(), Ok("busy"));
}

/// Compressed once, a session that is written to again is compressed again: saying so is not used up.
#[test]
fn a_session_written_to_again_is_compressed_again() {
    let (compressed, said) = channel();
    let housekeeping = Housekeeping::start_with(Duration::from_millis(30), Some(compressed));
    let screen = Arc::new(Mutex::new(Screen::new(80, 24)));
    let quiet = housekeeping.for_session("s-1", &screen);

    for _ in 0..2 {
        screen.lock().expect("the screen locks").feed(&lines(5_000));
        quiet.wrote();
        assert_eq!(said.recv_timeout(PATIENCE).as_deref(), Ok("s-1"));
    }
}

#[test]
fn a_session_that_ended_while_it_waited_is_forgotten() {
    let (compressed, said) = channel();
    let housekeeping = Housekeeping::start_with(Duration::from_millis(30), Some(compressed));
    let gone = Arc::new(Mutex::new(Screen::new(80, 24)));
    let quiet = housekeeping.for_session("gone", &gone);
    quiet.wrote();
    drop(gone);

    // Another session after it is still served, which is what says the thread went on
    let screen = Arc::new(Mutex::new(Screen::new(80, 24)));
    screen.lock().expect("the screen locks").feed(&lines(2_000));
    housekeeping.for_session("alive", &screen).wrote();
    assert_eq!(said.recv_timeout(PATIENCE).as_deref(), Ok("alive"));
}

/// One session's turn is not another's. A session written to a moment ago is left alone for its own quiet
/// period, though the housekeeper is awake and at work on a session beside it.
#[test]
fn a_session_just_written_to_waits_its_own_time_while_another_is_compressed() {
    let quiet_for = Duration::from_millis(300);
    let (compressed, said) = channel();
    let housekeeping = Housekeeping::start_with(quiet_for, Some(compressed));

    // Enough history that compressing it takes the housekeeper more than one look
    let long = Arc::new(Mutex::new(Screen::new(80, 24)));
    long.lock().expect("the screen locks").feed(&lines(30_000));
    housekeeping.for_session("long", &long).wrote();

    // Written to just before the first session's compression begins
    std::thread::sleep(quiet_for * 19 / 10);
    let recent = Arc::new(Mutex::new(Screen::new(80, 24)));
    recent.lock().expect("the screen locks").feed(&lines(2_000));
    let written = std::time::Instant::now();
    housekeeping.for_session("recent", &recent).wrote();

    let mut left_alone_for = None;
    for _ in 0..2 {
        if said.recv_timeout(PATIENCE).as_deref() == Ok("recent") {
            left_alone_for = Some(written.elapsed());
        }
    }
    let left_alone_for = left_alone_for.expect("the session written to last was compressed");
    assert!(left_alone_for >= quiet_for, "compressed {left_alone_for:?} after it was written to");
}
