//! Exclusive-owner Session Shard runtime (`K = min(cores, 8)` threads, zero locks on VT state).
//!
//! Each shard owns its assigned [`SessionMachine<GhosttyEngine>`] instances exclusively:
//! - On Unix, a single [`apack_os::pty_poller::PtyPoller`] multiplexes every live session's
//!   `O_NONBLOCK` master PTY fd (zero per-session reader/writer threads).
//! - On Windows, each live session wraps [`apack_os::pty_windows::ConPtyBridge`] waking the
//!   shard's `Condvar` via bounded channels and dropping its master handle off-shard on exit.
//! - Every shard turn executes strictly:
//!   1. **Tier 0** (control/interactive mailbox: input, resize, attach/detach, kill, signal, seek)
//!   2. **Tier 1** (live PTY output round-robin, capped at `256 KiB` per session per turn)
//!   3. **Tier 2** (session timers, child reap, 2 s kill escalation, cooperative idle compression)

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
#[cfg(windows)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use apack_core::flow::OutboundPriority;
use apack_core::lease::ConnId;
use apack_core::marks::MarksMode;
use apack_core::session::{SessionEffect, SessionMachine, StreamKey};
use apack_core::time::TimestampMs;
use apack_core::vt::VtEngine;
#[cfg(unix)]
use apack_os::pty::{NonBlockingRead, PtyMaster};
use apack_os::pty::{PtySpawnConfig, spawn_pty};
#[cfg(unix)]
use apack_os::pty_poller::{Interest, PollEvent, PollToken, PtyPoller, PtyWaker};
#[cfg(windows)]
use apack_os::pty_windows::ConPtyBridge;
#[cfg(unix)]
use apack_os::signal::hang_up_session_descendants;
use apack_os::signal::{deliver_signal, kill_session_descendants, signal_name_from_description};
use apack_os::tree::TreeGuard;
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SessionSummary, SpawnSpec};
use apack_proto::types::{
    Confirmed, ErrorCode, NonZeroStreamId, RequestId, ResizePolicy, SessionId, SignalKind, SignalScope, SignalTarget,
    StreamId, SubFlags, ViewportHint, WaitCondition,
};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, ImageStore};
use apack_wal::{
    EngineCheckpoint, KeyframeV2, MAX_ENGINE_SNAPSHOT_BYTES, MAX_KEYFRAME_HISTORY_ROWS, RecordingRegistry,
    VisualKeyframe, WalConfig, WalHandle, WalReader, WalRecord, WalWorker, WalWriter,
};

use crate::config::AUTH_KEY_ENV;
#[cfg(windows)]
use crate::shard_windows::{drain_windows_pty_writes, wait_for_win_mailbox};
use crate::{fill_random, now_unix_ms};

/// Per-session PTY read cap per shard turn (`256 KiB`) preventing one flood from starving siblings.
pub const PTY_READ_QUANTUM_BYTES: usize = 262_144;

#[cfg(unix)]
const PTY_READ_BUF_BYTES: usize = 65_536;
#[cfg(unix)]
const PTY_WRITE_CHUNK_BYTES: usize = 16_384;

/// Escalation delay after graceful `SIGHUP` before `SIGKILL` of surviving descendants (`2 s`).
pub const KILL_ESCALATE_AFTER: Duration = Duration::from_secs(2);

/// Cooperative scrollback pages compressed per Tier 2 idle turn.
const COMPRESS_PAGES_PER_TURN: usize = 4;

/// Maximum time to poll between child-exit reap checks while a session is terminating (`20 ms`).
const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Default scrollback line cap when a caller passes `scrollback_limit == 0` on `SpawnSpec`.
const DEFAULT_SCROLLBACK_LINES: usize = 10_000;

/// Bounded queue depth for off-shard OSC 7501 foreground-process verification requests.
const FG_VERIFY_QUEUE_CAP: usize = 64;

/// Events sent from a Session Shard back to the single-threaded Control & Transport Reactor.
#[derive(Debug)]
pub enum ReactorEvent {
    /// Enqueue a control or stream message on `conn`.
    SendMsg {
        /// Destination connection.
        conn: ConnId,
        /// Destination wire stream.
        stream_id: StreamId,
        /// Correlation ID (`RequestId::PUSH` for unsolicited events).
        req_id: RequestId,
        /// Outbound priority tier.
        priority: OutboundPriority,
        /// Encoded protocol message.
        msg: BrokerMsg,
    },
    /// Publish a coalesced differential or full-sync view frame for `(conn, stream_id)`.
    PublishFrame {
        /// Destination connection.
        conn: ConnId,
        /// Bound view stream.
        stream_id: NonZeroStreamId,
        /// Owning session.
        session: SessionId,
        /// Frame payload.
        frame: Box<FrameUpdate>,
    },
    /// Publish raw PTY output bytes to all `SUB_RAW_OUTPUT` subscribers of `session`.
    PublishRawOutput {
        /// Source session.
        session: SessionId,
        /// Raw output bytes.
        bytes: Vec<u8>,
    },
    /// Confirm a session was spawned or rewound and registered on `shard_idx`.
    SessionRegistered {
        /// Assigned session ID.
        session: SessionId,
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
        /// Correlation ID.
        req_id: RequestId,
        /// Client message.
        msg: ClientMsg,
    },
    /// Notify the shard that the Control Reactor accepted the previous view frame for `(conn, stream_id)`
    /// and is ready for the next coalesced diff.
    ViewSlotFree {
        /// Target session ID.
        session_id: SessionId,
        /// Owning connection.
        conn: ConnId,
        /// Bound view stream ID.
        stream_id: NonZeroStreamId,
    },
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
    /// Close an unpinned rewound or non-kept session if it was only alive due to its session lease.
    SessionUnpinned {
        /// Unpinned session ID.
        session_id: SessionId,
        /// Whether any broker-wide lease is currently active.
        broker_lease_active: bool,
    },
    /// Foreground-process verification result from the off-shard blocking worker.
    ForegroundVerified {
        /// Target session ID.
        session_id: SessionId,
        /// OSC 7501 slot ID.
        id: String,
        /// Revision to match.
        changed: u64,
        /// Verification verdict.
        confirmed: Confirmed,
    },
    /// Gracefully tear down all sessions and exit the shard thread.
    Shutdown,
}

/// Mailbox queue paired with a `woken` flag under the same mutex so Windows `ConPtyBridge`
/// notifications can never be lost between draining output and waiting on `Condvar`.
#[derive(Default)]
pub(crate) struct ShardMailboxState {
    pub(crate) queue: VecDeque<ShardCmd>,
    pub(crate) woken: bool,
}

/// Platform-neutral wake handle for notifying a sleeping shard thread.
#[derive(Clone)]
enum ShardWake {
    #[cfg(unix)]
    Poller(PtyWaker),
    #[cfg(windows)]
    Cond(Arc<Mutex<ShardMailboxState>>, Arc<Condvar>),
}

impl ShardWake {
    fn wake(&self) {
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
struct ShardTelemetryAtomics {
    scrollback_bytes: AtomicU64,
    total_pty_bytes: AtomicU64,
}

/// Handle held by the Control Reactor to communicate with one Session Shard.
pub struct ShardHandle {
    mailbox: Arc<Mutex<ShardMailboxState>>,
    wake: ShardWake,
    load: Arc<AtomicUsize>,
    telemetry: Arc<ShardTelemetryAtomics>,
    thread: Option<JoinHandle<()>>,
}

impl ShardHandle {
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

    /// Pushes a Tier 0 command into this shard's mailbox and wakes the shard thread immediately.
    pub fn send(&self, cmd: ShardCmd) {
        {
            let mut state = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
            state.queue.push_back(cmd);
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
    /// WAL defaults for recorded sessions.
    pub wal_config: WalConfig,
    /// Idle delay before cooperative scrollback compression starts.
    pub idle_compress_after: Duration,
    /// Escalation delay after graceful `SIGHUP` before `SIGKILL`.
    pub kill_escalate_after: Duration,
    /// Cached `libghostty-vt` ABI hash for `EngineCheckpoint`.
    pub engine_abi_hash: [u8; 32],
}

/// Request sent to the shard's off-shard blocking worker to inspect a foreground PID's `argv`.
struct FgVerifyReq {
    session_id: SessionId,
    pgid: Option<u32>,
    id: String,
    changed: u64,
    app: String,
}

/// Spawns one Session Shard thread and returns its [`ShardHandle`].
pub fn spawn_shard(
    shard_idx: usize,
    shared: ShardShared,
    reactor_tx: mpsc::Sender<ReactorEvent>,
    wake_reactor: Arc<dyn Fn() + Send + Sync>,
) -> std::io::Result<ShardHandle> {
    let mailbox = Arc::new(Mutex::new(ShardMailboxState::default()));
    let load = Arc::new(AtomicUsize::new(0));
    let telemetry = Arc::new(ShardTelemetryAtomics::default());

    #[cfg(unix)]
    let (poller, wake) = {
        let poller = PtyPoller::new()?;
        let wake = ShardWake::Poller(poller.waker());
        (poller, wake)
    };
    #[cfg(windows)]
    let (cond, wake) = {
        let cv = Arc::new(Condvar::new());
        let wake = ShardWake::Cond(Arc::clone(&mailbox), Arc::clone(&cv));
        (cv, wake)
    };

    // Bounded off-shard blocking worker for OSC 7501 `sysinfo` foreground-argv verification.
    let (fg_tx, fg_rx) = mpsc::sync_channel::<FgVerifyReq>(FG_VERIFY_QUEUE_CAP);
    let fg_mailbox = Arc::clone(&mailbox);
    let fg_wake = wake.clone();
    let fg_worker = thread::Builder::new().name(format!("apack-fg-{shard_idx}")).spawn(move || {
        while let Ok(req) = fg_rx.recv() {
            let confirmed = match req.pgid.and_then(apack_os::tree::argv_for_pid) {
                Some(argv) => {
                    if argv_matches_app(&argv, &req.app) {
                        Confirmed::Matched
                    } else {
                        Confirmed::Mismatched
                    }
                }
                None => Confirmed::Unknown,
            };
            {
                let mut state = fg_mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.push_back(ShardCmd::ForegroundVerified {
                    session_id: req.session_id,
                    id: req.id,
                    changed: req.changed,
                    confirmed,
                });
                state.woken = true;
            }
            fg_wake.wake();
        }
    })?;

    let worker_mailbox = Arc::clone(&mailbox);
    let worker_load = Arc::clone(&load);
    let worker_telemetry = Arc::clone(&telemetry);
    #[cfg(windows)]
    let worker_wake = wake.clone();
    let thread = thread::Builder::new().name(format!("apack-shard-{shard_idx}")).spawn(move || {
        let mut runner = ShardRunner {
            shard_idx,
            shared,
            reactor_tx,
            wake_reactor,
            mailbox: worker_mailbox,
            load: worker_load,
            telemetry: worker_telemetry,
            #[cfg(windows)]
            self_wake: worker_wake,
            fg_tx: Some(fg_tx),
            fg_worker: Some(fg_worker),
            fg_inflight: HashSet::new(),
            sessions: HashMap::new(),
            #[cfg(unix)]
            poller,
            #[cfg(unix)]
            token_to_session: HashMap::new(),
            #[cfg(windows)]
            cond,
            #[cfg(unix)]
            read_buf: vec![0u8; PTY_READ_BUF_BYTES],
            #[cfg(unix)]
            write_scratch: Vec::new(),
            total_pty_bytes: 0,
            running: true,
        };
        runner.run();
    })?;

    Ok(ShardHandle { mailbox, wake, load, telemetry, thread: Some(thread) })
}

pub(crate) enum LivePty {
    #[cfg(unix)]
    Unix { master: PtyMaster, token: PollToken, eof: bool },
    #[cfg(windows)]
    Windows { bridge: ConPtyBridge, eof: bool },
}

pub(crate) struct LiveSession {
    pub(crate) machine: SessionMachine<GhosttyEngine>,
    pub(crate) pty: Option<LivePty>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    killer: Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
    tree: Option<TreeGuard>,
    pid: Option<SignalTarget>,
    kept: bool,
    rewound: bool,
    created_ms: u64,
    wal: Option<WalHandle>,
    rewind_reader: Option<WalReader>,
    rewind_at_ms: u64,
    last_activity: Instant,
    compressed_idle: bool,
    paused_by_stream: bool,
    paused_by_raw_flow: bool,
    terminating_since: Option<Instant>,
    escalated_sigkill: bool,
    /// View streams currently ready to accept the next `FrameUpdate` (1-slot lossless handshake).
    view_slot_free: HashSet<StreamKey>,
    /// Active `SUB_RAW_OUTPUT` subscribers (avoids sending `PublishRawOutput` when empty).
    raw_output_subs: HashSet<StreamKey>,
}

impl LiveSession {
    fn is_pty_paused(&self) -> bool {
        self.paused_by_stream || self.paused_by_raw_flow
    }

    fn summary(&self) -> SessionSummary {
        let foreground = match &self.pty {
            #[cfg(unix)]
            Some(LivePty::Unix { master, .. }) => master.foreground_process(),
            _ => None,
        };
        SessionSummary {
            id: self.machine.id(),
            size: self.machine.current_size(),
            alive: self.machine.is_alive(),
            kept: self.kept,
            rewound: self.rewound,
            title: self.machine.title().to_owned(),
            cwd: self.machine.cwd().map(ToOwned::to_owned),
            pid: self.pid,
            foreground,
            created_ms: self.created_ms,
        }
    }
}

pub(crate) struct ShardRunner {
    shard_idx: usize,
    shared: ShardShared,
    reactor_tx: mpsc::Sender<ReactorEvent>,
    wake_reactor: Arc<dyn Fn() + Send + Sync>,
    mailbox: Arc<Mutex<ShardMailboxState>>,
    load: Arc<AtomicUsize>,
    telemetry: Arc<ShardTelemetryAtomics>,
    #[cfg(windows)]
    self_wake: ShardWake,
    fg_tx: Option<mpsc::SyncSender<FgVerifyReq>>,
    fg_worker: Option<JoinHandle<()>>,
    fg_inflight: HashSet<SessionId>,
    pub(crate) sessions: HashMap<SessionId, LiveSession>,
    #[cfg(unix)]
    poller: PtyPoller,
    #[cfg(unix)]
    token_to_session: HashMap<PollToken, SessionId>,
    #[cfg(windows)]
    cond: Arc<Condvar>,
    #[cfg(unix)]
    read_buf: Vec<u8>,
    #[cfg(unix)]
    write_scratch: Vec<u8>,
    total_pty_bytes: u64,
    running: bool,
}

impl ShardRunner {
    fn run(&mut self) {
        #[cfg(unix)]
        let mut poll_events: Vec<PollEvent> = Vec::with_capacity(64);

        while self.running {
            // Tier 0: Drain all pending control/interactive commands first.
            self.drain_mailbox();
            if !self.running {
                break;
            }

            // Tier 1: Drain ready PTY output (round-robin, 256 KiB quantum per session) + PTY writes.
            #[cfg(windows)]
            self.drain_windows_ptys();

            // Tier 2: Session timers, child reap / kill escalation, cooperative idle compression.
            let now_ms = TimestampMs::from_millis(now_unix_ms());
            self.run_tier2(now_ms);
            self.refresh_telemetry_atomics();

            let mut timeout = Duration::from_secs(5);
            for sess in self.sessions.values() {
                let pty_eof = match &sess.pty {
                    #[cfg(unix)]
                    Some(LivePty::Unix { eof, .. }) => *eof,
                    #[cfg(windows)]
                    Some(LivePty::Windows { eof, .. }) => *eof,
                    None => false,
                };
                timeout = timeout.min(shard_session_wait_timeout(
                    sess.machine.next_deadline(),
                    now_ms,
                    sess.terminating_since.is_some(),
                    sess.is_pty_paused(),
                    pty_eof,
                    sess.compressed_idle,
                    sess.last_activity.elapsed(),
                    self.shared.idle_compress_after,
                ));
            }

            // Wait until next PTY readiness, mailbox wakeup, or earliest deadline.
            #[cfg(unix)]
            {
                // Skip blocking poll if new mailbox commands arrived during Tier 1/2.
                if !self.mailbox.lock().unwrap_or_else(|e| e.into_inner()).queue.is_empty() {
                    continue;
                }
                poll_events.clear();
                if self.poller.poll(&mut poll_events, Some(timeout)).is_ok() {
                    self.handle_unix_poll_events(&poll_events);
                }
            }

            #[cfg(windows)]
            wait_for_win_mailbox(&self.mailbox, &self.cond, timeout);
        }

        // Shutdown: tear down every remaining session cleanly, seal WALs, and stop fg worker.
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.force_drop_session(id);
        }
        self.load.store(0, Ordering::Relaxed);
        self.fg_tx.take();
        if let Some(worker) = self.fg_worker.take() {
            let _ = worker.join();
        }
    }

    fn emit(&self, event: ReactorEvent) {
        let _ = self.reactor_tx.send(event);
        (self.wake_reactor)();
    }

    fn drain_mailbox(&mut self) {
        while self.running {
            let cmd = {
                let mut state = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.pop_front()
            };
            let Some(cmd) = cmd else {
                break;
            };
            self.handle_cmd(cmd);
        }
    }

    fn handle_cmd(&mut self, cmd: ShardCmd) {
        match cmd {
            ShardCmd::Spawn { session_id, conn, req_id, spec } => {
                self.handle_spawn(session_id, conn, req_id, *spec);
            }
            ShardCmd::Rewind { session_id, conn, req_id, record_dir, at_ms } => {
                self.handle_rewind(session_id, conn, req_id, &record_dir, at_ms);
            }
            ShardCmd::Seek { session_id, conn, req_id, at_ms } => {
                self.handle_seek(session_id, conn, req_id, at_ms);
            }
            ShardCmd::Kill { session_id, conn, req_id } => {
                self.handle_kill(session_id, conn, req_id);
            }
            ShardCmd::Signal { session_id, conn, req_id, signal, scope } => {
                self.handle_signal(session_id, conn, req_id, signal, scope);
            }
            ShardCmd::Owns { session_id, conn, req_id, pid } => {
                self.handle_owns(session_id, conn, req_id, pid);
            }
            ShardCmd::SetKept { session_id, conn, req_id, kept } => {
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    if sess.rewound {
                        self.send_error(
                            conn,
                            StreamId::CONTROL,
                            req_id,
                            ErrorCode::Malformed,
                            "cannot keep rewound session",
                        );
                    } else {
                        sess.kept = kept;
                        self.emit(ReactorEvent::SessionKeptUpdated { session: session_id, kept });
                        self.send_ok(conn, StreamId::CONTROL, req_id);
                    }
                } else {
                    self.send_no_such_session(conn, req_id);
                }
            }
            ShardCmd::Wait { session_id, conn, req_id, condition, timeout_ms } => {
                let now = TimestampMs::from_millis(now_unix_ms());
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.machine.register_wait(conn, req_id, condition, timeout_ms, now, &mut effects);
                    self.apply_effects(session_id, effects);
                } else {
                    self.send_no_such_session(conn, req_id);
                }
            }
            ShardCmd::Attach { session_id, conn, req_id, stream_id, flags, initial_viewport } => {
                self.handle_attach(session_id, conn, req_id, stream_id, flags, initial_viewport);
            }
            ShardCmd::Detach { session_id, conn, stream_id } => {
                let key = (conn, stream_id);
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.view_slot_free.remove(&key);
                    sess.raw_output_subs.remove(&key);
                    sess.machine.detach_stream(key, &mut effects);
                    self.apply_effects(session_id, effects);
                }
            }
            ShardCmd::CancelConnWaiters { conn } => {
                for sess in self.sessions.values_mut() {
                    sess.machine.cancel_waiters_for_conn(conn);
                    sess.view_slot_free.retain(|&(c, _)| c != conn);
                    sess.raw_output_subs.retain(|&(c, _)| c != conn);
                }
            }
            ShardCmd::StreamMsg { session_id, conn, stream_id, req_id, msg } => {
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.machine.handle_stream_msg((conn, stream_id), req_id, msg, &mut effects);
                    self.apply_effects(session_id, effects);
                } else {
                    self.send_error(
                        conn,
                        stream_id.as_stream_id(),
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                    );
                }
            }
            ShardCmd::ViewSlotFree { session_id, conn, stream_id } => {
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.view_slot_free.insert((conn, stream_id));
                    self.try_publish_dirty_views(session_id);
                }
            }
            ShardCmd::SetPtyPaused { session_id, paused } => {
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.paused_by_raw_flow = paused;
                    self.sync_pty_interest(session_id);
                }
            }
            ShardCmd::ListSessions { list_id } => {
                let sessions = self.sessions.values().map(LiveSession::summary).collect();
                self.emit(ReactorEvent::SessionListPart { list_id, sessions });
            }
            ShardCmd::TeardownNonKept { pinned } => {
                let victims: Vec<SessionId> =
                    self.sessions.iter().filter(|(id, s)| !s.kept && !pinned.contains(id)).map(|(id, _)| *id).collect();
                for id in victims {
                    self.initiate_session_termination(id);
                }
            }
            ShardCmd::TeardownAll => {
                let victims: Vec<SessionId> = self.sessions.keys().copied().collect();
                for id in victims {
                    self.initiate_session_termination(id);
                }
            }
            ShardCmd::SessionUnpinned { session_id, broker_lease_active } => {
                if let Some(sess) = self.sessions.get(&session_id)
                    && (sess.rewound || (!sess.kept && !broker_lease_active))
                {
                    self.initiate_session_termination(session_id);
                }
            }
            ShardCmd::ForegroundVerified { session_id, id, changed, confirmed } => {
                self.fg_inflight.remove(&session_id);
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.machine.apply_status_confirmation(&id, changed, confirmed, &mut effects);
                    self.apply_effects(session_id, effects);
                }
            }
            ShardCmd::Shutdown => {
                self.running = false;
            }
        }
    }

    fn handle_spawn(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId, spec: SpawnSpec) {
        if spec.image_files {
            self.emit(ReactorEvent::SpawnOrRewindFailed);
            self.send_error(
                conn,
                StreamId::CONTROL,
                req_id,
                ErrorCode::Malformed,
                "image_files file-transmission loading is not enabled on this broker",
            );
            return;
        }
        let scrollback_lines = resolve_scrollback_lines(spec.scrollback_limit);
        let mut vt_cfg = GhosttyConfig::new(spec.size);
        vt_cfg.cell_px = spec.cell_px;
        vt_cfg.scrollback_lines = scrollback_lines;
        vt_cfg.image_store = Some(Arc::clone(&self.shared.image_store));

        let mut engine = match GhosttyEngine::new(vt_cfg) {
            Ok(e) => e,
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
        };

        // Optional dual-layer restoration BEFORE spawning the live shell.
        if let Some(restore_dir) = &spec.restore_from {
            if let Err(err) = restore_engine_from_dir(
                &mut engine,
                Path::new(restore_dir),
                &self.shared.wal_registry,
                &self.shared.engine_abi_hash,
            ) {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("restore failed: {err}"));
                return;
            }
            // Align restored VT geometry with the live PTY dimensions requested by the caller.
            if engine.meta().size != spec.size {
                let mut evs = Vec::new();
                engine.resize(spec.size, spec.cell_px, &mut evs);
            }
        }

        // Optional segmented WAL recording setup.
        let created_ms = now_unix_ms();
        let wal = if let Some(record_dir) = &spec.record_dir {
            let mut uuid = [0u8; 16];
            if let Err(err) = fill_random(&mut uuid) {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err);
                return;
            }
            match WalWriter::open_or_create(
                record_dir,
                uuid,
                created_ms,
                self.shared.wal_config,
                self.shared.wal_registry.clone(),
            ) {
                Ok(writer) => Some(WalWorker::spawn(writer)),
                Err(err) => {
                    self.emit(ReactorEvent::SpawnOrRewindFailed);
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Pty,
                        &format!("recording failed: {err}"),
                    );
                    return;
                }
            }
        } else {
            None
        };

        // Spawn OS PTY process. Never leak `AUTH_KEY_ENV` or `spec.marks_key` into child env!
        let (shell, args) = match spec.argv.split_first() {
            Some((prog, rest)) => (Some(prog.clone()), rest.to_vec()),
            None => (None, Vec::new()),
        };
        let pty_cfg = PtySpawnConfig {
            size: spec.size,
            cell_px: spec.cell_px,
            shell,
            args,
            env: spec.env,
            unset: vec![String::from(AUTH_KEY_ENV)],
            cwd: spec.cwd,
        };
        let mut spawned = match spawn_pty(&pty_cfg) {
            Ok(s) => s,
            Err(err) => {
                if let Some(w) = wal {
                    let _ = w.shutdown();
                }
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, err.error_code(), &err.to_string());
                return;
            }
        };

        let marks_mode = match spec.marks_key {
            Some(key) => MarksMode::Keyed(key),
            None => MarksMode::Claimed,
        };
        let machine = SessionMachine::new(session_id, engine, spec.resize_policy, marks_mode, spec.cell_px);

        // Capture initial KeyframeV2 as record 0 if recording is enabled.
        if let Some(handle) = &wal {
            let kf = capture_keyframe(&machine, &self.shared.engine_abi_hash);
            let _ = handle.start_segment(created_ms, kf);
        }

        // Register PTY with shard multiplexer; kill & reap child if registration fails.
        #[cfg(unix)]
        let live_pty = {
            let token = PollToken(session_id.get());
            if let Err(err) = self.poller.register(token, spawned.master.raw_fd(), Interest::READ) {
                if let Some(w) = wal {
                    let _ = w.shutdown();
                }
                let _ = deliver_signal(spawned.pid, SignalKind::Kill, SignalScope::Group);
                let _ = spawned.killer.kill();
                let _ = spawned.child.wait();
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
            self.token_to_session.insert(token, session_id);
            LivePty::Unix { master: spawned.master, token, eof: false }
        };

        #[cfg(windows)]
        let live_pty = {
            let wake = self.self_wake.clone();
            match ConPtyBridge::start(spawned.master, move || wake.wake()) {
                Ok(bridge) => LivePty::Windows { bridge, eof: false },
                Err(err) => {
                    if let Some(w) = wal {
                        let _ = w.shutdown();
                    }
                    drop(spawned.tree);
                    let _ = spawned.killer.kill();
                    let _ = spawned.child.wait();
                    self.emit(ReactorEvent::SpawnOrRewindFailed);
                    self.send_error(conn, StreamId::CONTROL, req_id, err.error_code(), &err.to_string());
                    return;
                }
            }
        };

        let pid_target = i32::try_from(spawned.pid).ok().and_then(SignalTarget::new);
        let kept = spec.kept;
        self.sessions.insert(
            session_id,
            LiveSession {
                machine,
                pty: Some(live_pty),
                child: Some(spawned.child),
                killer: Some(spawned.killer),
                tree: spawned.tree,
                pid: pid_target,
                kept,
                rewound: false,
                created_ms,
                wal,
                rewind_reader: None,
                rewind_at_ms: 0,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: false,
                paused_by_raw_flow: false,
                terminating_since: None,
                escalated_sigkill: false,
                view_slot_free: HashSet::new(),
                raw_output_subs: HashSet::new(),
            },
        );
        self.load.store(self.sessions.len(), Ordering::Relaxed);

        self.emit(ReactorEvent::SessionRegistered {
            session: session_id,
            shard_idx: self.shard_idx,
            rewound: false,
            kept,
        });
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Spawned { session: session_id, pid: pid_target },
        });
    }

    fn handle_rewind(
        &mut self,
        session_id: SessionId,
        conn: ConnId,
        req_id: RequestId,
        record_dir: &str,
        at_ms: Option<u64>,
    ) {
        let reader = match WalReader::open(record_dir, &self.shared.wal_registry) {
            Ok(r) => r,
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("rewind failed: {err}"));
                return;
            }
        };
        let Some((first_ms, last_ms)) = reader.span() else {
            self.emit(ReactorEvent::SpawnOrRewindFailed);
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "recording contains no segments");
            return;
        };
        let target_ms = at_ms.map_or(last_ms, |t| first_ms.saturating_add(t).min(last_ms));
        let duration_ms = last_ms.saturating_sub(first_ms);

        let slice = match reader.records_for_moment(target_ms) {
            Ok(Some(s)) => s,
            Ok(None) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "recording has no seek slice");
                return;
            }
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("rewind read failed: {err}"));
                return;
            }
        };

        let mut vt_cfg = GhosttyConfig::new(slice.keyframe.visual.size);
        vt_cfg.cell_px = slice.keyframe.visual.cell_px;
        vt_cfg.image_store = Some(Arc::clone(&self.shared.image_store));
        let mut engine = match GhosttyEngine::new(vt_cfg) {
            Ok(e) => e,
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
        };

        apply_seek_slice(&mut engine, &slice, &self.shared.engine_abi_hash);
        let machine = SessionMachine::new(
            session_id,
            engine,
            ResizePolicy::ExplicitOverride,
            MarksMode::Claimed,
            slice.keyframe.visual.cell_px,
        );
        let actual_offset_ms = target_ms.saturating_sub(first_ms);

        self.sessions.insert(
            session_id,
            LiveSession {
                machine,
                pty: None,
                child: None,
                killer: None,
                tree: None,
                pid: None,
                kept: false,
                rewound: true,
                created_ms: reader.meta().created_ms,
                wal: None,
                rewind_reader: Some(reader),
                rewind_at_ms: actual_offset_ms,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: false,
                paused_by_raw_flow: false,
                terminating_since: None,
                escalated_sigkill: false,
                view_slot_free: HashSet::new(),
                raw_output_subs: HashSet::new(),
            },
        );
        self.load.store(self.sessions.len(), Ordering::Relaxed);

        self.emit(ReactorEvent::SessionRegistered {
            session: session_id,
            shard_idx: self.shard_idx,
            rewound: true,
            kept: false,
        });
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Rewound { session: session_id, duration_ms, at_ms: actual_offset_ms },
        });
    }

    fn handle_seek(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId, at_ms: u64) {
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            self.send_no_such_session(conn, req_id);
            return;
        };
        let Some(reader) = &sess.rewind_reader else {
            self.send_error(
                conn,
                StreamId::CONTROL,
                req_id,
                ErrorCode::Malformed,
                "session is not a rewound recording",
            );
            return;
        };
        let Some((first_ms, last_ms)) = reader.span() else {
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "recording is empty");
            return;
        };
        let target_ms = first_ms.saturating_add(at_ms).min(last_ms);
        let slice = match reader.records_for_moment(target_ms) {
            Ok(Some(s)) => s,
            Ok(None) => {
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "seek target missing");
                return;
            }
            Err(err) => {
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("seek failed: {err}"));
                return;
            }
        };

        // Construct a clean VT engine on seek so ABI-mismatch fallback never appends onto old seek rows!
        let mut vt_cfg = GhosttyConfig::new(slice.keyframe.visual.size);
        vt_cfg.cell_px = slice.keyframe.visual.cell_px;
        vt_cfg.image_store = Some(Arc::clone(&self.shared.image_store));
        let mut fresh_engine = match GhosttyEngine::new(vt_cfg) {
            Ok(e) => e,
            Err(err) => {
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
        };
        apply_seek_slice(&mut fresh_engine, &slice, &self.shared.engine_abi_hash);
        *sess.machine.vt_mut() = fresh_engine;
        sess.machine.sync_vt_geometry(slice.keyframe.visual.cell_px);
        sess.machine.mark_all_views_full_sync();
        let actual_offset_ms = target_ms.saturating_sub(first_ms);
        sess.rewind_at_ms = actual_offset_ms;

        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Sought { at_ms: actual_offset_ms },
        });
        self.try_publish_dirty_views(session_id);
    }

    fn handle_kill(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId) {
        if !self.sessions.contains_key(&session_id) {
            self.send_no_such_session(conn, req_id);
            return;
        }
        self.initiate_session_termination(session_id);
        self.send_ok(conn, StreamId::CONTROL, req_id);
    }

    fn handle_signal(
        &mut self,
        session_id: SessionId,
        conn: ConnId,
        req_id: RequestId,
        signal: SignalKind,
        scope: SignalScope,
    ) {
        let Some(sess) = self.sessions.get(&session_id) else {
            self.send_no_such_session(conn, req_id);
            return;
        };
        let Some(pid) = sess.pid else {
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "session has no live process");
            return;
        };
        match deliver_signal(pid.pid() as u32, signal, scope) {
            Ok(()) => self.send_ok(conn, StreamId::CONTROL, req_id),
            Err(err) => self.send_error(conn, StreamId::CONTROL, req_id, err.error_code(), &err.to_string()),
        }
    }

    fn handle_owns(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId, pid: SignalTarget) {
        let Some(sess) = self.sessions.get(&session_id) else {
            self.send_no_such_session(conn, req_id);
            return;
        };
        let owned = match &sess.tree {
            Some(tree) => match tree.owns(pid.pid() as u32) {
                Ok(v) => v,
                Err(err) => {
                    self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                    return;
                }
            },
            None => false,
        };
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::OwnsReply { owned },
        });
    }

    fn handle_attach(
        &mut self,
        session_id: SessionId,
        conn: ConnId,
        req_id: RequestId,
        stream_id: NonZeroStreamId,
        flags: SubFlags,
        initial_viewport: Option<ViewportHint>,
    ) {
        let mut effects = Vec::new();
        let reply = {
            let Some(sess) = self.sessions.get_mut(&session_id) else {
                self.send_no_such_session(conn, req_id);
                return;
            };
            sess.machine.attach_stream((conn, stream_id), flags, initial_viewport, &mut effects);
            if flags.contains(SubFlags::VIEW) {
                sess.view_slot_free.insert((conn, stream_id));
            }
            if flags.contains(SubFlags::RAW_OUTPUT) {
                sess.raw_output_subs.insert((conn, stream_id));
            }
            let meta = sess.machine.vt().meta();
            let foreground = match &sess.pty {
                #[cfg(unix)]
                Some(LivePty::Unix { master, .. }) => master.foreground_process(),
                _ => None,
            };
            BrokerMsg::Attached {
                session: session_id,
                stream_id,
                size: meta.size,
                epoch: meta.epoch,
                first_active_line: meta.first_active_line,
                scrollback_lines: meta.scrollback_lines,
                foreground,
            }
        };
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: reply,
        });
        self.apply_effects(session_id, effects);
    }

    fn apply_effects(&mut self, session_id: SessionId, effects: Vec<SessionEffect>) {
        let mut views_dirty = false;
        let mut write_ready = false;

        for effect in effects {
            match effect {
                SessionEffect::Send { conn, stream_id, req_id, priority, msg } => {
                    self.emit(ReactorEvent::SendMsg { conn, stream_id, req_id, priority, msg });
                }
                SessionEffect::ResizePty { size, cell_px } => {
                    if let Some(sess) = self.sessions.get_mut(&session_id) {
                        match &sess.pty {
                            #[cfg(unix)]
                            Some(LivePty::Unix { master, .. }) => {
                                let _ = master.resize(size, cell_px);
                            }
                            #[cfg(windows)]
                            Some(LivePty::Windows { bridge, .. }) => {
                                let _ = bridge.resize(size, cell_px);
                            }
                            None => {}
                        }
                        if let Some(wal) = &sess.wal {
                            let _ = wal.write_resize(now_unix_ms(), size, cell_px);
                        }
                    }
                }
                SessionEffect::PtyWriteReady => {
                    write_ready = true;
                }
                SessionEffect::ViewsDirty => {
                    views_dirty = true;
                }
                SessionEffect::VerifyForeground { id, changed, app } => {
                    self.dispatch_verify_foreground(session_id, id, changed, app);
                }
                SessionEffect::FetchImageBlob { conn, stream_id, req_id, blob, offset, max_bytes } => {
                    let chunk_opt = {
                        let mut store = self.shared.image_store.lock().unwrap_or_else(|e| e.into_inner());
                        store.get_chunk(blob, offset, max_bytes)
                    };
                    let msg = match chunk_opt {
                        Some(chunk) => BrokerMsg::ImageBlobChunk {
                            blob: chunk.blob,
                            encoding: chunk.encoding,
                            total_bytes: chunk.total_bytes,
                            offset: chunk.offset,
                            data: chunk.data,
                        },
                        None => BrokerMsg::Error {
                            code: ErrorCode::ResourceLimit,
                            message: String::from("image blob evicted or unknown"),
                        },
                    };
                    self.emit(ReactorEvent::SendMsg {
                        conn,
                        stream_id: stream_id.as_stream_id(),
                        req_id,
                        priority: OutboundPriority::Bulk,
                        msg,
                    });
                }
                SessionEffect::SetPtyPaused(paused) => {
                    if let Some(sess) = self.sessions.get_mut(&session_id) {
                        sess.paused_by_stream = paused;
                        self.sync_pty_interest(session_id);
                    }
                }
            }
        }

        if write_ready {
            self.drain_pty_writes_for(session_id);
        }
        if views_dirty {
            self.try_publish_dirty_views(session_id);
        }
    }

    fn dispatch_verify_foreground(&mut self, session_id: SessionId, id: String, changed: u64, app: String) {
        let pgid = match self.sessions.get(&session_id).and_then(|s| s.pty.as_ref()) {
            #[cfg(unix)]
            Some(LivePty::Unix { master, .. }) => master.foreground_pgid(),
            _ => None,
        };
        // Dispatch off-shard onto the bounded foreground worker (never block hot shard on sysinfo!).
        if let Some(tx) = &self.fg_tx {
            self.fg_inflight.insert(session_id);
            if tx.try_send(FgVerifyReq { session_id, pgid, id, changed, app }).is_err() {
                self.fg_inflight.remove(&session_id);
            }
        }
    }

    fn try_publish_dirty_views(&mut self, session_id: SessionId) {
        let keys: Vec<StreamKey> = match self.sessions.get(&session_id) {
            Some(sess) => {
                sess.machine.view_subscribers().into_iter().filter(|k| sess.view_slot_free.contains(k)).collect()
            }
            None => return,
        };
        for key in keys {
            let mut emitted_frame: Option<Box<FrameUpdate>> = None;
            let published = match self.sessions.get_mut(&session_id) {
                Some(sess) => sess.machine.try_publish_view(key, |frame| {
                    emitted_frame = Some(frame);
                    true
                }),
                None => return,
            };
            if published && let Some(frame) = emitted_frame {
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.view_slot_free.remove(&key);
                }
                self.emit(ReactorEvent::PublishFrame { conn: key.0, stream_id: key.1, session: session_id, frame });
            }
        }
    }

    #[cfg(unix)]
    fn handle_unix_poll_events(&mut self, events: &[PollEvent]) {
        for ev in events {
            let Some(&session_id) = self.token_to_session.get(&ev.token) else {
                continue;
            };
            if ev.writable {
                self.drain_pty_writes_for(session_id);
            }
            if ev.readable || ev.hangup {
                self.read_unix_pty_quantum(session_id);
            }
            if ev.hangup {
                self.try_reap_session(session_id);
            }
        }
    }

    #[cfg(unix)]
    fn read_unix_pty_quantum(&mut self, session_id: SessionId) {
        let mut budget = PTY_READ_QUANTUM_BYTES;
        let mut buf = std::mem::take(&mut self.read_buf);
        while budget != 0 {
            let is_paused = self.sessions.get(&session_id).is_none_or(LiveSession::is_pty_paused);
            if is_paused {
                break;
            }
            let read_res = match self.sessions.get(&session_id).and_then(|s| s.pty.as_ref()) {
                Some(LivePty::Unix { master, eof: false, .. }) => {
                    let cap = budget.min(buf.len());
                    master.read_nonblocking(&mut buf[..cap])
                }
                _ => break,
            };
            match read_res {
                Ok(NonBlockingRead::Bytes(n)) => {
                    budget = budget.saturating_sub(n);
                    self.ingest_pty_bytes(session_id, &buf[..n]);
                }
                Ok(NonBlockingRead::WouldBlock) => break,
                Ok(NonBlockingRead::Eof) | Err(_) => {
                    if let Some(sess) = self.sessions.get_mut(&session_id)
                        && let Some(LivePty::Unix { token, eof, .. }) = sess.pty.as_mut()
                    {
                        *eof = true;
                        let tok = *token;
                        let _ = self.poller.deregister(tok);
                        self.token_to_session.remove(&tok);
                    }
                    self.read_buf = buf;
                    self.try_reap_session(session_id);
                    return;
                }
            }
        }
        self.read_buf = buf;
    }

    pub(crate) fn ingest_pty_bytes(&mut self, session_id: SessionId, bytes: &[u8]) {
        self.total_pty_bytes = self.total_pty_bytes.saturating_add(bytes.len() as u64);
        let now_ms = now_unix_ms();
        let mut effects = Vec::new();
        let mut need_rollover = false;
        let mut has_raw_subs = false;

        if let Some(sess) = self.sessions.get_mut(&session_id) {
            sess.last_activity = Instant::now();
            sess.compressed_idle = false;
            has_raw_subs = !sess.raw_output_subs.is_empty();
            sess.machine.on_pty_output(bytes, TimestampMs::from_millis(now_ms), &mut effects);
            let at_ground = sess.machine.vt().at_ground();
            if let Some(wal) = &sess.wal {
                let _ = wal.write_output(now_ms, bytes.to_vec(), at_ground);
                if at_ground && wal.take_rollover_needed() {
                    need_rollover = true;
                }
            }
            if need_rollover && let Some(wal) = &sess.wal {
                let kf = capture_keyframe(&sess.machine, &self.shared.engine_abi_hash);
                let _ = wal.start_segment(now_ms, kf);
            }
        }
        if has_raw_subs {
            self.emit(ReactorEvent::PublishRawOutput { session: session_id, bytes: bytes.to_vec() });
        }
        self.apply_effects(session_id, effects);
    }

    fn drain_pty_writes_for(&mut self, session_id: SessionId) {
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        #[cfg(unix)]
        {
            while sess.machine.pty_write_len() != 0 {
                sess.machine.peek_pty_write(PTY_WRITE_CHUNK_BYTES, &mut self.write_scratch);
                let written = match &sess.pty {
                    Some(LivePty::Unix { master, eof: false, .. }) => master.write_nonblocking(&self.write_scratch),
                    _ => break,
                };
                match written {
                    Ok(0) => break, // EAGAIN -> arm writable interest below
                    Ok(n) => sess.machine.consume_pty_write(n),
                    Err(_) => break,
                }
            }
            self.sync_pty_interest(session_id);
        }
        #[cfg(windows)]
        drain_windows_pty_writes(sess);
    }

    fn sync_pty_interest(&mut self, session_id: SessionId) {
        #[cfg(unix)]
        {
            let Some(sess) = self.sessions.get(&session_id) else {
                return;
            };
            if let Some(LivePty::Unix { token, eof: false, .. }) = &sess.pty {
                let interest =
                    Interest { readable: !sess.is_pty_paused(), writable: sess.machine.pty_write_len() != 0 };
                let _ = self.poller.modify(*token, interest);
            }
        }
        #[cfg(windows)]
        {
            if let Some(sess) = self.sessions.get(&session_id)
                && let Some(LivePty::Windows { bridge, .. }) = &sess.pty
            {
                bridge.set_paused(sess.is_pty_paused());
            }
        }
    }

    fn initiate_session_termination(&mut self, session_id: SessionId) {
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if sess.rewound {
            self.finish_and_remove_session(session_id, None, None);
            return;
        }
        if sess.terminating_since.is_some() {
            return;
        }
        sess.terminating_since = Some(Instant::now());
        // Unpause PTY reads during teardown so macOS/BSD kernel `ttydrain()` cannot wedge an
        // exiting child in `?Es` state waiting for unread master PTY bytes!
        sess.paused_by_stream = false;
        sess.paused_by_raw_flow = false;
        #[cfg(unix)]
        let raw_pid = sess.pid.map(|p| p.pid() as u32);
        self.sync_pty_interest(session_id);

        #[cfg(unix)]
        {
            if let Some(raw) = raw_pid {
                // Signal non-leader descendants WHILE the leader is still alive, then SIGHUP leader group!
                hang_up_session_descendants(&[raw]);
                let _ = deliver_signal(raw, SignalKind::Hup, SignalScope::Group);
            }
        }
        #[cfg(windows)]
        {
            if let Some(sess) = self.sessions.get_mut(&session_id) {
                // Dropping TreeGuard closes the Windows Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`),
                // terminating the entire process tree atomically; close ConPTY master asynchronously.
                sess.tree = None;
                if let Some(killer) = sess.killer.as_mut() {
                    let _ = killer.kill();
                }
                if let Some(LivePty::Windows { bridge, .. }) = &sess.pty {
                    bridge.close_master_async();
                }
            }
        }

        self.try_reap_session(session_id);
    }

    pub(crate) fn try_reap_session(&mut self, session_id: SessionId) {
        let status_opt = {
            let Some(sess) = self.sessions.get_mut(&session_id) else {
                return;
            };
            let Some(child) = sess.child.as_mut() else {
                return;
            };
            match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => None,
            }
        };

        let Some(status) = status_opt else {
            return;
        };

        let (code, signal) = parse_exit_status(&status);
        self.finish_and_remove_session(session_id, code, signal);
    }

    fn finish_and_remove_session(&mut self, session_id: SessionId, code: Option<i32>, signal: Option<String>) {
        // Drain any tail output still buffered in the PTY before emitting `Exited`!
        #[cfg(unix)]
        {
            let mut buf = std::mem::take(&mut self.read_buf);
            let mut chunks = Vec::new();
            if let Some(LivePty::Unix { master, eof: false, .. }) =
                self.sessions.get(&session_id).and_then(|s| s.pty.as_ref())
            {
                drain_unix_pty_nonblocking(master, &mut buf, PTY_READ_QUANTUM_BYTES, |chunk| {
                    chunks.push(chunk.to_vec());
                });
            }
            self.read_buf = buf;
            for chunk in chunks {
                self.ingest_pty_bytes(session_id, &chunk);
            }
        }
        #[cfg(windows)]
        {
            let mut chunks = Vec::new();
            if let Some(LivePty::Windows { bridge, eof: false }) =
                self.sessions.get(&session_id).and_then(|s| s.pty.as_ref())
            {
                let _ = crate::shard_windows::drain_conpty_quantum(bridge, PTY_READ_QUANTUM_BYTES, |b| {
                    chunks.push(b.to_vec())
                });
            }
            for chunk in chunks {
                self.ingest_pty_bytes(session_id, &chunk);
            }
        }

        let mut effects = Vec::new();
        if let Some(mut sess) = self.sessions.remove(&session_id) {
            #[cfg(unix)]
            if let Some(LivePty::Unix { token, .. }) = sess.pty {
                let _ = self.poller.deregister(token);
                self.token_to_session.remove(&token);
            }
            #[cfg(windows)]
            if let Some(LivePty::Windows { ref bridge, .. }) = sess.pty {
                bridge.close_master_async();
            }
            if let Some(wal) = sess.wal.take() {
                let _ = wal.seal();
                let _ = wal.shutdown();
            }
            let now = TimestampMs::from_millis(now_unix_ms());
            if !sess.rewound {
                sess.machine.on_process_exit(code, signal, now, &mut effects);
            }
            drop(sess);
        }
        self.load.store(self.sessions.len(), Ordering::Relaxed);
        self.apply_effects(session_id, effects);
        self.emit(ReactorEvent::SessionEnded { session: session_id });
    }

    fn force_drop_session(&mut self, session_id: SessionId) {
        if let Some(mut sess) = self.sessions.remove(&session_id) {
            #[cfg(unix)]
            {
                if let Some(LivePty::Unix { token, .. }) = sess.pty {
                    let _ = self.poller.deregister(token);
                    self.token_to_session.remove(&token);
                }
                if let Some(pid) = sess.pid {
                    let raw = pid.pid() as u32;
                    // SIGKILL non-leader descendants BEFORE killing the leader!
                    kill_session_descendants(&[raw]);
                    let _ = deliver_signal(raw, SignalKind::Kill, SignalScope::Group);
                }
            }
            #[cfg(windows)]
            {
                sess.tree = None;
                if let Some(LivePty::Windows { ref bridge, .. }) = sess.pty {
                    bridge.close_master_async();
                }
            }
            if let Some(mut killer) = sess.killer.take() {
                let _ = killer.kill();
            }
            if let Some(wal) = sess.wal.take() {
                let _ = wal.seal();
                let _ = wal.shutdown();
            }
        }
    }

    fn run_tier2(&mut self, now: TimestampMs) {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        let now_instant = Instant::now();

        for id in ids {
            // 1. Child reap & 2-second SIGKILL escalation BEFORE firing timers so tail PTY bytes
            //    from an exited child satisfy pending `Wait` conditions before any deadline expires.
            let is_live = self.sessions.get(&id).is_some_and(|s| s.child.is_some());
            if is_live {
                if let Some(sess) = self.sessions.get_mut(&id)
                    && let Some(since) = sess.terminating_since
                    && !sess.escalated_sigkill
                    && now_instant.saturating_duration_since(since) >= self.shared.kill_escalate_after
                {
                    sess.escalated_sigkill = true;
                    if let Some(pid) = sess.pid {
                        let raw = pid.pid() as u32;
                        // SIGKILL non-leader descendants BEFORE killing the leader!
                        kill_session_descendants(&[raw]);
                        let _ = deliver_signal(raw, SignalKind::Kill, SignalScope::Group);
                    }
                    if let Some(killer) = sess.killer.as_mut() {
                        let _ = killer.kill();
                    }
                }
                self.try_reap_session(id);
            }

            // 2. Session timers (DEC 2026 hold cap, search debounce, Wait deadlines).
            let mut effects = Vec::new();
            if let Some(sess) = self.sessions.get_mut(&id) {
                sess.machine.poll_timers(now, &mut effects);
            }
            self.apply_effects(id, effects);

            // 3. Cooperative idle scrollback compression (4 pages/turn after idle_compress_after).
            if let Some(sess) = self.sessions.get_mut(&id) {
                maybe_compress_idle(
                    &mut sess.compressed_idle,
                    now_instant.saturating_duration_since(sess.last_activity),
                    self.shared.idle_compress_after,
                    || sess.machine.vt_mut().compress_step(COMPRESS_PAGES_PER_TURN),
                );
            }
        }
    }

    fn refresh_telemetry_atomics(&self) {
        let scrollback_bytes: u64 = self.sessions.values().map(|s| s.machine.vt().scrollback_bytes()).sum();
        self.telemetry.scrollback_bytes.store(scrollback_bytes, Ordering::Relaxed);
        self.telemetry.total_pty_bytes.store(self.total_pty_bytes, Ordering::Relaxed);
    }

    fn send_ok(&self, conn: ConnId, stream_id: StreamId, req_id: RequestId) {
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Ok,
        });
    }

    fn send_no_such_session(&self, conn: ConnId, req_id: RequestId) {
        self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::NoSuchSession, "no such session");
    }

    fn send_error(&self, conn: ConnId, stream_id: StreamId, req_id: RequestId, code: ErrorCode, message: &str) {
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Error { code, message: message.to_owned() },
        });
    }
}

/// Drains up to `budget` bytes of immediately available non-blocking Unix PTY output.
#[cfg(unix)]
pub(crate) fn drain_unix_pty_nonblocking(
    master: &PtyMaster,
    buf: &mut [u8],
    mut budget: usize,
    mut on_bytes: impl FnMut(&[u8]),
) {
    while budget != 0 {
        let cap = budget.min(buf.len());
        match master.read_nonblocking(&mut buf[..cap]) {
            Ok(NonBlockingRead::Bytes(n)) => {
                budget = budget.saturating_sub(n);
                on_bytes(&buf[..n]);
            }
            _ => break,
        }
    }
}

/// Resolves `SpawnSpec::scrollback_limit` (`0` selects [`DEFAULT_SCROLLBACK_LINES`]).
pub(crate) fn resolve_scrollback_lines(limit: u32) -> usize {
    if limit == 0 { DEFAULT_SCROLLBACK_LINES } else { limit as usize }
}

/// Runs one cooperative idle-compression step once `idle_elapsed >= idle_after`.
pub(crate) fn maybe_compress_idle(
    compressed_idle: &mut bool,
    idle_elapsed: Duration,
    idle_after: Duration,
    compress_fn: impl FnOnce() -> apack_core::vt::CompressStep,
) {
    if !*compressed_idle && idle_elapsed >= idle_after && compress_fn() == apack_core::vt::CompressStep::Done {
        *compressed_idle = true;
    }
}

/// Computes the next shard wait timeout contribution for a single session.
#[allow(clippy::too_many_arguments)]
pub(crate) fn shard_session_wait_timeout(
    next_deadline: Option<TimestampMs>,
    now: TimestampMs,
    terminating: bool,
    pty_paused: bool,
    pty_eof: bool,
    compressed_idle: bool,
    idle_elapsed: Duration,
    idle_compress_after: Duration,
) -> Duration {
    let mut timeout = Duration::from_secs(5);
    if let Some(dl) = next_deadline {
        timeout = timeout.min(Duration::from_millis(dl.saturating_sub(now)));
    }
    if terminating || pty_paused || pty_eof {
        timeout = timeout.min(TERMINATE_POLL_INTERVAL);
    } else if !compressed_idle {
        if idle_elapsed >= idle_compress_after {
            timeout = timeout.min(Duration::from_millis(5));
        } else {
            timeout = timeout.min(idle_compress_after - idle_elapsed);
        }
    }
    timeout
}

/// Captures a dual-layer [`KeyframeV2`] (broker-owned [`VisualKeyframe`] + ABI-hashed Ghostty snapshot).
pub(crate) fn capture_keyframe(machine: &SessionMachine<GhosttyEngine>, abi_hash: &[u8; 32]) -> KeyframeV2 {
    let vt = machine.vt();
    let meta = vt.meta();
    let mut primary_rows = Vec::new();
    let hist_take = meta.scrollback_lines.min(MAX_KEYFRAME_HISTORY_ROWS as u64);
    if hist_take != 0 {
        let from_line = meta.first_active_line.saturating_sub(hist_take);
        vt.extract_history_rows(from_line, hist_take as u32, &mut primary_rows);
    }
    let vp_indices: Vec<u16> = (0..meta.size.rows.get()).collect();
    vt.extract_viewport_rows(&vp_indices, &mut primary_rows);

    let visual = VisualKeyframe {
        size: meta.size,
        cell_px: machine.current_cell_px(),
        active_alt_screen: meta.modes.alt_screen,
        cursor: meta.cursor,
        modes: meta.modes,
        palette: vt.palette_overrides(),
        primary_rows,
        alt_rows: Vec::new(),
        images: vt.images(),
        marks_key: machine.marks_key(),
        marks_last_seq: machine.marks_last_seq(),
        marks_refused: machine.marks_refused(),
        commands: machine.commands(),
    };

    let engine = vt
        .export_snapshot(MAX_ENGINE_SNAPSHOT_BYTES)
        .ok()
        .map(|snapshot_bytes| EngineCheckpoint { engine_abi_hash: *abi_hash, snapshot_bytes });

    KeyframeV2 { visual, engine }
}

fn restore_engine_from_dir(
    engine: &mut GhosttyEngine,
    dir: &Path,
    registry: &RecordingRegistry,
    abi_hash: &[u8; 32],
) -> Result<(), apack_wal::WalError> {
    let reader = WalReader::open(dir, registry)?;
    let Some(slice) = reader.latest_segment_records()? else {
        return Ok(());
    };
    apply_seek_slice(engine, &slice, abi_hash);
    Ok(())
}

pub(crate) fn apply_seek_slice(engine: &mut GhosttyEngine, slice: &apack_wal::SeekSlice, abi_hash: &[u8; 32]) {
    let mut events = Vec::new();
    let mut restored_lossless = false;
    if let Some(snap) = slice.keyframe.engine_snapshot_if_compatible(abi_hash)
        && engine.restore_snapshot(snap).is_ok()
    {
        restored_lossless = true;
    }
    if !restored_lossless {
        // Visual fallback when `libghostty-vt` ABI hash changed across an upgrade:
        // resize to visual geometry and reconstruct visible text lines cleanly.
        engine.resize(slice.keyframe.visual.size, slice.keyframe.visual.cell_px, &mut events);
        for (idx, row) in slice.keyframe.visual.primary_rows.iter().enumerate() {
            if idx > 0 {
                engine.feed(b"\r\n", &mut events);
            }
            if !row.text.is_empty() {
                engine.feed(row.text.as_bytes(), &mut events);
            }
        }
    }
    for timed in &slice.tail_records {
        events.clear();
        match &timed.record {
            WalRecord::Output(bytes) => engine.feed(bytes, &mut events),
            WalRecord::Resize { size, cell_px } => engine.resize(*size, *cell_px, &mut events),
            WalRecord::Keyframe(_) | WalRecord::Marks { .. } | WalRecord::Gap { .. } => {}
        }
    }
}

pub(crate) fn parse_exit_status(status: &portable_pty::ExitStatus) -> (Option<i32>, Option<String>) {
    if let Some(desc) = status.signal() {
        (None, Some(signal_name_from_description(desc)))
    } else {
        (Some(status.exit_code() as i32), None)
    }
}

pub(crate) fn argv_matches_app(argv: &[String], app: &str) -> bool {
    let needle = app.trim();
    if needle.is_empty() {
        return false;
    }
    argv.iter().any(|arg| {
        let base = Path::new(arg).file_name().and_then(|s| s.to_str()).unwrap_or(arg.as_str());
        base.eq_ignore_ascii_case(needle)
    })
}
