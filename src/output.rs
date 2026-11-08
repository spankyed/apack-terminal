//! The presentation channel: bounded, coalesced, and with capacity an answer can always use.
//!
//! **Why a terminal needs more than one class.** A shell can write faster than a window can draw, so output
//! has to be bounded or the broker buffers without limit. But the app also *asks* a terminal questions — a
//! DA or DSR query, whose answer the terminal itself produces — and an answer that cannot leave because
//! output filled the channel is a deadlock: the app waits for something the terminal already wrote. So
//! **output spends credit and answers do not**, which is `Outgoing::answer` carrying `credit: None`.
//!
//! **That split is guarded by its structure, not by a case, and the attempt to find one is why.** Three
//! mutations that gave answers output's credit were measured *passing* every behavioural case written for
//! them — a saturating producer with an idle reader, a lowered bound, a request during saturation. The
//! reason is this design: the queue is unbounded and only output spends credit, so an answer is never
//! waiting on anything and there is no state in which it is. A shared **bounded queue** is what deadlocks,
//! and this cannot be turned into one by a change small enough for a mutation to model. So the case to
//! watch for is a future change that bounds the queue itself: that is the edit this comment exists to stop,
//! and `Outgoing::answer` returning credit would be its first step.
//!
//! **The window and the cap are readings, not preferences.** apack's Phase G measured 64 KiB of terminal
//! output arriving as 115 messages over 1,389 ms — about 12 ms between chunks, because the producer was a
//! pipe writing small pieces. The window is one frame at 60 Hz, which is the next value up from that
//! interval and the shortest one a window could act on; the cap is what a fast producer fills before the
//! window elapses, so saturation flushes on size rather than waiting.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::Event;

/// One frame at 60 Hz, the next value up from the 12 ms inter-chunk interval Phase G measured
pub const OUTPUT_WINDOW: Duration = Duration::from_millis(16);
/// What a fast producer fills before the window elapses: flush on size rather than hold it
pub const OUTPUT_CAP: usize = 64 * 1024;
/// How many coalesced output messages may be in flight. Phase G's ceiling, in messages
pub const OUTPUT_CREDIT: usize = 64;

/// Credit for the output class: a producer takes one per message and the writer returns it.
///
/// A plain bounded channel would do this for one producer, but the writer has to drain *two* classes and
/// `std::sync::mpsc` has no select — so the bound lives here, at the producer, and both classes share one
/// queue. An answer takes no credit, which is the whole point: it is never behind output.
pub struct Credit {
    free: Mutex<usize>,
    returned: Condvar,
}

impl Credit {
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self { free: Mutex::new(total), returned: Condvar::new() })
    }

    /// Waits for credit, answering how long it had to. A non-zero wait **is** the overload, observably.
    ///
    /// **Timed around the condvar and not around the lock.** It used to start the clock before acquiring the
    /// mutex, so taking an uncontended lock — a few hundred nanoseconds — counted as waiting for credit, and
    /// every single flush reported itself throttled. Driving `less` in the app put a line in the log for each
    /// one, at a level that reads as an error, saying `waited 0ms`: a report firing constantly while its own
    /// number said nothing had happened. What is being measured is time spent with no credit, which is
    /// exactly the time inside `wait`.
    fn take(&self) -> Duration {
        let mut free = self.free.lock().expect("credit is not poisoned");
        let mut waited = Duration::ZERO;
        while *free == 0 {
            let waiting = Instant::now();
            free = self.returned.wait(free).expect("credit is not poisoned");
            waited += waiting.elapsed();
        }
        *free -= 1;
        waited
    }

    fn give(&self) {
        let mut free = self.free.lock().expect("credit is not poisoned");
        *free += 1;
        self.returned.notify_one();
    }
}

/// What the writer takes off the queue: an event, and the credit to return once it is written.
pub struct Outgoing {
    pub event: Event,
    credit: Option<Arc<Credit>>,
}

impl Outgoing {
    /// An answer: no credit, so it is never held behind output
    pub fn answer(event: Event) -> Self {
        Self { event, credit: None }
    }

    /// Returns the credit this message spent, once it has actually been written
    pub fn written(self) {
        if let Some(credit) = self.credit {
            credit.give();
        }
    }
}

/// Where to cut a buffer so the part sent holds no half-arrived character.
///
/// **A pty read is bytes, and a character can straddle two of them.** `read` returns whatever the kernel
/// had, so a multi-byte sequence is split whenever the boundary falls inside it — and a flush between the
/// two reads used to convert each half on its own, replacing both with U+FFFD. `é` written as `a\xC3` then
/// `\xA9b` arrived as `a..b`. Every TUI's box drawing, all CJK and every emoji were exposed; ASCII never
/// was, which is why it went unnoticed.
///
/// So a trailing sequence that has not all arrived stays in the buffer for the next read. Only a trailing
/// one: bytes that are invalid rather than incomplete can never become valid by waiting, so they are sent
/// and replaced as before. A sequence is at most four bytes, so at most three are ever held back.
fn sendable(bytes: &[u8]) -> usize {
    // Only the last three bytes can begin a sequence that is still arriving
    for at in (bytes.len().saturating_sub(3)..bytes.len()).rev() {
        let byte = bytes[at];
        // A continuation byte: its lead is further back, so keep looking
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needed = match byte {
            0x00..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // Not a lead byte at all (0xC0, 0xC1, 0xF5..): invalid, and waiting cannot mend it
            _ => 1,
        };
        return if bytes.len() - at < needed { at } else { bytes.len() };
    }
    bytes.len()
}

/// Coalesces one session's output and sends it under credit.
///
/// Runs as its own thread because the reader's `read` blocks: a time-based flush needs something that can
/// wait *with a deadline*, which `recv_timeout` is and a blocking read is not.
pub fn coalesce(session: String, chunks: Receiver<Vec<u8>>, outgoing: Sender<Outgoing>, credit: Arc<Credit>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut opened = Instant::now();

    // `ending` is the stream's last flush, where a partial character will never be completed and holding it
    // back would lose it outright
    let flush = |buffer: &mut Vec<u8>, credit: &Arc<Credit>, ending: bool| -> bool {
        let cut = if ending { buffer.len() } else { sendable(buffer) };
        if cut == 0 {
            return true;
        }
        let waited = credit.take();
        let text = String::from_utf8_lossy(&buffer[..cut]).into_owned();
        buffer.drain(..cut);
        if waited > Duration::ZERO {
            // The overload is reported rather than silent, and it is a *wait* rather than a loss: the pty's
            // own buffer is what fills behind this, which is backpressure reaching the process.
            let throttled = Event::Throttled {
                session: session.clone(),
                waited_us: waited.as_micros().min(u32::MAX as u128) as u32,
            };
            if outgoing.send(Outgoing::answer(throttled)).is_err() {
                return false;
            }
        }
        outgoing
            .send(Outgoing {
                event: Event::Output { session: session.clone(), text },
                credit: Some(Arc::clone(credit)),
            })
            .is_ok()
    };

    loop {
        let remaining = OUTPUT_WINDOW.saturating_sub(opened.elapsed());
        match chunks.recv_timeout(remaining) {
            Ok(chunk) => {
                buffer.extend_from_slice(&chunk);
                // A fast producer fills the cap before the window elapses; flushing on size keeps the
                // window from becoming a latency floor under saturation
                if buffer.len() >= OUTPUT_CAP {
                    if !flush(&mut buffer, &credit, false) {
                        return;
                    }
                    opened = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !flush(&mut buffer, &credit, false) {
                    return;
                }
                opened = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = flush(&mut buffer, &credit, true);
                return;
            }
        }
    }
}

/// The queue both classes share, with the credit that bounds the output one
pub fn channel_with_credit() -> (Sender<Outgoing>, Receiver<Outgoing>, Arc<Credit>) {
    let (sender, receiver) = channel();
    (sender, receiver, Credit::new(OUTPUT_CREDIT))
}
