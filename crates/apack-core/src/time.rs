//! Virtual monotonic timestamps and a unified cancelable deadline wheel.
//!
//! Nothing in `apack-core` polls wall clocks: callers sleep until [`TimerWheel::next_deadline`]
//! and advance time explicitly via [`TimerWheel::poll_expired`].

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::num::NonZeroU64;

/// Explicit monotonic virtual timestamp in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TimestampMs(u64);

impl TimestampMs {
    /// Zero epoch timestamp.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw millisecond timestamp.
    pub const fn from_millis(ms: u64) -> Self {
        Self(ms)
    }

    /// Returns the raw millisecond count.
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Adds `delta_ms` with saturation at `u64::MAX`.
    pub const fn saturating_add(self, delta_ms: u64) -> Self {
        Self(self.0.saturating_add(delta_ms))
    }

    /// Returns elapsed milliseconds since `earlier`, saturating at `0`.
    pub const fn saturating_sub(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Opaque non-zero identifier for a scheduled timer entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId(NonZeroU64);

impl TimerId {
    /// Returns the underlying non-zero `u64`.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Unified, deterministic, $O(\log N)$ cancelable deadline scheduler.
///
/// Ties at the same [`TimestampMs`] fire in strict FIFO order of [`TimerId`].
#[derive(Debug)]
pub struct TimerWheel<T> {
    next_id: u64,
    by_deadline: BTreeMap<(TimestampMs, TimerId), T>,
    by_id: BTreeMap<TimerId, TimestampMs>,
}

impl<T> Default for TimerWheel<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TimerWheel<T> {
    /// Creates an empty timer wheel.
    pub fn new() -> Self {
        Self { next_id: 1, by_deadline: BTreeMap::new(), by_id: BTreeMap::new() }
    }

    /// Returns the number of currently scheduled timers.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Returns `true` when no timers are scheduled.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Returns the earliest pending deadline, if any.
    pub fn next_deadline(&self) -> Option<TimestampMs> {
        self.by_deadline.first_key_value().map(|((at, _), _)| *at)
    }

    /// Schedules `payload` to fire at `at`, returning its cancellation [`TimerId`].
    pub fn schedule(&mut self, at: TimestampMs, payload: T) -> TimerId {
        let id = TimerId(NonZeroU64::new(self.next_id).expect("TimerId counter must stay non-zero"));
        self.next_id = self.next_id.checked_add(1).expect("TimerId overflow");
        self.by_deadline.insert((at, id), payload);
        self.by_id.insert(id, at);
        id
    }

    /// Cancels a previously scheduled timer, returning its payload if it had not yet fired.
    pub fn cancel(&mut self, id: TimerId) -> Option<T> {
        let at = self.by_id.remove(&id)?;
        self.by_deadline.remove(&(at, id))
    }

    /// Drains every timer with `deadline <= now` into `out` in deterministic `(deadline, id)` order.
    pub fn poll_expired(&mut self, now: TimestampMs, out: &mut Vec<(TimerId, T)>) {
        while let Some((&(at, id), _)) = self.by_deadline.first_key_value() {
            if at > now {
                break;
            }
            if let Some((_, payload)) = self.by_deadline.pop_first() {
                self.by_id.remove(&id);
                out.push((id, payload));
            }
        }
    }
}
