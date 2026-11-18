//! What is written to a session's program, in order, by one thread.
//!
//! **Nothing that has something for the program waits for the program to take it.** A pty takes a write
//! when the program reads, and a program reads when it likes: one that has stopped reading — busy, stopped
//! on a signal, or simply not a reader — leaves a write to it blocked for as long as that lasts. Written
//! from wherever it arose, that block is whoever asked standing still, with whatever they had locked
//! locked under them; and for a large paste into a program that echoes it is a deadlock, when what is
//! locked is the screen: the paste waits for the program to read, the program waits for its echo to be
//! read, and the thread that reads it waits for the screen.
//!
//! So everything bound for the program — what a client writes, what a view types, pastes and points at,
//! and what the terminal itself answers a program's questions with — is put on a queue, and one thread
//! takes it off and writes it. The order things were queued in is the order the program reads them, and
//! the only thing a slow program can hold up is its own input.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};

/// How much may be waiting for a program to read before more is refused.
///
/// **A ceiling on what a program that does not read can make the broker hold.** Far more than anything
/// typed, and more than a paste a person would mean; a caller past it is told, rather than its input
/// being dropped or the queue growing with whatever is sent.
pub const INPUT_BYTES: usize = 16 << 20;

/// Why something for the program was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// More than `INPUT_BYTES` would be waiting
    Full,
    /// The session has ended: nothing queued would be read
    Closed,
    /// The pty refused a write, with the session still there. Nothing more is taken
    Broken,
}

/// A session's input: the queue, and the thread writing it to the pty.
pub struct Input {
    queue: Mutex<Queue>,
    /// Something was queued, or the queue was closed
    changed: Condvar,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Vec<u8>>,
    /// The bytes waiting, the ones being written among them
    bytes: usize,
    closed: bool,
    /// Closed by the pty refusing a write rather than by the session ending
    broken: bool,
    /// The program has ended and been waited for: nothing can read what letting go of the pty writes
    reaped: bool,
    /// The pty's writer, put down by the thread that wrote with it and not yet let go of (`release`)
    kept: Option<Box<dyn Write + Send>>,
}

impl Input {
    /// Starts the thread that writes to `writer`, which is the pty's and from here on this thread's
    /// alone. It ends when the input is closed or the pty refuses a write.
    pub fn start(writer: Box<dyn Write + Send>) -> Arc<Self> {
        let input = Arc::new(Self { queue: Mutex::new(Queue::default()), changed: Condvar::new() });
        std::thread::spawn({
            let input = Arc::clone(&input);
            move || input.write_to(writer)
        });
        input
    }

    /// Queues bytes for the program, after everything queued before them. Never waits for the program.
    pub fn send(&self, bytes: Vec<u8>) -> Result<(), Refused> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Ok(mut queue) = self.queue.lock() else { return Err(Refused::Closed) };
        if queue.closed {
            return Err(if queue.broken { Refused::Broken } else { Refused::Closed });
        }
        if queue.bytes + bytes.len() > INPUT_BYTES {
            return Err(Refused::Full);
        }
        queue.bytes += bytes.len();
        queue.waiting.push_back(bytes);
        self.changed.notify_one();
        Ok(())
    }

    /// Ends the input: what is waiting is dropped, nothing more is taken, and the thread stops at the
    /// next thing it would have written. For a session that is being ended.
    ///
    /// **The pty's writer is not let go of here.** Letting go of one types a line ending and an
    /// end-of-input into the pty (`portable-pty` does, so a program reading its input sees it end) —
    /// which, into a shell with a line half typed, runs the line. A session is closed with whatever was
    /// on its prompt unsubmitted, so the writer is kept until the program is gone (`release`).
    pub fn close(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.closed = true;
            queue.waiting.clear();
            queue.bytes = 0;
        }
        self.changed.notify_one();
    }

    /// The program has ended and been waited for: the pty's writer is let go of, now or when the thread
    /// that holds it puts it down.
    pub fn release(&self) {
        let kept = self.queue.lock().ok().and_then(|mut queue| {
            queue.reaped = true;
            queue.kept.take()
        });
        // Outside the lock: letting go of it writes to the pty
        drop(kept);
    }

    fn write_to(&self, mut writer: Box<dyn Write + Send>) {
        loop {
            let next = {
                let Ok(mut queue) = self.queue.lock() else { break };
                loop {
                    if queue.closed {
                        break None;
                    }
                    if let Some(next) = queue.waiting.pop_front() {
                        break Some(next);
                    }
                    let Ok(woken) = self.changed.wait(queue) else { break None };
                    queue = woken;
                }
            };
            let Some(next) = next else { break };
            // **Outside the lock**, which is the point: this is the write that can take as long as the
            // program likes, and meanwhile more is queued behind it
            if writer.write_all(&next).and_then(|()| writer.flush()).is_err() {
                if let Ok(mut queue) = self.queue.lock() {
                    // Unless the session ended under the write, which is the ending's to say
                    queue.broken = !queue.closed;
                }
                self.close();
                break;
            }
            if let Ok(mut queue) = self.queue.lock() {
                queue.bytes = queue.bytes.saturating_sub(next.len());
            }
        }
        // Put down rather than dropped, unless the program is already gone
        if let Ok(mut queue) = self.queue.lock()
            && !queue.reaped
        {
            queue.kept = Some(writer);
            return;
        }
        drop(writer);
    }
}
