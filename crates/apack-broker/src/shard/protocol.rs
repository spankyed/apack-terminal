//! Shard mailbox command/event protocol, wake primitives, and shared broker handles.

use std::collections::{HashSet, VecDeque};
use std::fmt;
#[cfg(windows)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use apack_core::flow::OutboundPriority;
use apack_core::lease::ConnId;
use apack_core::session::StreamAttachmentId;
use apack_os::image_sources::Loaded;
#[cfg(unix)]
use apack_os::pty_poller::PtyWaker;
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SessionSummary, SpawnSpec};
use apack_proto::types::{
    Confirmed, NonZeroStreamId, RequestId, SessionId, SignalKind, SignalScope, SignalTarget, StreamId, SubFlags,
    ViewportHint, WaitCondition,
};
use apack_vt_ghostty::ImageStore;
use apack_wal::{RecordingRegistry, WalConfig, WalReaper};

/// Per-session high-water cap (`2 MiB`) on raw PTY output bytes in flight to the Control Reactor.
pub(crate) const RAW_OUTPUT_SESSION_HIGH_WATER: usize = 2_097_152;

/// Per-session low-water resume threshold (`1 MiB`) for in-flight raw PTY output bytes.
pub(crate) const RAW_OUTPUT_SESSION_LOW_WATER: usize = 1_048_576;

/// Per-shard aggregate high-water cap (`8 MiB`) on raw PTY output bytes in flight to the Control Reactor.
pub(crate) const RAW_OUTPUT_SHARD_HIGH_WATER: usize = 8_388_608;

/// Per-shard aggregate low-water resume threshold (`4 MiB`) for in-flight raw PTY output bytes.
pub(crate) const RAW_OUTPUT_SHARD_LOW_WATER: usize = 4_194_304;

/// Returns `true` when in-flight `PublishRawOutput` bytes hit either the per-session or per-shard cap.
pub(crate) const fn should_pause_raw_inflight(session_bytes: usize, shard_bytes: usize) -> bool {
    session_bytes >= RAW_OUTPUT_SESSION_HIGH_WATER || shard_bytes >= RAW_OUTPUT_SHARD_HIGH_WATER
}

/// Returns `true` when draining `PublishRawOutput` crossed from `>= low_water` to `< low_water`.
pub(crate) const fn crossed_raw_low_water(prev_bytes: usize, next_bytes: usize, low_water: usize) -> bool {
    prev_bytes >= low_water && next_bytes < low_water
}

/// Returns `true` when draining `PublishRawOutput` crossed either the per-session or per-shard low-water mark.
pub(crate) const fn should_notify_raw_drained(
    prev_sess: usize,
    next_sess: usize,
    prev_shard: usize,
    next_shard: usize,
) -> bool {
    crossed_raw_low_water(prev_sess, next_sess, RAW_OUTPUT_SESSION_LOW_WATER)
        || crossed_raw_low_water(prev_shard, next_shard, RAW_OUTPUT_SHARD_LOW_WATER)
}

/// RAII permit bounding concurrent in-flight shard commands and shard->reactor replies per client.
///
/// Dropping the last `Arc<ShardCmdPermit>` (after the Control Reactor consumes the final reply or
/// after a fire-and-forget shard command completes) releases the slot automatically and wakes the
/// Control Reactor when the connection is waiting on `ClientFin` half-close drain.
pub struct ShardCmdPermit {
    counter: Arc<AtomicUsize>,
    fin_pending: Option<Arc<AtomicBool>>,
    wake_reactor: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for ShardCmdPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShardCmdPermit").field("inflight", &self.counter.load(Ordering::Relaxed)).finish()
    }
}

impl ShardCmdPermit {
    /// Attempts to acquire one command slot below `limit`, returning `None` if already at cap.
    pub fn try_acquire(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        Self::try_acquire_with_wake(counter, limit, None, None)
    }

    /// Attempts to acquire one command slot below `limit`, waking `wake_reactor` when the final
    /// permit drops while `fin_pending` is set.
    pub fn try_acquire_with_wake(
        counter: &Arc<AtomicUsize>,
        limit: usize,
        fin_pending: Option<Arc<AtomicBool>>,
        wake_reactor: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Option<Self> {
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| (cur < limit).then_some(cur + 1))
            .ok()
            .map(|_| Self { counter: Arc::clone(counter), fin_pending, wake_reactor })
    }
}

impl Drop for ShardCmdPermit {
    fn drop(&mut self) {
        // SeqCst Dekker pair with `BrokerAction::ClientFin` (`fin_pending.store(true, SeqCst)`
        // followed by `has_inflight_cmds()` `SeqCst` load) so a fire-and-forget command finishing
        // concurrently with `ClientFin` can never lose its reactor wakeup on ARM64.
        let prev = self.counter.fetch_sub(1, Ordering::SeqCst);
        if prev == 1
            && self.fin_pending.as_ref().is_some_and(|f| f.load(Ordering::SeqCst))
            && let Some(wake) = &self.wake_reactor
        {
            wake();
        }
    }
}

/// Events sent from a Session Shard back to the single-threaded Control & Transport Reactor.
#[derive(Debug)]
pub enum ReactorEvent {
    /// Enqueue a control or stream message on `conn`.
    SendMsg {
        /// Destination connection.
        conn: ConnId,
        /// Destination wire stream.
        stream_id: StreamId,
        /// Attachment lifecycle ID (`Some` for stream-scoped messages, `None` for control stream).
        attachment: Option<StreamAttachmentId>,
        /// Correlation ID (`RequestId::PUSH` for unsolicited events).
        req_id: RequestId,
        /// Outbound priority tier.
        priority: OutboundPriority,
        /// Encoded protocol message.
        msg: BrokerMsg,
        /// Optional in-flight command permit released strictly after the reactor enqueues `msg`.
        permit: Option<Arc<ShardCmdPermit>>,
    },
    /// Publish a coalesced differential or full-sync view frame for `(conn, stream_id, attachment_id)`.
    PublishFrame {
        /// Destination connection.
        conn: ConnId,
        /// Bound view stream.
        stream_id: NonZeroStreamId,
        /// Monotonic attachment lifecycle ID.
        attachment_id: StreamAttachmentId,
        /// Owning session.
        session: SessionId,
        /// Frame payload.
        frame: Box<FrameUpdate>,
        /// `true` for the final exit `full_sync` frame, which is delivered as a non-droppable
        /// ordered `Event` strictly ahead of `Exited`/`StreamClosed`.
        final_frame: bool,
    },
    /// Publish raw PTY output bytes to all `SUB_RAW_OUTPUT` subscribers of `session`.
    PublishRawOutput {
        /// Source session.
        session: SessionId,
        /// Raw output bytes.
        bytes: Vec<u8>,
        /// Per-session in-flight byte counter decremented on reactor drain.
        session_inflight: Arc<AtomicUsize>,
        /// Per-shard aggregate in-flight byte counter decremented on reactor drain.
        shard_inflight: Arc<AtomicUsize>,
    },
    /// Confirm a session was spawned or rewound and registered on `shard_idx`.
    SessionRegistered {
        /// Assigned session ID.
        session: SessionId,
        /// Connection that requested `Spawn` or `Rewind` (owner of `ConnBound` sessions).
        owner: ConnId,
        /// Owning shard index.
        shard_idx: usize,
        /// `true` for read-only rewound recordings.
        rewound: bool,
        /// Initial `kept` status.
        kept: bool,
    },
    /// Notify the Control Reactor that a pending `Spawn` or `Rewind` failed before registration.
    SpawnOrRewindFailed,
    /// Confirm a session's `kept` flag changed.
    SessionKeptUpdated {
        /// Session ID.
        session: SessionId,
        /// New kept flag.
        kept: bool,
    },
    /// Notify the Control Reactor that `session` exited (or rewound session closed) and was removed.
    SessionEnded {
        /// Ended session ID.
        session: SessionId,
    },
    /// Partial `ListSessions` contribution from one shard for an in-flight aggregation.
    SessionListPart {
        /// Broker-allocated aggregation ID.
        list_id: u64,
        /// Shard's session summaries.
        sessions: Vec<SessionSummary>,
        /// Shared `ListSessions` permit released after the reactor enqueues the merged reply.
        permit: Option<Arc<ShardCmdPermit>>,
    },
}

/// Commands dispatched from the Control Reactor (or off-shard workers) into a Shard's Tier 0 mailbox.
#[derive(Debug)]
pub enum ShardCmd {
    /// Spawn a live PTY session assigned to `session_id`.
    Spawn {
        /// Assigned session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// Validated spawn specification.
        spec: Box<SpawnSpec>,
    },
    /// Open a read-only rewound session from `record_dir`.
    Rewind {
        /// Assigned session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// Recording directory path.
        record_dir: String,
        /// Optional initial seek timestamp (ms).
        at_ms: Option<u64>,
    },
    /// Seek an open rewound session to `at_ms`.
    Seek {
        /// Rewound session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// Target timestamp (ms).
        at_ms: u64,
    },
    /// Terminate a live or rewound session.
    Kill {
        /// Target session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
    },
    /// Deliver a POSIX signal to a live session.
    Signal {
        /// Target session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// Signal kind.
        signal: SignalKind,
        /// Signal scope.
        scope: SignalScope,
    },
    /// Query whether `pid` belongs to `session_id`'s process tree.
    Owns {
        /// Target session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// Positive PID to inspect.
        pid: SignalTarget,
    },
    /// Update a session's `kept` flag.
    SetKept {
        /// Target session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// New kept state.
        kept: bool,
    },
    /// Register a reactive event-driven wait predicate.
    Wait {
        /// Target session ID.
        session_id: SessionId,
        /// Requesting connection.
        conn: ConnId,
        /// Correlation ID.
        req_id: RequestId,
        /// Wait condition.
        condition: WaitCondition,
        /// Deadline in milliseconds.
        timeout_ms: u32,
    },
    /// Attach a multiplexed stream to `session_id`.
    Attach {
        /// Target session ID.
        session_id: SessionId,
        /// Owning connection.
        conn: ConnId,
        /// Correlation ID for `Attached` reply.
        req_id: RequestId,
        /// Non-zero stream ID.
        stream_id: NonZeroStreamId,
        /// Monotonic attachment lifecycle ID minted by `BrokerCore`.
        attachment_id: StreamAttachmentId,
        /// Subscription flags.
        flags: SubFlags,
        /// Optional initial viewport hint.
        initial_viewport: Option<ViewportHint>,
    },
    /// Detach a stream from `session_id`.
    Detach {
        /// Target session ID.
        session_id: SessionId,
        /// Owning connection.
        conn: ConnId,
        /// Stream ID.
        stream_id: NonZeroStreamId,
        /// Monotonic attachment lifecycle ID being detached.
        attachment_id: StreamAttachmentId,
    },
    /// Cancel all pending waits and subscriptions owned by `conn` across all sessions on this shard.
    CancelConnWaiters {
        /// Disconnected connection.
        conn: ConnId,
    },
    /// Dispatch a validated per-session stream message.
    StreamMsg {
        /// Target session ID.
        session_id: SessionId,
        /// Owning connection.
        conn: ConnId,
        /// Stream ID.
        stream_id: NonZeroStreamId,
        /// Active attachment lifecycle ID.
        attachment_id: StreamAttachmentId,
        /// Correlation ID.
        req_id: RequestId,
        /// Client message.
        msg: ClientMsg,
    },
    /// Notify the shard that the Control Reactor accepted the previous view frame for
    /// `(conn, stream_id, attachment_id)` and is ready for the next coalesced diff.
    ViewSlotFree {
        /// Target session ID.
        session_id: SessionId,
        /// Owning connection.
        conn: ConnId,
        /// Bound view stream ID.
        stream_id: NonZeroStreamId,
        /// Monotonic attachment lifecycle ID.
        attachment_id: StreamAttachmentId,
        /// When `true`, marks the subscriber for a self-contained `full_sync` frame (used when a
        /// deferred frame had to be dropped under `MemoryBudget` pressure).
        force_full_sync: bool,
    },
    /// Notify the shard that in-flight `PublishRawOutput` bytes drained below low-water mark.
    RawOutputDrained,
    /// Engage or release `BACKPRESSURE_PTY` pause on `session_id`.
    SetPtyPaused {
        /// Target session ID.
        session_id: SessionId,
        /// `true` to pause PTY reads, `false` to resume.
        paused: bool,
    },
    /// Collect summaries for all sessions owned by this shard.
    ListSessions {
        /// Broker-allocated aggregation ID.
        list_id: u64,
    },
    /// Tear down all non-kept sessions whose IDs are NOT in `pinned`.
    TeardownNonKept {
        /// Currently pinned session IDs (via `LeaseKind::Session`).
        pinned: HashSet<SessionId>,
    },
    /// Tear down ALL sessions (kept and non-kept alike) during explicit broker shutdown.
    TeardownAll,
    /// Close an unpinned non-kept `LeaseBound` session if no broker-wide lease is active.
    SessionUnpinned {
        /// Unpinned session ID.
        session_id: SessionId,
        /// Whether any broker-wide lease is currently active.
        broker_lease_active: bool,
    },
    /// Tear down one specific session (e.g. `ConnBound` owner disconnected or orphaned mid-spawn).
    TeardownSession {
        /// Target session ID.
        session_id: SessionId,
    },
    /// Batched foreground-process verification results from the off-shard blocking worker.
    ForegroundVerified {
        /// Target session ID.
        session_id: SessionId,
        /// `(slot_id, revision, verdict)` tuples verified against the same foreground `argv`.
        results: Vec<(String, u64, Confirmed)>,
        /// `true` when produced by `fg_worker` (releasing the session's in-flight slot),
        /// `false` for local fallback completions when `fg_worker` is unavailable.
        from_worker: bool,
    },
    /// Off-shard Kitty graphics named-source (`t=f` / `t=s`) read completion.
    ImageSourceLoaded {
        /// Target session ID.
        session_id: SessionId,
        /// Per-session ticket rejecting stale completions after cancel/teardown.
        ticket: u64,
        /// Completed load result, or `None` when timed out / circuit-breaker tripped.
        loaded: Option<Loaded>,
    },
    /// Gracefully tear down all sessions and exit the shard thread.
    Shutdown,
}

/// Mailbox queue paired with a `woken` flag under the same mutex so Windows `ConPtyBridge`
/// notifications can never be lost between draining output and waiting on `Condvar`.
#[derive(Default)]
pub(crate) struct ShardMailboxState {
    pub(crate) queue: VecDeque<(ShardCmd, Option<Arc<ShardCmdPermit>>)>,
    pub(crate) woken: bool,
}

/// Platform-neutral wake handle for notifying a sleeping shard thread.
#[derive(Clone)]
pub(super) enum ShardWake {
    #[cfg(unix)]
    Poller(PtyWaker),
    #[cfg(windows)]
    Cond(Arc<Mutex<ShardMailboxState>>, Arc<Condvar>),
}

impl ShardWake {
    pub(super) fn wake(&self) {
        match self {
            #[cfg(unix)]
            Self::Poller(w) => {
                let _ = w.wake();
            }
            #[cfg(windows)]
            Self::Cond(mailbox, cv) => {
                let mut state = mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.woken = true;
                cv.notify_one();
            }
        }
    }
}

/// Lock-free per-shard telemetry counters read directly by the Control Reactor on `Metrics`.
#[derive(Default)]
pub(super) struct ShardTelemetryAtomics {
    pub(super) scrollback_bytes: AtomicU64,
    pub(super) total_pty_bytes: AtomicU64,
}

/// Handle held by the Control Reactor to communicate with one Session Shard.
pub struct ShardHandle {
    pub(super) mailbox: Arc<Mutex<ShardMailboxState>>,
    pub(super) wake: ShardWake,
    pub(super) load: Arc<AtomicUsize>,
    pub(super) telemetry: Arc<ShardTelemetryAtomics>,
    pub(super) thread: Option<JoinHandle<()>>,
}

impl ShardHandle {
    #[cfg(all(test, unix))]
    pub(crate) fn dummy_for_test() -> (Self, Arc<Mutex<ShardMailboxState>>) {
        let poller = apack_os::pty_poller::PtyPoller::new().expect("dummy poller");
        let mailbox = Arc::new(Mutex::new(ShardMailboxState::default()));
        let handle = Self {
            mailbox: Arc::clone(&mailbox),
            wake: ShardWake::Poller(poller.waker()),
            load: Arc::new(AtomicUsize::new(0)),
            telemetry: Arc::new(ShardTelemetryAtomics::default()),
            thread: None,
        };
        (handle, mailbox)
    }

    /// Current number of sessions assigned to this shard (used for least-loaded placement).
    pub fn session_load(&self) -> usize {
        self.load.load(Ordering::Relaxed)
    }

    /// Reads `(scrollback_bytes, total_pty_bytes)` lock-free from this shard.
    pub fn telemetry(&self) -> (u64, u64) {
        (
            self.telemetry.scrollback_bytes.load(Ordering::Relaxed),
            self.telemetry.total_pty_bytes.load(Ordering::Relaxed),
        )
    }

    /// Pushes a system/lifecycle Tier 0 command into this shard's mailbox and wakes the shard thread immediately.
    pub fn send(&self, cmd: ShardCmd) {
        self.send_with_permit(cmd, None);
    }

    /// Pushes a Tier 0 command paired with an optional per-connection [`ShardCmdPermit`] into this
    /// shard's mailbox and wakes the shard thread immediately.
    pub fn send_with_permit(&self, cmd: ShardCmd, permit: Option<Arc<ShardCmdPermit>>) {
        {
            let mut state = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
            state.queue.push_back((cmd, permit));
            state.woken = true;
        }
        self.wake.wake();
    }

    /// Signals shutdown and joins the shard thread cleanly.
    pub fn shutdown(&mut self) {
        self.send(ShardCmd::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ShardHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Shared broker resources injected into every shard.
#[derive(Clone)]
pub struct ShardShared {
    /// Global deduplicated content-addressed image store (`256 MiB` hard cap).
    pub image_store: Arc<Mutex<ImageStore>>,
    /// Global WAL directory single-writer + rewind pin registry.
    pub wal_registry: RecordingRegistry,
    /// Bounded non-blocking background reaper for asynchronously closed WAL workers.
    pub wal_reaper: WalReaper,
    /// WAL defaults for recorded sessions.
    pub wal_config: WalConfig,
    /// Idle delay before cooperative scrollback compression starts.
    pub idle_compress_after: Duration,
    /// Escalation delay after graceful `SIGHUP` before `SIGKILL`.
    pub kill_escalate_after: Duration,
    /// Cached `libghostty-vt` ABI hash for `EngineCheckpoint`.
    pub engine_abi_hash: [u8; 32],
}

/// Batched request sent to the shard's off-shard blocking worker to inspect a foreground PID's `argv`.
pub(super) struct FgVerifyReq {
    pub(super) session_id: SessionId,
    pub(super) pgid: Option<u32>,
    pub(super) items: Vec<(String, u64, String)>,
}
