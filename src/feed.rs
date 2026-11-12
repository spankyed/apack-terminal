//! What is waiting to be written to the client a broker reports to.
//!
//! **A client's own queue, and its own writer.** Everything a broker has to tell its client — a session's
//! output, a title, a command ending, the answer to a request — is put here by the one thread that takes
//! it off the broker's queue, and written from here by a thread that does nothing else. So a client that
//! has stopped reading holds up the thread writing to it and nothing more: a session that ends is still
//! forgotten, its views are still told, and a broker with nothing left still goes.
//!
//! **Bounded by letting the client go, never by making anything wait.** Output is bounded before it gets
//! here, by credit (`output`): a client that stops reading stops the program. Nothing else is, and must
//! not be — an answer held back until there is room is the deadlock that module describes, the client
//! waiting for something the broker is waiting to send. What is left is to count: a client with more
//! than `FEED_BYTES` waiting is one that is not reading, so its queue is closed, what it held is dropped
//! and its connection ends. The client is then where a client whose broker went away is, which it
//! already has to recover from.
//!
//! **Whatever is dropped is still accounted for.** A message carries the credit its output spent and the
//! word of anyone waiting for the writer to reach it (`Outgoing::written`), and both are owed whether it
//! was written or not: credit kept by a message nobody will write is output that stalls for ever.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use crate::output::Outgoing;

/// How much may be waiting for a client to read.
///
/// **More than output can ever have waiting**, which is what makes this a bound on everything else:
/// credit lets `OUTPUT_CREDIT` messages of `OUTPUT_CAP` bytes be unwritten at once, and a byte of output
/// is at most six in a frame, so output alone stays under 25 MiB and a client is never let go for a
/// program that prints. As much as one view connection may leave unread (`view::sink::SINK_BYTES`).
pub const FEED_BYTES: usize = 64 << 20;

/// The variable a broker is told a bound other than `FEED_BYTES` by, in bytes.
///
/// A parameter for the reason `listening::START_GRACE_ENV` is one: the bound is the subject of a case,
/// and a case about what happens past sixty-four megabytes would spend its time writing them.
pub const FEED_BYTES_ENV: &str = "APACK_TERMINAL_FEED_BYTES";

/// The bound a broker was started with: what `FEED_BYTES_ENV` holds when it is a number of bytes, and
/// `FEED_BYTES` when it is unset or anything else.
pub fn feed_bytes(asked: Option<&str>) -> usize {
    asked.and_then(|asked| asked.parse().ok()).unwrap_or(FEED_BYTES)
}

/// One message as it waits: the line to write, or none for a place in the queue somebody is waiting for
/// the writer to reach, and what is owed once it has been dealt with.
pub type Waiting = (Option<String>, Outgoing);

/// One client's queue: filled by the broker's writer, emptied by the client's own.
pub struct Feed {
    queue: Mutex<Queue>,
    changed: Condvar,
    limit: usize,
    /// Closes the connection, which is what being overrun leads to. Called by the thread that queued
    /// the message that overran it, which must not wait.
    hang_up: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Waiting>,
    bytes: usize,
    /// Nothing more is taken, and what was waiting is gone
    closed: bool,
}

impl Feed {
    /// A feed for a connection that `hang_up` closes, holding `limit` bytes at most.
    pub fn holding(limit: usize, hang_up: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
            limit,
            hang_up: Box::new(hang_up),
        })
    }

    /// The queue, locked. Every change to it is whole before the lock is let go, so one a thread
    /// panicked holding is still true.
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a message, and never waits. `false` when the feed is closed — as it is by this call, when
    /// the message would put more waiting than the client may leave unread.
    pub fn deliver(&self, frame: Option<String>, message: Outgoing) -> bool {
        let size = frame.as_ref().map_or(0, String::len);
        let mut queue = self.queue();
        if queue.closed {
            drop(queue);
            message.written();
            return false;
        }
        if queue.bytes + size > self.limit {
            let dropped = close(&mut queue);
            drop(queue);
            self.changed.notify_all();
            settle(dropped);
            message.written();
            (self.hang_up)();
            return false;
        }
        queue.bytes += size;
        queue.waiting.push_back((frame, message));
        drop(queue);
        self.changed.notify_one();
        true
    }

    /// The next message to write, waiting for one. `None` when there will be no more.
    pub fn next(&self) -> Option<Waiting> {
        let mut queue = self.queue();
        loop {
            if queue.closed {
                return None;
            }
            if let Some(next) = queue.waiting.pop_front() {
                queue.bytes -= next.0.as_ref().map_or(0, String::len);
                return Some(next);
            }
            queue = self.changed.wait(queue).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Ends the feed: what is waiting is dropped, and nothing more is taken. The connection is left as
    /// it is, which is whoever closes this to decide.
    pub fn close(&self) {
        let dropped = close(&mut self.queue());
        self.changed.notify_all();
        settle(dropped);
    }
}

/// Closes a queue and answers what was waiting in it.
fn close(queue: &mut Queue) -> VecDeque<Waiting> {
    queue.closed = true;
    queue.bytes = 0;
    std::mem::take(&mut queue.waiting)
}

/// Accounts for messages that will not be written, with no lock held: what each owes is another
/// thread's to hear of.
fn settle(dropped: VecDeque<Waiting>) {
    for (_, message) in dropped {
        message.written();
    }
}
