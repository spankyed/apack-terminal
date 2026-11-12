//! When a wait that nothing has met is given up on.
//!
//! **One thread for every wait there is, asleep until the next of them falls due.** A wait is settled
//! where the bytes land (`session`), and what is left for a clock is the one that never was: its caller
//! is owed `TIMEOUT` at the moment it asked for. A thread a wait, each sleeping out its own timeout,
//! is as many threads as there are callers waiting — an agent that waits on every command holds one
//! for the whole of each, met or not. Here a wait costs an entry in a list.
//!
//! A wait that was met is dropped from the list the next time the thread looks, and says nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use crate::output::Outgoing;
use crate::protocol::{Event, SessionId, WaitOutcome};

/// Where a wait's deadline is set.
pub struct Deadlines {
    set: Sender<Deadline>,
}

struct Deadline {
    at: Instant,
    session: SessionId,
    /// Whether the wait has been answered, by whichever of its two ends got there first
    settled: Arc<AtomicBool>,
}

impl Deadlines {
    /// Starts the thread that keeps them, which says a wait timed out to `events`. It ends when this
    /// is dropped.
    pub fn keeping(events: Sender<Outgoing>) -> Self {
        let (set, deadlines) = channel();
        std::thread::spawn(move || keep(&deadlines, &events));
        Self { set }
    }

    /// The wait on `session` that `settled` speaks for is given up on `after` from now, unless it has
    /// been met by then.
    pub fn set(&self, after: Duration, session: SessionId, settled: Arc<AtomicBool>) {
        let _ = self.set.send(Deadline { at: Instant::now() + after, session, settled });
    }
}

fn keep(set: &Receiver<Deadline>, events: &Sender<Outgoing>) {
    let mut waiting: Vec<Deadline> = Vec::new();
    loop {
        let arrived = match waiting.iter().map(|deadline| deadline.at).min() {
            // Nothing is waited for: asleep until something is
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
        waiting.extend(arrived);
        let now = Instant::now();
        waiting.retain(|deadline| {
            // Met since it was last looked at: there is nothing to say of it
            if deadline.settled.load(Ordering::SeqCst) {
                return false;
            }
            if !deadline.at.saturating_duration_since(now).is_zero() {
                return true;
            }
            // Whichever end settles a wait first is the one that answers it
            if !deadline.settled.swap(true, Ordering::SeqCst) {
                let timed_out = Event::Waited { session: deadline.session.clone(), outcome: WaitOutcome::Timeout };
                let _ = events.send(Outgoing::answer(timed_out));
            }
            false
        });
    }
}
