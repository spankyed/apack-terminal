//! Bounded, non-blocking background reaper for asynchronously closed [`WalHandle`] worker threads.
//!
//! Ensures session exit/teardown on a hot shard thread never blocks on a 30-second flush barrier
//! or hung NFS/USB disk `join()`, while still reaping healthy workers promptly and capping
//! retained handles under permanent I/O stalls.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::WalHandle;

/// Default maximum number of in-flight closing WAL worker handles tracked per [`WalReaper`].
pub const MAX_REAP_HANDLES: usize = 64;

const REAPER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(5);

struct ReaperState {
    pending: VecDeque<JoinHandle<()>>,
    shutting_down: bool,
    started: bool,
}

struct ReaperInner {
    state: Mutex<ReaperState>,
    cv: Condvar,
    max_handles: usize,
}

impl ReaperInner {
    fn take_finished_locked(state: &mut ReaperState) -> Vec<JoinHandle<()>> {
        let mut finished = Vec::new();
        let mut idx = 0;
        while idx < state.pending.len() {
            if state.pending[idx].is_finished() {
                if let Some(h) = state.pending.remove(idx) {
                    finished.push(h);
                }
            } else {
                idx += 1;
            }
        }
        finished
    }
}

struct ReaperGuard(Arc<ReaperInner>);

impl Drop for ReaperGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.shutting_down = true;
        self.0.cv.notify_all();
    }
}

/// Bounded non-blocking reaper that joins asynchronously closed WAL worker threads off-shard.
#[derive(Clone)]
pub struct WalReaper {
    inner: Arc<ReaperInner>,
    _guard: Arc<ReaperGuard>,
}

impl std::fmt::Debug for WalReaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalReaper").field("max_handles", &self.inner.max_handles).finish_non_exhaustive()
    }
}

impl Default for WalReaper {
    fn default() -> Self {
        Self::new()
    }
}

impl WalReaper {
    /// Creates a new reaper bounded to [`MAX_REAP_HANDLES`] pending worker handles.
    pub fn new() -> Self {
        Self::with_max_handles(MAX_REAP_HANDLES)
    }

    /// Creates a new reaper bounded to at most `max_handles` (clamped `>= 1`) pending handles.
    pub fn with_max_handles(max_handles: usize) -> Self {
        let inner = Arc::new(ReaperInner {
            state: Mutex::new(ReaperState { pending: VecDeque::new(), shutting_down: false, started: false }),
            cv: Condvar::new(),
            max_handles: max_handles.max(1),
        });
        let guard = Arc::new(ReaperGuard(Arc::clone(&inner)));
        Self { inner, _guard: guard }
    }

    /// Closes `wal` without blocking and hands its worker thread to this reaper.
    pub fn close_async(&self, wal: WalHandle) {
        wal.close_async_into(self);
    }

    /// Accepts a detached worker `JoinHandle` without blocking, reaping any already-finished
    /// siblings and detaching the oldest handle if the bounded capacity is reached.
    pub fn reap(&self, handle: JoinHandle<()>) {
        let (finished, spawn_reaper) = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            let finished = ReaperInner::take_finished_locked(&mut state);
            while state.pending.len() >= self.inner.max_handles {
                let _ = state.pending.pop_front();
            }
            state.pending.push_back(handle);
            let spawn_reaper = !state.started;
            if spawn_reaper {
                state.started = true;
            }
            self.inner.cv.notify_all();
            (finished, spawn_reaper)
        };
        for h in finished {
            let _ = h.join();
        }
        if spawn_reaper {
            let bg = Arc::clone(&self.inner);
            if thread::Builder::new().name("apack-wal-reaper".to_owned()).spawn(move || run_reaper_loop(bg)).is_err() {
                let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                state.started = false;
            }
        }
    }

    /// Waits up to `timeout` for all currently pending closing WAL workers to finish and be joined.
    ///
    /// Returns `true` when all pending workers completed within `timeout`, or `false` if one or
    /// more workers (e.g. stalled on hung storage) are still running when `timeout` elapses.
    pub fn drain_bounded(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let (finished, empty) = {
                let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
                let finished = ReaperInner::take_finished_locked(&mut state);
                (finished, state.pending.is_empty())
            };
            for h in finished {
                let _ = h.join();
            }
            if empty {
                return true;
            }
            let rem = deadline.map_or(Duration::ZERO, |dl| dl.saturating_duration_since(Instant::now()));
            if rem.is_zero() {
                return false;
            }
            let state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            let _ = self.inner.cv.wait_timeout(state, rem.min(DRAIN_POLL_INTERVAL));
        }
    }

    /// Returns the number of still-running worker handles tracked by this reaper after sweeping any
    /// already-finished handles.
    pub fn pending_count(&self) -> usize {
        let (finished, count) = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            let finished = ReaperInner::take_finished_locked(&mut state);
            (finished, state.pending.len())
        };
        for h in finished {
            let _ = h.join();
        }
        count
    }

    #[cfg(test)]
    pub(crate) fn raw_pending_len(&self) -> usize {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner()).pending.len()
    }

    #[cfg(test)]
    pub(crate) fn probe(&self) -> ReaperProbe {
        ReaperProbe(Arc::clone(&self.inner))
    }
}

#[cfg(test)]
pub(crate) struct ReaperProbe(Arc<ReaperInner>);

#[cfg(test)]
impl ReaperProbe {
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).shutting_down
    }

    pub(crate) fn is_started(&self) -> bool {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).started
    }
}

fn run_reaper_loop(inner: Arc<ReaperInner>) {
    loop {
        let (finished, exit) = {
            let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pending.is_empty() && !state.shutting_down {
                state = inner.cv.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            let finished = ReaperInner::take_finished_locked(&mut state);
            if state.pending.is_empty() {
                inner.cv.notify_all();
                let shutting_down = state.shutting_down;
                if shutting_down {
                    state.started = false;
                }
                (finished, shutting_down)
            } else if state.shutting_down {
                // Detach still-stalled handles on reaper shutdown so process exit never hangs.
                state.pending.clear();
                state.started = false;
                (finished, true)
            } else {
                let _ = inner.cv.wait_timeout(state, REAPER_POLL_INTERVAL);
                (finished, false)
            }
        };
        for h in finished {
            let _ = h.join();
        }
        if exit {
            return;
        }
    }
}

pub(crate) fn global_reaper() -> &'static WalReaper {
    static GLOBAL: OnceLock<WalReaper> = OnceLock::new();
    GLOBAL.get_or_init(WalReaper::new)
}
