//! When a wait that nothing has met is given up on.
//!
//! **One thread for every wait there is, asleep until the next of them falls due.** A wait is settled
//! where the bytes land (`session`), and what is left for a clock is the one that never was: its caller
//! is owed `TIMEOUT` at the moment it asked for. A thread a wait, each sleeping out its own timeout,
//! is as many threads as there are callers waiting — an agent that waits on every command holds one
//! for the whole of each, met or not. Here a wait costs an entry in a list.
//!
//! A wait that was met is dropped from the list the next time the thread looks at it, and says nothing.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
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
    /// been met by then. A duration so large it overflows `Instant` cannot elapse and is ignored.
    pub fn set(&self, after: Duration, session: SessionId, settled: Arc<AtomicBool>) {
        let Some(at) = Instant::now().checked_add(after) else {
            return;
        };
        let _ = self.set.send(Deadline { at, session, settled });
    }
}

/// Minimum number of tracked entries before sweeping already-settled waits out of the heap.
const PRUNE_THRESHOLD: usize = 256;

fn keep(set: &Receiver<Deadline>, events: &Sender<Outgoing>) {
    // Ordered by when each falls due, earliest first; `seq` breaks a tie so that two due at the same
    // instant are both given up on. The deadlines themselves are kept beside it, by `seq`
    let mut waiting: BinaryHeap<Reverse<(Instant, u64)>> = BinaryHeap::new();
    let mut pending: HashMap<u64, Deadline> = HashMap::new();
    let mut seq = 0_u64;
    let mut next_prune = PRUNE_THRESHOLD;
    loop {
        // Everything due is dealt with before anything is waited for again
        let now = Instant::now();
        while let Some(Reverse((at, next))) = waiting.peek().copied() {
            let Some(deadline) = pending.remove(&next) else {
                waiting.pop();
                continue;
            };
            // Met since it was set: there is nothing to say of it, and nothing to wait for either
            if deadline.settled.load(Ordering::SeqCst) {
                waiting.pop();
                continue;
            }
            if at > now {
                pending.insert(next, deadline);
                break;
            }
            waiting.pop();
            // Whichever end settles a wait first is the one that answers it
            if !deadline.settled.swap(true, Ordering::SeqCst) {
                let timed_out = Event::Waited { session: deadline.session, outcome: WaitOutcome::Timeout };
                let _ = events.send(Outgoing::answer(timed_out));
            }
        }
        let arrived = match waiting.peek() {
            // Nothing is waited for: asleep until something is
            None => match set.recv() {
                Ok(deadline) => Some(deadline),
                Err(_) => return,
            },
            Some(Reverse((at, _))) => match set.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(deadline) => Some(deadline),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        if let Some(deadline) = arrived {
            seq += 1;
            waiting.push(Reverse((deadline.at, seq)));
            pending.insert(seq, deadline);
            prune_if_needed(&mut waiting, &mut pending, &mut next_prune);
        }
    }
}

fn prune_if_needed(
    waiting: &mut BinaryHeap<Reverse<(Instant, u64)>>,
    pending: &mut HashMap<u64, Deadline>,
    next_prune: &mut usize,
) {
    if pending.len() >= *next_prune {
        pending.retain(|_, entry| !entry.settled.load(Ordering::Relaxed));
        waiting.retain(|Reverse((_, id))| pending.contains_key(id));
        *next_prune = pending.len().saturating_mul(2).max(PRUNE_THRESHOLD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_if_needed_runs_at_threshold_and_keeps_unsettled_entries() {
        let mut waiting = BinaryHeap::new();
        let mut pending = HashMap::new();
        let mut next_prune = PRUNE_THRESHOLD;
        let now = Instant::now();

        for seq in 1..PRUNE_THRESHOLD as u64 {
            waiting.push(Reverse((now, seq)));
            pending.insert(
                seq,
                Deadline { at: now, session: format!("s-{seq}"), settled: Arc::new(AtomicBool::new(true)) },
            );
        }
        // Below threshold: nothing pruned yet.
        prune_if_needed(&mut waiting, &mut pending, &mut next_prune);
        assert_eq!(pending.len(), PRUNE_THRESHOLD - 1);
        assert_eq!(waiting.len(), PRUNE_THRESHOLD - 1);

        // One more live entry reaches PRUNE_THRESHOLD: settled entries drop, live entry stays.
        let live_seq = PRUNE_THRESHOLD as u64;
        waiting.push(Reverse((now, live_seq)));
        pending.insert(
            live_seq,
            Deadline { at: now, session: "live".to_owned(), settled: Arc::new(AtomicBool::new(false)) },
        );
        prune_if_needed(&mut waiting, &mut pending, &mut next_prune);
        assert_eq!(pending.len(), 1);
        assert_eq!(waiting.len(), 1);
        assert!(pending.contains_key(&live_seq));
        assert_eq!(next_prune, PRUNE_THRESHOLD);
    }
}
