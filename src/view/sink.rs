//! What is waiting to be written to one view connection.
//!
//! **A view's own queue, bounded.** A view is sent a frame and then nothing until it says it drew it, so
//! frames alone cannot pile up. Answers can: a view may ask for lines, for an image's pixels, for a search
//! — megabytes each — as fast as it can write requests, and whether it also reads the answers is up to
//! it. A queue that took whatever was asked for would be the broker's memory, spent by a view that had
//! stopped reading. So what is waiting is counted, and a connection with more waiting than `SINK_BYTES`
//! is one that is not reading: the queue is closed, what it held is dropped, and the connection ends.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use super::publish::Outlet;

/// How much may be waiting for one view connection to read.
///
/// Several of the largest messages there are (`wire::MAX_MESSAGE_BYTES`): room for every session a
/// window shows to be owed a whole screen at once, and for the answers to what the window asked meanwhile.
pub const SINK_BYTES: usize = 64 << 20;

/// One view connection's queue: filled by whatever has something for it, emptied by its writer.
pub struct Sink {
    queue: Mutex<Queue>,
    changed: Condvar,
    limit: usize,
    /// Closes the connection, which is what being overrun leads to. Called by whatever queued the
    /// message that overran it — a session's reader, with its screen locked — so it must not wait.
    hang_up: Box<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Vec<u8>>,
    bytes: usize,
    /// Nothing more is taken, and what was waiting is gone
    closed: bool,
    /// How many messages the writer has been handed
    handed: u64,
    /// Nothing more will be queued: what is waiting is the last of it
    finished: bool,
}

impl Sink {
    /// A sink for a connection that `hang_up` closes.
    ///
    /// **The connection is closed where the overrun happens**, not where it is next noticed: what
    /// overruns a sink is as likely a frame as an answer, the view that let it happen is not reading and
    /// may not be writing either, and the thread writing to it is parked in a write only the connection
    /// closing ends.
    pub fn new(hang_up: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Self::holding(SINK_BYTES, hang_up)
    }

    /// A sink that holds `limit` bytes at most, for a case about what happens past it.
    pub fn holding(limit: usize, hang_up: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(Queue::default()),
            changed: Condvar::new(),
            limit,
            hang_up: Box::new(hang_up),
        })
    }

    /// Queues a message. `false` when the sink is closed — as it is by this call, when the message
    /// would put more waiting than the view may leave unread.
    pub fn deliver(&self, message: Vec<u8>) -> bool {
        let Ok(mut queue) = self.queue.lock() else { return false };
        if queue.closed || queue.finished {
            return false;
        }
        if queue.bytes + message.len() > self.limit {
            queue.closed = true;
            queue.waiting.clear();
            queue.bytes = 0;
            drop(queue);
            self.changed.notify_all();
            (self.hang_up)();
            return false;
        }
        queue.bytes += message.len();
        queue.waiting.push_back(message);
        self.changed.notify_one();
        true
    }

    /// The next message to write, waiting for one. `None` when there will be no more: the sink is
    /// closed, or finished and empty.
    pub fn next(&self) -> Option<Vec<u8>> {
        let mut queue = self.queue.lock().ok()?;
        loop {
            if queue.closed {
                return None;
            }
            if let Some(message) = queue.waiting.pop_front() {
                queue.bytes -= message.len();
                queue.handed += 1;
                return Some(message);
            }
            if queue.finished {
                return None;
            }
            queue = self.changed.wait(queue).ok()?;
        }
    }

    /// Nothing more will be queued. What is waiting is still handed out, and then there is no more.
    pub fn finish(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.finished = true;
        }
        self.changed.notify_all();
    }

    /// Ends the sink: what is waiting is dropped, and nothing more is taken.
    pub fn close(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.closed = true;
            queue.waiting.clear();
            queue.bytes = 0;
        }
        self.changed.notify_all();
    }

    /// How many messages the writer has been handed so far: what moves while the view is reading and
    /// stands still while it is not.
    pub fn handed_out(&self) -> u64 {
        self.queue.lock().map_or(0, |queue| queue.handed)
    }
}

impl Outlet for Arc<Sink> {
    fn deliver(&self, message: Vec<u8>) -> bool {
        Sink::deliver(self, message)
    }
}
