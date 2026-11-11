//! The deadlines kept for screens a program is holding: one for a screen that is held, however many holds
//! it has had, and none for a screen whose hold is over or whose session has gone.
//!
//! What a view is sent when a hold runs out is `view_frames.rs`'s. These are about what the broker keeps
//! meanwhile, which no view sees.

use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use apack_terminal::holds::{HOLD_LIMIT, Holds};
use apack_terminal::screen::Screen;
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::wire::{ToView, body};

/// How long a deadline that should have been let go of is waited for. **Shorter than the runner's deadline
/// for a case** (`.config/nextest.toml`), so a count that never settles fails saying what it stood at.
const PATIENCE: Duration = Duration::from_secs(5);

/// A screen whose holds `holds` ends, and what its one view is sent. The view has drawn the screen it was
/// sent on attaching, so the next thing it is sent is the screen as a hold's end left it.
fn watched(holds: &Holds) -> (Arc<Mutex<Screen>>, Receiver<Vec<u8>>) {
    let mut screen = Screen::new(20, 4);
    let (sink, incoming) = channel();
    let id = ViewId { connection: 1, channel: 1 };
    screen.attach_view(id, sink).expect("a view attaches");
    let first = match incoming.try_recv().map(|encoded| ToView::decode(body(&encoded))) {
        Ok(Ok(ToView::Frame(frame))) => frame,
        other => panic!("expected the screen a view is sent on attaching, found {other:?}"),
    };
    screen.acknowledge(id, first.sequence).expect("an acknowledgement is taken");
    let screen = Arc::new(Mutex::new(screen));
    screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
    (screen, incoming)
}

/// Waits for `holds` to be keeping `count` deadlines. A deadline is let go of by the thread that ends
/// holds, around the moment the screen is drawn and not at one this thread can name, so the count is
/// waited for rather than read once.
fn settles_at(holds: &Holds, count: usize) {
    let waiting_since = Instant::now();
    while holds.outstanding() != count {
        assert!(
            waiting_since.elapsed() < PATIENCE,
            "{} deadlines are kept, for {count} screens being held",
            holds.outstanding()
        );
        std::thread::yield_now();
    }
}

/// **A deadline a screen that is held, and that is all.** A screen taking a hold is one more; the same
/// screen letting go and taking it again, as often as it likes, is not; and once every hold has run out
/// and its screen been drawn there are none.
#[test]
fn one_deadline_is_kept_for_each_screen_held_and_none_once_each_has_been_drawn() {
    let holds = Holds::start();
    let (restless, _restless_view) = watched(&holds);
    let (still, still_view) = watched(&holds);
    assert_eq!(holds.outstanding(), 0, "no screen is held");
    {
        // **Each screen is kept locked while it is counted.** A hold is ended on its screen, so the
        // deadline of one that is locked is still there however long this thread takes to read the count
        let mut restless_screen = restless.lock().expect("the screen");
        restless_screen.feed(b"\x1b[?2026hrestless");
        assert_eq!(holds.outstanding(), 1, "one screen is held");
        for _ in 0..500 {
            restless_screen.feed(b"\x1b[?2026l\x1b[?2026h");
        }
        assert_eq!(holds.outstanding(), 1, "one screen is held, which has by now had five hundred holds");

        let mut still_screen = still.lock().expect("the screen");
        still_screen.feed(b"\x1b[?2026hstill");
        assert_eq!(holds.outstanding(), 2, "two screens are held");
    }
    // The hold taken last is the last to run out
    still_view.recv_timeout(HOLD_LIMIT * 30).expect("the hold is ended for the program");
    settles_at(&holds, 0);
}

/// **A session that ends holding its screen leaves nothing behind it.** Its hold has nothing left to
/// draw, and its deadline goes when it comes round rather than being kept for a screen that is not there.
#[test]
fn a_session_that_ends_while_holding_its_screen_leaves_no_deadline_behind() {
    let holds = Holds::start();
    let (ended, _ended_view) = watched(&holds);
    {
        let mut screen = ended.lock().expect("the screen");
        screen.feed(b"\x1b[?2026hnever let go of");
        assert_eq!(holds.outstanding(), 1, "one screen is held");
    }
    drop(ended);

    // A hold another session takes afterwards runs out after the one that was left, so its screen being
    // drawn is when the other's deadline has come and gone
    let (other, other_view) = watched(&holds);
    other.lock().expect("the screen").feed(b"\x1b[?2026hother");
    other_view.recv_timeout(HOLD_LIMIT * 30).expect("the other session's hold is ended");
    settles_at(&holds, 0);
}
