//! Reading a named source on a thread of its own, and giving up on one that has stopped answering.
//!
//! **A read the system is in the middle of cannot be cancelled.** A file on a mount that has stopped
//! answering — a hard-mounted network share, a FUSE filesystem whose server has hung — holds `open`,
//! `fstat` and `read` in a wait no signal ends, and `O_NONBLOCK` means nothing to a regular file. So the
//! read is made on a thread that can be left in that wait, and whoever is waiting for its answer stops
//! waiting once it has gone `QUIET` without reading anything: the session is answered as for a source that
//! could not be read, and goes on.
//!
//! **Quiet, not slow, is what is given up on.** The read says each time it has read some of the source,
//! and each one starts the wait again, so a large file on a slow disk is read however long it takes. Only
//! a read that has stopped moving is let go of.
//!
//! **A thread left waiting is one a session may have.** While it is still in its wait, the session's next
//! named source is refused without a thread being started for it, so a mount that never answers again
//! costs a session one thread and not one for every image its program names.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// How long a read may go without reading anything before it is taken to have stopped answering.
///
/// **Declared, not measured**: a disk that answers at all answers a megabyte in far less, and one that
/// has stopped answers in none. It bounds how long a session's output waits on a source that will not
/// answer, which is the one cost of waiting at all.
pub const QUIET: Duration = Duration::from_secs(5);

/// Where a session's named sources are read: one at a time, each on a thread of its own.
#[derive(Debug, Default)]
pub struct Worker {
    /// A read is under way, or was given up on and has not yet ended
    busy: Arc<AtomicBool>,
}

/// Says the worker is free once the read it was given has ended, however it ended.
struct Free(Arc<AtomicBool>);

impl Drop for Free {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Worker {
    /// Runs `work` on a thread of its own, and answers what it answers — or `None` once it has gone
    /// `quiet` without saying it has made progress, or has ended without an answer, or where an earlier
    /// read that was given up on has not ended yet. `work` is handed what to call each time it has.
    pub fn run<T: Send + 'static>(
        &self,
        quiet: Duration,
        work: impl FnOnce(&dyn Fn()) -> T + Send + 'static,
    ) -> Option<T> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        let free = Free(Arc::clone(&self.busy));
        let (tell, told) = mpsc::channel();
        let started = std::thread::Builder::new().name("image source".to_owned()).spawn(move || {
            let progress = || {
                let _ = tell.send(None);
            };
            let answer = work(&progress);
            // Free before answering, so whoever is told can name the next source at once
            drop(free);
            let _ = tell.send(Some(answer));
        });
        // A thread that could not be started did nothing, and the worker went free with what it was given
        started.ok()?;
        loop {
            match told.recv_timeout(quiet) {
                Ok(Some(answer)) => return Some(answer),
                Ok(None) => {}
                // Quiet for as long as is waited, or ended without an answer
                Err(_) => return None,
            }
        }
    }
}
