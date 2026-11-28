//! Off-shard single-flight worker with progress-resetting timeout and 1-stuck-thread-per-session
//! circuit breaker for reading untrusted Kitty graphics named sources.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Maximum silence before an unprogressing filesystem read is abandoned.
pub const QUIET: Duration = Duration::from_secs(5);

/// Multiplier deriving the hard total wall-clock deadline from `quiet` (`6 * 5s = 30s`),
/// preventing a slow-drip FUSE mount from pinning the worker indefinitely.
const TOTAL_DEADLINE_FACTOR: u32 = 6;

/// Per-session single-flight named-source reader with stuck-mount circuit breaker.
#[derive(Debug, Default)]
pub struct Worker {
    busy: Arc<AtomicBool>,
}

struct Free(Arc<AtomicBool>);

impl Drop for Free {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Worker {
    /// Runs `work` on a dedicated thread and returns `Some(T)` if it finishes without going
    /// `quiet` between progress ticks, or `None` if timed out, panicked, or already busy with a
    /// previously abandoned read.
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
        let started = std::thread::Builder::new().name("image-source".to_owned()).spawn(move || {
            let progress = || {
                let _ = tell.send(None);
            };
            let answer = work(&progress);
            drop(free);
            let _ = tell.send(Some(answer));
        });
        started.ok()?;
        let hard_deadline = Instant::now() + quiet.saturating_mul(TOTAL_DEADLINE_FACTOR);
        loop {
            let remaining = hard_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match told.recv_timeout(quiet.min(remaining)) {
                Ok(Some(answer)) => return Some(answer),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_returns_value_resets_on_progress_and_trips_circuit_breaker_when_stuck() {
        let worker = Worker::default();

        // Progress ticks keep a slow job alive past `quiet` (3 * 120ms = 360ms > 300ms quiet).
        let val = worker.run(Duration::from_millis(300), |progress| {
            for _ in 0..3 {
                std::thread::sleep(Duration::from_millis(120));
                progress();
            }
            42u32
        });
        assert_eq!(val, Some(42));

        // Stuck read times out and keeps `busy` tripped until the stuck thread exits.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let timed_out = worker.run(Duration::from_millis(30), move |_| {
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
            99u32
        });
        assert_eq!(timed_out, None);
        // Circuit breaker refuses immediately while the stuck thread is still blocked.
        assert_eq!(worker.run(Duration::from_millis(50), |_| 1u32), None);

        // Once the stuck thread finishes, the worker recovers cleanly.
        let _ = release_tx.send(());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let recovered = loop {
            if let Some(v) = worker.run(Duration::from_millis(100), |_| 7u32) {
                break v;
            }
            assert!(std::time::Instant::now() < deadline, "worker did not recover");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(recovered, 7);

        // Slow-drip producer that keeps ticking before `quiet` still hits the hard total deadline (`6 * quiet`).
        let (drip_stop_tx, drip_stop_rx) = mpsc::channel::<()>();
        let drip = worker.run(Duration::from_millis(20), move |progress| {
            for _ in 0..40 {
                if drip_stop_rx.try_recv().is_ok() {
                    return 0u32;
                }
                std::thread::sleep(Duration::from_millis(10));
                progress();
            }
            1u32
        });
        let _ = drip_stop_tx.send(());
        assert_eq!(drip, None, "slow-drip worker must time out at hard total deadline");
    }
}
