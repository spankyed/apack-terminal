//! The synchronized-output hold (mode 2026), measured in the broker: what a full-screen repaint costs
//! between the program's `2026h` and `2026l`, and how long a hold that is never let go of lasts before the
//! broker ends it. Run with `cargo bench --bench hold`.
//!
//! It reports and never fails. The first number is parse-and-publish cost on this machine, not how long a
//! real program holds the screen: a program sets the length of its own hold. The second is the one the
//! broker's timer bounds, and should be `HOLD_LIMIT`.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use apack_terminal::holds::{HOLD_LIMIT, Holds};
use apack_terminal::screen::Screen;
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::wire::{ToView, body};

const COLS: u16 = 200;
const ROWS: u16 = 50;
const RUNS: usize = 200;
const HOLD_RUNS: usize = 5;

/// A screen with one view attached and up to date, so every publish has a view to send to.
fn viewed(screen: &mut Screen) -> Receiver<Vec<u8>> {
    let (sink, incoming) = channel();
    let id = ViewId { connection: 1, channel: 1 };
    screen.attach_view(id, sink).expect("a view attaches");
    let first = match incoming.try_recv().map(|encoded| ToView::decode(body(&encoded))) {
        Ok(Ok(ToView::Frame(frame))) => frame,
        other => panic!("expected the screen a view is sent on attaching, found {other:?}"),
    };
    screen.acknowledge(id, first.sequence).expect("an acknowledgement is taken");
    incoming
}

/// A full repaint: every row rewritten in colour, inside a hold that is let go of at the end.
fn repaint() -> String {
    let mut frame = String::from("\x1b[?2026h");
    for row in 0..ROWS {
        frame.push_str(&format!("\x1b[{};1H", row + 1));
        for chunk in 0..COLS / 10 {
            frame.push_str(&format!("\x1b[{}m{}", 31 + (row + chunk) % 7, "abcdefghij"));
        }
    }
    frame.push_str("\x1b[0m\x1b[?2026l");
    frame
}

/// The `p`th percentile of sorted samples, in microseconds.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index]
}

fn report(name: &str, mut samples: Vec<f64>) {
    samples.sort_by(f64::total_cmp);
    println!(
        "{name}: n={} p50={:.0}us p95={:.0}us max={:.0}us",
        samples.len(),
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        samples.last().copied().unwrap_or(0.0),
    );
}

fn main() {
    let frame = repaint();
    let mut hold_end = Vec::with_capacity(RUNS);
    let mut fetch = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let mut screen = Screen::new(COLS, ROWS);
        let view = viewed(&mut screen);
        let (epoch, _) = screen.view_position().expect("a position reads");

        let begun = Instant::now();
        screen.feed(frame.as_bytes());
        let ended = begun.elapsed();
        let sent = view.try_iter().count();
        assert!(sent > 0, "the released hold was published to the view");

        let asked = Instant::now();
        screen.fetch(epoch, 0, u32::from(ROWS)).expect("a fetch reads");
        fetch.push(asked.elapsed().as_secs_f64() * 1e6);
        hold_end.push(ended.as_secs_f64() * 1e6);
    }
    report("hold begin to release, feed and publish", hold_end);
    report("view fetch of the whole screen after release", fetch);

    // A program that asks for the screen and never lets go: the broker's limit is what ends it
    let holds = Holds::start();
    let mut limit = Vec::with_capacity(HOLD_RUNS);
    for _ in 0..HOLD_RUNS {
        let mut screen = Screen::new(COLS, ROWS);
        let view = viewed(&mut screen);
        let screen = Arc::new(Mutex::new(screen));
        screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
        let begun = Instant::now();
        screen.lock().expect("the screen").feed(b"\x1b[?2026hstuck");
        view.recv_timeout(HOLD_LIMIT * 30).expect("the hold is ended by the broker");
        limit.push(begun.elapsed().as_secs_f64() * 1e6);
    }
    report(&format!("never released hold, ended by the broker (HOLD_LIMIT {} us)", HOLD_LIMIT.as_micros()), limit);
}
