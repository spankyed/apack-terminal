//! Connection-bound lifetime leases with opt-in remote reconnect grace.
//!
//! - Local default (`LeaseKind::BrokerImmediate`): disconnecting drops the lease immediately so
//!   non-kept sessions tear down with zero grace window.
//! - Remote opt-in (`LeaseKind::BrokerGrace`): disconnecting transitions the lease into bounded
//!   grace (`1..=MAX_LEASE_GRACE_MS`). Reconnecting with the same 32-byte [`ResumeToken`]
//!   (verified in constant time) seamlessly resumes the lease and cancels its grace timer.
//! - Per-session (`LeaseKind::Session`): pins one specific session (e.g. `Rewind` or ephemeral tool)
//!   to the holding connection.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::num::NonZeroU64;

use apack_proto::types::{LeaseGraceMs, LeaseKind, ResumeToken, SessionId};

use crate::MAX_IN_GRACE_LEASES;
use crate::auth::ct_eq_32;
use crate::time::{TimerId, TimestampMs};

/// Non-zero connection identifier assigned by the transport reactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(NonZeroU64);

impl ConnId {
    /// Creates a `ConnId` from a raw `u64`, returning `None` for `0`.
    pub fn new(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(Self)
    }

    /// Returns the underlying non-zero `u64`.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Non-zero lease identifier assigned by [`LeaseTable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId(NonZeroU64);

impl LeaseId {
    /// Creates a `LeaseId` from a raw `u64`, returning `None` for `0`.
    pub fn new(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(Self)
    }

    /// Returns the underlying non-zero `u64`.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Error returned when a lease operation violates ownership or resource bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseError {
    /// Caller attempted to release a lease it does not own (or that does not exist).
    NotOwned,
    /// Another live connection already holds a lease with the exact same `ResumeToken`.
    TokenInUse,
}

#[derive(Debug, Clone, Copy)]
enum LeaseState {
    Held(ConnId),
    InGrace { timer: TimerId },
}

#[derive(Debug, Clone, Copy)]
struct LeaseEntry {
    kind: LeaseKind,
    state: LeaseState,
}

/// Outcome of [`LeaseTable::acquire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcquireOutcome {
    /// Assigned or resumed lease ID.
    pub lease_id: LeaseId,
    /// `true` if an existing in-grace lease was resumed.
    pub resumed: bool,
    /// Cancelled grace timer ID, if an in-grace lease was resumed.
    pub cancelled_timer: Option<TimerId>,
}

/// Outcome of releasing, disconnecting, or expiring leases.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LeaseChanges {
    /// Grace timers that must be scheduled for disconnected `BrokerGrace` leases: `(lease_id, deadline)`.
    pub schedule_grace: Vec<(LeaseId, TimestampMs)>,
    /// `true` when the last active broker lease (live or in-grace) just ended and non-kept sessions must tear down.
    pub teardown_non_kept: bool,
    /// Sessions whose last session-specific lease just ended.
    pub unpinned_sessions: Vec<SessionId>,
}

/// Tracks all live and in-grace broker and session leases.
#[derive(Debug)]
pub struct LeaseTable {
    next_id: u64,
    leases: BTreeMap<LeaseId, LeaseEntry>,
}

impl Default for LeaseTable {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaseTable {
    /// Creates an empty lease table.
    pub fn new() -> Self {
        Self { next_id: 1, leases: BTreeMap::new() }
    }

    /// Returns `true` when at least one broker lease is currently held or in reconnect grace.
    pub fn has_active_broker_lease(&self) -> bool {
        self.leases.values().any(|e| matches!(e.kind, LeaseKind::BrokerImmediate | LeaseKind::BrokerGrace { .. }))
    }

    /// Returns the number of currently disconnected leases waiting in reconnect grace.
    pub fn in_grace_count(&self) -> usize {
        self.leases.values().filter(|e| matches!(e.state, LeaseState::InGrace { .. })).count()
    }

    /// Returns `true` if `session` is currently pinned by at least one live `LeaseKind::Session`.
    pub fn is_session_pinned(&self, session: SessionId) -> bool {
        self.leases.values().any(|e| matches!(e.kind, LeaseKind::Session(s) if s == session))
    }

    /// Acquires a new lease or resumes an in-grace `BrokerGrace` lease matching `resume_token`.
    pub fn acquire(&mut self, conn: ConnId, kind: LeaseKind) -> Result<AcquireOutcome, LeaseError> {
        if let LeaseKind::BrokerGrace { grace_ms, resume_token } = kind
            && let Some((existing_id, existing_state)) = self.find_by_token(&resume_token)
        {
            return match existing_state {
                LeaseState::Held(_) => Err(LeaseError::TokenInUse),
                LeaseState::InGrace { timer } => {
                    let entry = self.leases.get_mut(&existing_id).expect("found entry");
                    entry.kind = LeaseKind::BrokerGrace { grace_ms, resume_token };
                    entry.state = LeaseState::Held(conn);
                    Ok(AcquireOutcome { lease_id: existing_id, resumed: true, cancelled_timer: Some(timer) })
                }
            };
        }

        let id = LeaseId(NonZeroU64::new(self.next_id).expect("LeaseId counter must stay non-zero"));
        self.next_id = self.next_id.checked_add(1).expect("LeaseId overflow");
        self.leases.insert(id, LeaseEntry { kind, state: LeaseState::Held(conn) });
        Ok(AcquireOutcome { lease_id: id, resumed: false, cancelled_timer: None })
    }

    /// Explicitly releases a lease owned by `conn`.
    pub fn release(&mut self, conn: ConnId, lease_id: LeaseId) -> Result<LeaseChanges, LeaseError> {
        let Some(entry) = self.leases.get(&lease_id) else {
            return Err(LeaseError::NotOwned);
        };
        if !matches!(entry.state, LeaseState::Held(owner) if owner == conn) {
            return Err(LeaseError::NotOwned);
        }
        let had_broker = self.has_active_broker_lease();
        let removed = self.leases.remove(&lease_id).expect("checked");
        let mut changes = LeaseChanges::default();
        if let LeaseKind::Session(sid) = removed.kind
            && !self.is_session_pinned(sid)
        {
            changes.unpinned_sessions.push(sid);
        }
        if had_broker && !self.has_active_broker_lease() {
            changes.teardown_non_kept = true;
        }
        Ok(changes)
    }

    /// Handles connection EOF: drops immediate and session leases, and moves `BrokerGrace` leases
    /// into reconnect grace (up to [`MAX_IN_GRACE_LEASES`]).
    ///
    /// `schedule_timer` is invoked to register each grace deadline in the broker's unified timer wheel.
    pub fn on_disconnect(
        &mut self,
        conn: ConnId,
        now: TimestampMs,
        mut schedule_timer: impl FnMut(LeaseId, TimestampMs) -> TimerId,
    ) -> LeaseChanges {
        let had_broker = self.has_active_broker_lease();
        let owned: Vec<(LeaseId, LeaseKind)> = self
            .leases
            .iter()
            .filter_map(|(id, e)| match e.state {
                LeaseState::Held(owner) if owner == conn => Some((*id, e.kind)),
                _ => None,
            })
            .collect();

        let mut changes = LeaseChanges::default();
        let mut released_sessions = BTreeSet::new();
        for (id, kind) in owned {
            match kind {
                LeaseKind::BrokerImmediate => {
                    self.leases.remove(&id);
                }
                LeaseKind::BrokerGrace { grace_ms, .. } => {
                    if self.in_grace_count() < MAX_IN_GRACE_LEASES {
                        let deadline = grace_deadline(now, grace_ms);
                        let timer = schedule_timer(id, deadline);
                        if let Some(entry) = self.leases.get_mut(&id) {
                            entry.state = LeaseState::InGrace { timer };
                        }
                        changes.schedule_grace.push((id, deadline));
                    } else {
                        // Hard cap reached: err on the side of teardown rather than leaking state.
                        self.leases.remove(&id);
                    }
                }
                LeaseKind::Session(sid) => {
                    self.leases.remove(&id);
                    released_sessions.insert(sid);
                }
            }
        }

        for sid in released_sessions {
            if !self.is_session_pinned(sid) {
                changes.unpinned_sessions.push(sid);
            }
        }
        if had_broker && !self.has_active_broker_lease() {
            changes.teardown_non_kept = true;
        }
        changes
    }

    /// Expires a disconnected grace lease when its timer fires.
    pub fn on_grace_expired(&mut self, lease_id: LeaseId) -> LeaseChanges {
        let had_broker = self.has_active_broker_lease();
        let expired = matches!(self.leases.get(&lease_id), Some(e) if matches!(e.state, LeaseState::InGrace { .. }));
        if expired {
            self.leases.remove(&lease_id);
        }
        let mut changes = LeaseChanges::default();
        if had_broker && !self.has_active_broker_lease() {
            changes.teardown_non_kept = true;
        }
        changes
    }

    fn find_by_token(&self, token: &ResumeToken) -> Option<(LeaseId, LeaseState)> {
        let mut matched = None;
        for (id, entry) in &self.leases {
            if let LeaseKind::BrokerGrace { resume_token, .. } = &entry.kind
                && ct_eq_32(resume_token.as_bytes(), token.as_bytes())
            {
                matched = Some((*id, entry.state));
            }
        }
        matched
    }
}

fn grace_deadline(now: TimestampMs, grace_ms: LeaseGraceMs) -> TimestampMs {
    now.saturating_add(u64::from(grace_ms.get()))
}
