//! Ending a hold a program never lets go of.
//!
//! A program that asks for the screen to be held while it redraws (mode 2026) is drawn when it lets go,
//! which the terminal learns where the bytes that say so are parsed (`Screen::feed`). **One that asks and
//! then says nothing more is the case with no bytes to learn from**: nothing arrives, so nothing that
//! runs when bytes arrive can notice that the hold has gone on too long. That is the one moment in a
//! view's life that is a point in time rather than an event, and this is what wakes for it.
//!
//! **One thread for every session, asleep unless a screen is being held.** A hold beginning says when it
//! must be over; this waits for the earliest of those and nothing else. With no hold anywhere there is no
//! timer at all — the housekeeping thread's arrangement (`housekeeping`), for the same reason.
//!
//! **One deadline a screen, however many holds it has had.** Only a screen's newest hold can be the one
//! in force, so that is the only one with a clock, kept where the screen and this thread both read it
//! (`Clock`). A program taking and letting go of the screen as fast as it can write moves that one clock
//! forward and tells this thread nothing: the thread hears from a screen once, when it had no deadline
//! outstanding, and next when that deadline has come and gone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, TryLockError, Weak};
use std::time::{Duration, Instant};

use crate::screen::Screen;

/// The longest a program may hold the screen. One that asks and never lets go is drawn anyway.
pub const HOLD_LIMIT: Duration = Duration::from_secs(1);

/// How long a hold whose screen is busy at its deadline is left before it is looked at again.
///
/// **One hold's wait, not a poll.** The thread takes a screen's lock to end a hold on it, and a screen
/// can be busy for a while — a search of a long history, a recording replayed. Waiting for it would make
/// every other session's hold wait too, so a busy screen is passed over and its hold comes round again
/// this much later; a hold nothing is in the way of is ended at its deadline.
const BUSY_RETRY: Duration = Duration::from_millis(20);

/// When a screen's newest hold must be over, and whether the thread has been told the screen has one.
struct Clock {
    /// Which of the screen's holds this is the end of, so one that has since been let go is left alone
    hold: u64,
    at: Instant,
    /// The thread is waiting on this clock: it reads `at` when it wakes, and need not be told again
    outstanding: bool,
}

/// One screen's deadline, as the thread holds it.
struct Deadline {
    screen: Weak<Mutex<Screen>>,
    clock: Arc<Mutex<Clock>>,
}

/// The thread that ends holds, and the way a screen is given its say in it.
#[derive(Clone)]
pub struct Holds {
    deadlines: Sender<Deadline>,
    outstanding: Arc<AtomicUsize>,
}

impl Holds {
    /// Starts the thread. It ends when every handle to it has gone.
    pub fn start() -> Self {
        let (deadlines, set) = channel();
        let outstanding = Arc::new(AtomicUsize::new(0));
        std::thread::spawn({
            let outstanding = Arc::clone(&outstanding);
            move || keep(&set, &outstanding)
        });
        Self { deadlines, outstanding }
    }

    /// What a screen holds to say when a hold of its must be over (`Screen::limit_holds`).
    pub fn for_screen(&self, screen: Weak<Mutex<Screen>>) -> HoldLimit {
        HoldLimit {
            deadlines: self.deadlines.clone(),
            outstanding: Arc::clone(&self.outstanding),
            screen,
            clock: Arc::new(Mutex::new(Clock { hold: 0, at: Instant::now(), outstanding: false })),
        }
    }

    /// How many screens have a deadline the thread is waiting on or has yet to hear of: never more than
    /// there are screens being held, whatever their programs do.
    pub fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::SeqCst)
    }
}

/// A screen's way of saying a hold has begun.
pub struct HoldLimit {
    deadlines: Sender<Deadline>,
    outstanding: Arc<AtomicUsize>,
    screen: Weak<Mutex<Screen>>,
    clock: Arc<Mutex<Clock>>,
}

impl HoldLimit {
    /// The hold numbered `hold` began now, and is over `HOLD_LIMIT` from now whether or not the program
    /// ends it. The thread is told only when it is not already waiting on this screen.
    pub fn began(&self, hold: u64) {
        let Ok(mut clock) = self.clock.lock() else { return };
        clock.hold = hold;
        clock.at = Instant::now() + HOLD_LIMIT;
        if !clock.outstanding {
            clock.outstanding = true;
            self.outstanding.fetch_add(1, Ordering::SeqCst);
            let _ = self.deadlines.send(Deadline { screen: self.screen.clone(), clock: Arc::clone(&self.clock) });
        }
    }
}

fn keep(set: &Receiver<Deadline>, outstanding: &AtomicUsize) {
    // One a screen at most, and when each is next to be looked at
    let mut waiting: Vec<(Instant, Deadline)> = Vec::new();
    loop {
        let arrived = match waiting.iter().map(|(look, _)| *look).min() {
            // Nothing is held: asleep until something is
            None => match set.recv() {
                Ok(deadline) => Some(deadline),
                Err(_) => return,
            },
            Some(next) => match set.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(deadline) => Some(deadline),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        if let Some(deadline) = arrived {
            let look = deadline.clock.lock().map_or_else(|_| Instant::now(), |clock| clock.at);
            waiting.push((look, deadline));
        }
        // **Whatever is due is dealt with on every turn**, a turn that took an arrival included: screens
        // arriving one after another must not be what keeps a hold that has run out from being ended
        let now = Instant::now();
        waiting.retain_mut(|(look, deadline)| {
            if *look > now {
                return true;
            }
            let Ok(mut clock) = deadline.clock.lock() else { return forget(outstanding) };
            // A newer hold moved the clock on since this was last looked at
            if clock.at > now {
                *look = clock.at;
                return true;
            }
            // A session that has ended has nothing left to draw
            let Some(screen) = deadline.screen.upgrade() else {
                clock.outstanding = false;
                return forget(outstanding);
            };
            match screen.try_lock() {
                Ok(mut screen) => {
                    // Before the screen is told: a hold that begins as this one is ended finds no
                    // deadline outstanding, and asks for one
                    clock.outstanding = false;
                    let hold = clock.hold;
                    drop(clock);
                    screen.end_hold(hold);
                    forget(outstanding)
                }
                Err(TryLockError::WouldBlock) => {
                    *look = now + BUSY_RETRY;
                    true
                }
                Err(TryLockError::Poisoned(_)) => {
                    clock.outstanding = false;
                    forget(outstanding)
                }
            }
        });
    }
}

/// One fewer deadline is outstanding. Answers `false`, which is what dropping it from the list is.
fn forget(outstanding: &AtomicUsize) -> bool {
    outstanding.fetch_sub(1, Ordering::SeqCst);
    false
}
