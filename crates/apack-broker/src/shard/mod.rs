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

mod commands;
mod images;
mod lifecycle;
mod protocol;
mod pty_io;
mod replay;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
#[cfg(windows)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use apack_core::flow::OutboundPriority;
use apack_core::lease::ConnId;
use apack_core::session::{SessionMachine, StreamAttachmentId};
use apack_core::status::confirm_foreground_app;
use apack_core::time::TimestampMs;
use apack_os::image_sources::{Scanner as ImageScanner, Worker as ImageWorker};
#[cfg(unix)]
use apack_os::pty::PtyMaster;
#[cfg(unix)]
use apack_os::pty_poller::{PollEvent, PollToken, PtyPoller};
#[cfg(windows)]
use apack_os::pty_windows::ConPtyBridge;
use apack_os::tree::TreeGuard;
use apack_proto::messages::{BrokerMsg, SessionSummary};
use apack_proto::types::{BlobHash, ErrorCode, RequestId, SessionId, SignalTarget, StreamId};
use apack_vt_ghostty::GhosttyEngine;
use apack_wal::{WalHandle, WalReader};

use crate::now_unix_ms;
#[cfg(windows)]
use crate::shard_windows::{clamp_win_write_timeout, maybe_wait_win_mailbox};

#[cfg(test)]
pub(crate) use commands::resolve_scrollback_lines;
#[cfg(test)]
pub(crate) use images::should_forward_probe_reply;
pub(crate) use lifecycle::shard_session_wait_timeout;
#[cfg(test)]
pub(crate) use lifecycle::{maybe_compress_idle, parse_exit_status};
#[cfg(test)]
pub(crate) use protocol::{RAW_OUTPUT_SESSION_LOW_WATER, RAW_OUTPUT_SHARD_LOW_WATER, crossed_raw_low_water};
pub use protocol::{ReactorEvent, ShardCmd, ShardHandle, ShardShared};
pub(crate) use protocol::{ShardCmdPermit, ShardMailboxState, should_notify_raw_drained, should_pause_raw_inflight};
#[cfg(all(test, unix))]
pub(crate) use pty_io::{drain_unix_pty_nonblocking, unix_pty_interest};
#[cfg(test)]
pub(crate) use replay::{build_keyframe_engine, capture_keyframe, hydrate_slice_blobs, restore_machine_from_slice};

use images::PendingImageLoad;
use protocol::{FgVerifyReq, ShardTelemetryAtomics, ShardWake};

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
            let argv = req.pgid.and_then(apack_os::tree::argv_for_pid);
            let results: Vec<(String, u64, apack_proto::types::Confirmed)> = req
                .items
                .into_iter()
                .map(|(id, changed, app)| {
                    let confirmed = confirm_foreground_app(&app, argv.as_deref());
                    (id, changed, confirmed)
                })
                .collect();
            {
                let mut state = fg_mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.push_back((
                    ShardCmd::ForegroundVerified { session_id: req.session_id, results, from_worker: true },
                    None,
                ));
                state.woken = true;
            }
            fg_wake.wake();
        }
    })?;

    let worker_mailbox = Arc::clone(&mailbox);
    let worker_load = Arc::clone(&load);
    let worker_telemetry = Arc::clone(&telemetry);
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
            self_wake: worker_wake,
            fg_tx: Some(fg_tx),
            fg_worker: Some(fg_worker),
            fg_inflight: HashSet::new(),
            fg_pending: HashMap::new(),
            raw_output_inflight: Arc::new(AtomicUsize::new(0)),
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
            tearing_down_all: false,
        };
        runner.run();
    })?;

    Ok(ShardHandle { mailbox, wake, load, telemetry, thread: Some(thread) })
}

pub(crate) enum LivePty {
    #[cfg(unix)]
    Unix { master: PtyMaster, token: PollToken, eof: bool, write_broken: bool },
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
    pending_wal_rollover: bool,
    /// Last `VtGenerations::images` observed when syncing image blobs to `wal`.
    wal_images_gen: u64,
    /// Content hashes already persisted to `<record_dir>/blobs/` during this session.
    wal_written_blobs: HashSet<BlobHash>,
    rewind_reader: Option<WalReader>,
    rewind_at_ms: u64,
    last_activity: Instant,
    compressed_idle: bool,
    paused_by_stream: bool,
    paused_by_raw_flow: bool,
    paused_by_raw_inflight: bool,
    raw_output_inflight: Arc<AtomicUsize>,
    terminating_since: Option<Instant>,
    escalated_sigkill: bool,
    /// View attachments currently ready to accept the next `FrameUpdate` (1-slot lossless handshake).
    view_slot_free: HashSet<StreamAttachmentId>,
    /// Active `SUB_RAW_OUTPUT` subscriber attachments (avoids sending `PublishRawOutput` when empty).
    raw_output_subs: HashSet<StreamAttachmentId>,
    /// Streaming Kitty graphics APC scanner when `SpawnSpec::image_files` is enabled.
    image_scanner: Option<ImageScanner>,
    /// Per-session off-shard single-flight worker with quiet timeout & stuck-mount circuit breaker.
    image_worker: Option<Arc<ImageWorker>>,
    /// Monotonic ticket invalidating stale off-shard image completions on cancel/teardown.
    image_ticket: u64,
    /// In-flight off-shard image read pausing PTY ingestion until completion or termination.
    pending_image: Option<PendingImageLoad>,
    /// Unconsumed PTY bytes queued behind `pending_image` to preserve exact byte-stream ordering.
    pending_image_tail: Vec<u8>,
    /// Cached natural exit status deferred until any in-flight `pending_image` finishes.
    reaped_exit: Option<(Option<i32>, Option<String>)>,
}

impl LiveSession {
    pub(crate) fn is_pty_paused(&self) -> bool {
        self.paused_by_stream || self.paused_by_raw_flow || self.paused_by_raw_inflight || self.pending_image.is_some()
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
    self_wake: ShardWake,
    fg_tx: Option<mpsc::SyncSender<FgVerifyReq>>,
    fg_worker: Option<JoinHandle<()>>,
    fg_inflight: HashSet<SessionId>,
    fg_pending: HashMap<SessionId, BTreeMap<String, (u64, String)>>,
    raw_output_inflight: Arc<AtomicUsize>,
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
    tearing_down_all: bool,
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
            let win_exhausted_quantum = self.drain_windows_ptys(PTY_READ_QUANTUM_BYTES);

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
                #[cfg(windows)]
                {
                    timeout = clamp_win_write_timeout(timeout, sess.machine.pty_write_len());
                }
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
            maybe_wait_win_mailbox(win_exhausted_quantum, &self.mailbox, &self.cond, timeout);
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
            let entry = {
                let mut state = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
                state.queue.pop_front()
            };
            let Some((cmd, _permit)) = entry else {
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
            ShardCmd::Attach { session_id, conn, req_id, stream_id, attachment_id, flags, initial_viewport } => {
                self.handle_attach(session_id, conn, req_id, stream_id, attachment_id, flags, initial_viewport);
            }
            ShardCmd::Detach { session_id, conn, stream_id, attachment_id } => {
                let key = (conn, stream_id);
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.view_slot_free.remove(&attachment_id);
                    sess.raw_output_subs.remove(&attachment_id);
                    if sess.machine.attachment_of(key) == Some(attachment_id) {
                        sess.machine.detach_stream(key, &mut effects);
                        self.apply_effects(session_id, effects);
                    }
                }
            }
            ShardCmd::CancelConnWaiters { conn } => {
                for sess in self.sessions.values_mut() {
                    sess.machine.cancel_waiters_for_conn(conn);
                }
            }
            ShardCmd::StreamMsg { session_id, conn, stream_id, attachment_id, req_id, msg } => {
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.machine.handle_stream_msg((conn, stream_id), attachment_id, req_id, msg, &mut effects);
                    self.apply_effects(session_id, effects);
                } else {
                    self.send_stream_error(
                        conn,
                        stream_id.as_stream_id(),
                        Some(attachment_id),
                        req_id,
                        ErrorCode::NoSuchSession,
                        "no such session",
                    );
                }
            }
            ShardCmd::ViewSlotFree { session_id, conn, stream_id, attachment_id } => {
                if let Some(sess) = self.sessions.get_mut(&session_id)
                    && sess.machine.attachment_of((conn, stream_id)) == Some(attachment_id)
                {
                    sess.view_slot_free.insert(attachment_id);
                    self.try_publish_dirty_views(session_id);
                }
            }
            ShardCmd::RawOutputDrained => {
                let shard_bytes = self.raw_output_inflight.load(Ordering::Acquire);
                let to_resume: Vec<SessionId> = self
                    .sessions
                    .iter()
                    .filter_map(|(&id, s)| {
                        (s.paused_by_raw_inflight
                            && !should_pause_raw_inflight(s.raw_output_inflight.load(Ordering::Acquire), shard_bytes))
                        .then_some(id)
                    })
                    .collect();
                for id in to_resume {
                    if let Some(sess) = self.sessions.get_mut(&id) {
                        sess.paused_by_raw_inflight = false;
                    }
                    self.sync_pty_interest(id);
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
                self.tearing_down_all = true;
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
            ShardCmd::ForegroundVerified { session_id, results, from_worker } => {
                if from_worker {
                    self.fg_inflight.remove(&session_id);
                }
                let mut effects = Vec::new();
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    for (id, changed, confirmed) in results {
                        sess.machine.apply_status_confirmation(&id, changed, confirmed, &mut effects);
                    }
                    self.apply_effects(session_id, effects);
                }
                if from_worker {
                    self.flush_all_pending_fg_verify();
                }
            }
            ShardCmd::ImageSourceLoaded { session_id, ticket, loaded } => {
                self.handle_image_source_loaded(session_id, ticket, loaded);
            }
            ShardCmd::Shutdown => {
                self.running = false;
            }
        }
    }

    fn send_ok(&self, conn: ConnId, stream_id: StreamId, req_id: RequestId) {
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id,
            attachment: None,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Ok,
        });
    }

    fn send_no_such_session(&self, conn: ConnId, req_id: RequestId) {
        self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::NoSuchSession, "no such session");
    }

    fn send_error(&self, conn: ConnId, stream_id: StreamId, req_id: RequestId, code: ErrorCode, message: &str) {
        self.send_stream_error(conn, stream_id, None, req_id, code, message);
    }

    fn send_stream_error(
        &self,
        conn: ConnId,
        stream_id: StreamId,
        attachment: Option<StreamAttachmentId>,
        req_id: RequestId,
        code: ErrorCode,
        message: &str,
    ) {
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id,
            attachment,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Error { code, message: message.to_owned() },
        });
    }
}
