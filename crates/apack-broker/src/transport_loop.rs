//! Single-threaded Control & Transport Reactor driving sans-IO [`BrokerCore`].
//!
//! - On **Unix**, one [`apack_os::pty_poller::PtyPoller`] multiplexes the non-blocking local socket
//!   listener AND every accepted client connection fd (`writev` scatter-gather writes, zero
//!   per-connection threads!).
//! - On **Windows**, a thin accept thread and per-connection bounded reader/writer threads bridge
//!   named-pipe `Stream`s into the exact same single-threaded `BrokerCore` loop (keeping the
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` listener alive during shutdown).

#[cfg(unix)]
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(unix)]
use std::io::{IoSlice, Read};
#[cfg(windows)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use apack_core::broker::{
    BrokerAction, BrokerCore, CloseReason, FrameStallReason, SessionLifetime, ShardTelemetry, StreamAttachmentId,
    estimate_frame_approx_bytes,
};
use apack_core::flow::OutboundPriority;
use apack_core::lease::ConnId;
use apack_core::time::TimestampMs;
#[cfg(unix)]
use apack_os::pty_poller::{Interest, PollEvent, PollToken, PtyPoller};
#[cfg(unix)]
use apack_os::transport::{
    hang_up, is_fd_exhaustion_error, listener_fd, set_listener_nonblocking, set_stream_nonblocking, stream_fd,
    verify_peer_is_current_user, write_vectored_some,
};
use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate, SessionSummary};
use apack_proto::types::{ErrorCode, NonZeroStreamId, RequestId, SessionId, StreamId};
use apack_vt_ghostty::ImageStore;
use interprocess::local_socket::Listener;
#[cfg(unix)]
use interprocess::local_socket::Stream;
#[cfg(unix)]
use interprocess::local_socket::traits::Listener as _;

use crate::config::BrokerConfig;
#[cfg(unix)]
use crate::fill_random;
use crate::shard::{
    ReactorEvent, ShardCmd, ShardCmdPermit, ShardHandle, ShardShared, should_notify_raw_drained, spawn_shard,
};
#[cfg(windows)]
use crate::transport_windows::{
    DrainingWinConn, MAX_WIN_EVENTS_PER_TURN, WIN_EVENT_CHANNEL_CAP, WinConnHandle, WinTransportEvent, clamp_win_wait,
    drain_win_events_bounded, spawn_win_accept_thread, wait_for_win_wake,
};
use crate::{engine_abi_hash, now_monotonic_ms};

/// Hard cap on simultaneous client connections accepted by the broker.
pub(crate) const MAX_CONNECTIONS: usize = 1024;

/// Maximum shard events drained from `reactor_rx` per reactor turn before servicing sockets & timers.
pub(crate) const MAX_SHARD_EVENTS_PER_TURN: usize = 256;

/// Maximum client-initiated bulk/control commands one connection may have queued/executing in shards
/// (and shard->reactor replies) at once.
pub(crate) const MAX_INFLIGHT_SHARD_CMDS_PER_CONN: usize = 1_024;

/// Dedicated in-flight permit cap for interactive input (`Key`/`Text`/`Pointer`/`Focus`/`ViewportHint`)
/// so bulk RPC bursts (`FetchRows`, `GetImageBlob`, etc.) can never reject typing with `Busy`.
pub(crate) const MAX_INFLIGHT_INPUT_CMDS_PER_CONN: usize = 1_024;

/// Backoff duration after `accept()` hits `EMFILE`/`ENFILE` before re-arming the Unix listener.
#[cfg(unix)]
const ACCEPT_EMFILE_BACKOFF: Duration = Duration::from_millis(50);

/// Computes the next `EMFILE`/`ENFILE` listener re-arm deadline.
#[cfg(unix)]
pub(crate) fn compute_accept_backoff_deadline(now: Instant) -> Instant {
    now + ACCEPT_EMFILE_BACKOFF
}

/// Returns `true` iff `msg` is a fire-and-forget interactive UI input message isolated from bulk RPC permits.
pub(crate) fn is_interactive_input_msg(msg: &ClientMsg) -> bool {
    matches!(
        msg,
        ClientMsg::Key(_)
            | ClientMsg::Text { .. }
            | ClientMsg::Pointer(_)
            | ClientMsg::Focus { .. }
            | ClientMsg::ViewportHint(_)
    )
}

#[cfg(unix)]
const LISTENER_TOKEN: PollToken = PollToken(0);

/// Maximum bytes drained from one client socket per reactor turn before yielding to peers.
#[cfg(unix)]
const CONN_READ_QUANTUM: usize = 131_072;

#[cfg(unix)]
const CONN_READ_BUF_BYTES: usize = 65_536;

/// Maximum pending outbound frame chunks buffered for `writev` per connection.
#[cfg(unix)]
const MAX_WRITEV_IOVS: usize = 32;

/// High-water mark (`128 KiB`) on per-connection Unix `write_queue` bytes so backpressure stays
/// inside `BrokerCore`'s 5-tier priority/WFQ `16 MiB` `OutboundQueue`.
#[cfg(unix)]
const WRITE_QUEUE_HIGH_WATER: usize = 131_072;

/// Classification of a non-blocking Unix socket I/O error.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NonBlockErr {
    WouldBlock,
    Interrupted,
    Fatal,
}

/// Classifies a non-blocking Unix I/O error into retry/yield/fatal categories.
#[cfg(unix)]
pub(crate) fn classify_nonblocking_err(err: &io::Error) -> NonBlockErr {
    match err.kind() {
        io::ErrorKind::WouldBlock => NonBlockErr::WouldBlock,
        io::ErrorKind::Interrupted => NonBlockErr::Interrupted,
        _ => NonBlockErr::Fatal,
    }
}

/// Advances a queued `writev` buffer deque after the kernel accepted `written` bytes.
#[cfg(unix)]
pub(crate) fn advance_write_queue(queue: &mut VecDeque<(Vec<u8>, usize)>, mut written: usize) {
    while written > 0 {
        let Some((buf, off)) = queue.front_mut() else {
            break;
        };
        let rem = buf.len() - *off;
        if written >= rem {
            written -= rem;
            queue.pop_front();
        } else {
            *off += written;
            written = 0;
        }
    }
}

/// Picks the index of the shard with the lowest active session count.
pub(crate) fn pick_least_loaded_shard(shards: &[ShardHandle]) -> usize {
    shards.iter().enumerate().min_by_key(|(_, s)| s.session_load()).map_or(0, |(idx, _)| idx)
}

/// Computes the next reactor poll/condvar wait duration from deadlines, start grace, and shutdown.
pub(crate) fn reactor_wait_timeout(
    next_deadline: Option<TimestampMs>,
    now: TimestampMs,
    ever_connected: bool,
    start_grace_rem: Duration,
    shutting_down: bool,
) -> Duration {
    let mut timeout = Duration::from_secs(1);
    if let Some(dl) = next_deadline {
        timeout = timeout.min(Duration::from_millis(dl.saturating_sub(now)));
    }
    if !ever_connected {
        timeout = timeout.min(start_grace_rem);
    }
    if shutting_down {
        timeout = timeout.min(Duration::from_millis(25));
    }
    timeout
}

/// Computes the hard shutdown drain deadline (`now + kill_escalate_after + 5s`).
pub(crate) fn compute_shutdown_deadline(now: Instant, kill_escalate_after: Duration) -> Instant {
    now + kill_escalate_after + Duration::from_secs(5)
}

/// Returns `true` once explicit shutdown has drained all sessions & in-flight spawns, or exceeded its deadline.
pub(crate) fn reactor_should_exit(
    shutting_down: bool,
    sessions_empty: bool,
    now: Instant,
    deadline: Option<Instant>,
) -> bool {
    shutting_down && (sessions_empty || deadline.is_some_and(|dl| now >= dl))
}

pub(crate) fn all_sessions_drained(sessions_empty: bool, pending_spawns: usize) -> bool {
    sessions_empty && pending_spawns == 0
}

#[cfg(any(unix, test))]
pub(crate) fn should_graceful_drain_unix(reason: CloseReason, drained_len: usize, has_final: bool) -> bool {
    matches!(reason, CloseReason::ClientFin) || drained_len != 0 || has_final
}

pub(crate) fn should_fast_poll(shutting_down: bool, has_draining: bool) -> bool {
    shutting_down || has_draining
}

pub(crate) fn drain_deadline_expired(now: Instant, deadline: Instant) -> bool {
    now >= deadline
}

/// Returns `true` when a newly accepted Unix socket satisfies shutdown, capacity, peer, and mode checks.
#[cfg(unix)]
pub(crate) fn can_accept_unix_conn(shutting_down: bool, active_conns: usize, peer_ok: bool, nonblock_ok: bool) -> bool {
    !shutting_down && active_conns < MAX_CONNECTIONS && peer_ok && nonblock_ok
}

/// Returns `true` when the per-connection Unix `writev` queue is below its chunk and byte watermarks.
#[cfg(unix)]
pub(crate) fn write_queue_can_pull(chunks: usize, bytes: usize) -> bool {
    chunks < MAX_WRITEV_IOVS && bytes < WRITE_QUEUE_HIGH_WATER
}

/// Removes all pending `ListSessions` entries belonging to `conn`.
pub(crate) fn prune_pending_lists_for_conn(pending_lists: &mut HashMap<u64, PendingList>, conn: ConnId) {
    pending_lists.retain(|_, pending| pending.conn != conn);
}

/// Drains up to `limit` queued events from `rx`, returning `true` when the cap was hit so the
/// caller clamps the next poll/condvar wait to `Duration::ZERO` without starving sockets or timers.
pub(crate) fn drain_events_bounded<T>(rx: &mpsc::Receiver<T>, limit: usize, mut handle: impl FnMut(T)) -> bool {
    let mut count = 0usize;
    while count < limit {
        match rx.try_recv() {
            Ok(ev) => {
                handle(ev);
                count += 1;
            }
            Err(_) => return false,
        }
    }
    true
}

/// Clamps the reactor poll wait to `Duration::ZERO` when a bounded channel drain hit its per-turn cap.
pub(crate) fn clamp_reactor_wait(wait_for: Duration, hit_cap: bool) -> Duration {
    if hit_cap { Duration::ZERO } else { wait_for }
}

/// Reactor-parked stalled view frame for one attachment, bounded by `BrokerCore`'s `MemoryBudget`.
pub(crate) enum StalledFrame {
    /// Frame payload retained on the reactor with `charged_bytes` accounted in `BrokerCore::budget`.
    Buffered {
        stream_id: NonZeroStreamId,
        session: SessionId,
        frame: Box<FrameUpdate>,
        charged_bytes: usize,
        reason: FrameStallReason,
    },
    /// `MemoryBudget` was too full to park the frame payload; wait until `reason` clears and
    /// `needed_bytes` of budget headroom opens, then request a fresh `full_sync` via `ViewSlotFree`.
    DeferredResync { stream_id: NonZeroStreamId, session: SessionId, reason: FrameStallReason, needed_bytes: usize },
}

impl StalledFrame {
    pub(crate) fn session(&self) -> SessionId {
        match self {
            Self::Buffered { session, .. } | Self::DeferredResync { session, .. } => *session,
        }
    }

    pub(crate) fn reason(&self) -> FrameStallReason {
        match self {
            Self::Buffered { reason, .. } | Self::DeferredResync { reason, .. } => *reason,
        }
    }

    pub(crate) fn charged_bytes(&self) -> usize {
        match self {
            Self::Buffered { charged_bytes, .. } => *charged_bytes,
            Self::DeferredResync { .. } => 0,
        }
    }
}

/// Removes any stalled or queued view frames belonging to `ended_session` from a connection,
/// returning total `MemoryBudget` bytes that must be released for the pruned stalled frames.
pub(crate) fn prune_ended_session_frames(
    stalled: &mut HashMap<StreamAttachmentId, StalledFrame>,
    queued: &mut HashMap<StreamAttachmentId, (NonZeroStreamId, SessionId)>,
    ended_session: SessionId,
) -> usize {
    let mut freed_bytes = 0usize;
    stalled.retain(|_, entry| {
        if entry.session() == ended_session {
            freed_bytes = freed_bytes.saturating_add(entry.charged_bytes());
            false
        } else {
            true
        }
    });
    queued.retain(|_, (_, sid)| *sid != ended_session);
    freed_bytes
}

/// Selects stalled view attachments waiting on outbound/budget space (skipping pacer-stalled frames
/// that can only unblock on `FrameAck`) whose outbound Priority 1 slot is currently free.
pub(crate) fn ready_stalled_attachments(
    stalled: &HashMap<StreamAttachmentId, StalledFrame>,
    queued: &HashMap<StreamAttachmentId, (NonZeroStreamId, SessionId)>,
) -> Vec<StreamAttachmentId> {
    stalled
        .iter()
        .filter_map(|(&att, entry)| {
            (entry.reason() == FrameStallReason::OutboundOrBudget && !queued.contains_key(&att)).then_some(att)
        })
        .collect()
}

/// In-flight `ListSessions` aggregation across all $K$ shards keyed by broker-allocated `list_id`.
pub(crate) struct PendingList {
    pub(crate) conn: ConnId,
    pub(crate) req_id: RequestId,
    pub(crate) remaining_shards: usize,
    pub(crate) collected: Vec<SessionSummary>,
}

#[cfg(unix)]
struct DrainingUnixConn {
    stream: Stream,
    write_queue: VecDeque<(Vec<u8>, usize)>,
    deadline: Instant,
}

#[cfg(unix)]
enum DrainProgress {
    Done,
    WouldBlock,
    Closed,
}

#[cfg(unix)]
fn drain_unix_queue_nonblocking(stream: &mut Stream, queue: &mut VecDeque<(Vec<u8>, usize)>) -> DrainProgress {
    loop {
        if queue.is_empty() {
            return DrainProgress::Done;
        }
        let slices: Vec<IoSlice<'_>> =
            queue.iter().take(MAX_WRITEV_IOVS).map(|(buf, off)| IoSlice::new(&buf[*off..])).collect();
        match write_vectored_some(stream, &slices) {
            Ok(0) => return DrainProgress::Closed,
            Ok(written) => advance_write_queue(queue, written),
            Err(err) => match classify_nonblocking_err(&err) {
                NonBlockErr::WouldBlock => return DrainProgress::WouldBlock,
                NonBlockErr::Interrupted => continue,
                NonBlockErr::Fatal => return DrainProgress::Closed,
            },
        }
    }
}

/// Per-connection transport state, stalled view frames (keyed by [`StreamAttachmentId`]),
/// and bounded in-flight shard mailbox command counters.
pub(crate) struct ConnState {
    #[cfg(unix)]
    stream: Stream,
    #[cfg(unix)]
    token: PollToken,
    #[cfg(unix)]
    write_queue: VecDeque<(Vec<u8>, usize)>,
    #[cfg(unix)]
    write_queue_bytes: usize,
    #[cfg(windows)]
    pub(crate) win: WinConnHandle,
    pub(crate) stalled_frames: HashMap<StreamAttachmentId, StalledFrame>,
    /// Set of active view attachments whose Priority 1 frame is currently queued in `BrokerCore::outbound`.
    pub(crate) queued_frame_streams: HashMap<StreamAttachmentId, (NonZeroStreamId, SessionId)>,
    /// Number of client-initiated bulk/control commands from this connection currently in flight.
    pub(crate) inflight_shard_cmds: Arc<AtomicUsize>,
    /// Number of interactive UI input commands (`Key`/`Text`/`Pointer`/`Focus`/`ViewportHint`) in flight.
    pub(crate) inflight_input_cmds: Arc<AtomicUsize>,
    /// Set once `ClientFin` arrived while in-flight shard commands were still pending.
    pub(crate) fin_pending: Arc<AtomicBool>,
}

impl ConnState {
    pub(crate) fn has_inflight_cmds(&self) -> bool {
        self.inflight_shard_cmds.load(Ordering::Acquire) != 0 || self.inflight_input_cmds.load(Ordering::Acquire) != 0
    }
}

/// Cross-thread wakeup handle for the Control Reactor (used by shards and external shutdown).
#[derive(Clone)]
pub(crate) struct ReactorWake {
    #[cfg(unix)]
    waker: apack_os::pty_poller::PtyWaker,
    #[cfg(windows)]
    cond: Arc<(Mutex<bool>, Condvar)>,
}

impl ReactorWake {
    pub(crate) fn wake(&self) {
        #[cfg(unix)]
        {
            let _ = self.waker.wake();
        }
        #[cfg(windows)]
        {
            let (lock, cv) = &*self.cond;
            let mut flag = lock.lock().unwrap_or_else(|e| e.into_inner());
            *flag = true;
            cv.notify_one();
        }
    }
}

/// Runs the Control & Transport Reactor on the calling thread until shutdown or idle teardown.
pub(crate) fn run_control_reactor(
    config: BrokerConfig,
    listener: Listener,
    shutdown_flag: Arc<AtomicBool>,
    on_wake_ready: impl FnOnce(ReactorWake),
) -> io::Result<()> {
    #[cfg(unix)]
    set_listener_nonblocking(&listener, true)?;

    #[cfg(unix)]
    let (poller, reactor_wake) = {
        let mut poller = PtyPoller::new()?;
        poller.register(LISTENER_TOKEN, listener_fd(&listener), Interest::READ)?;
        let wake = ReactorWake { waker: poller.waker() };
        (poller, wake)
    };

    #[cfg(windows)]
    let (win_cond, reactor_wake) = {
        let cond = Arc::new((Mutex::new(false), Condvar::new()));
        let wake = ReactorWake { cond: Arc::clone(&cond) };
        (cond, wake)
    };

    on_wake_ready(reactor_wake.clone());

    let (reactor_tx, reactor_rx) = mpsc::channel::<ReactorEvent>();
    let image_store = Arc::new(Mutex::new(ImageStore::default()));
    let shared = ShardShared {
        image_store: Arc::clone(&image_store),
        wal_registry: apack_wal::RecordingRegistry::new(),
        wal_config: config.wal_config,
        idle_compress_after: config.idle_compress_after,
        kill_escalate_after: config.kill_escalate_after,
        engine_abi_hash: engine_abi_hash(),
    };

    let wake_fn: Arc<dyn Fn() + Send + Sync> = {
        let w = reactor_wake.clone();
        Arc::new(move || w.wake())
    };
    let shard_count = config.shard_count.max(1);
    let mut shards = Vec::with_capacity(shard_count);
    for idx in 0..shard_count {
        shards.push(spawn_shard(idx, shared.clone(), reactor_tx.clone(), Arc::clone(&wake_fn))?);
    }

    #[cfg(windows)]
    let (win_ev_tx, win_ev_rx) = mpsc::sync_channel::<WinTransportEvent>(WIN_EVENT_CHANNEL_CAP);
    #[cfg(windows)]
    let listener = Arc::new(listener);
    #[cfg(windows)]
    let accept_thread = spawn_win_accept_thread(
        Arc::clone(&listener),
        Arc::clone(&shutdown_flag),
        win_ev_tx.clone(),
        Arc::clone(&wake_fn),
    )?;

    let mut state = ReactorState {
        core: BrokerCore::with_queue_cap(config.auth, config.max_queue_bytes),
        shards,
        image_store,
        conns: HashMap::new(),
        flushing_conns: HashSet::new(),
        next_conn_id: 1,
        next_session_id: 1,
        next_list_id: 1,
        session_shard: HashMap::new(),
        session_kept: HashMap::new(),
        pending_spawns: 0,
        pending_lists: HashMap::new(),
        ever_connected: false,
        started_at: Instant::now(),
        start_grace: config.start_grace,
        kill_escalate_after: config.kill_escalate_after,
        graceful_drain_timeout: config.graceful_drain_timeout,
        shutting_down: false,
        shutdown_deadline: None,
        shutdown_flag,
        wake_reactor: Arc::clone(&wake_fn),
        #[cfg(unix)]
        listener,
        #[cfg(unix)]
        poller,
        #[cfg(unix)]
        token_to_conn: HashMap::new(),
        #[cfg(unix)]
        draining_unix: HashMap::new(),
        #[cfg(unix)]
        read_buf: vec![0u8; CONN_READ_BUF_BYTES],
        #[cfg(unix)]
        accept_backoff_until: None,
        #[cfg(windows)]
        win_ev_tx,
        #[cfg(windows)]
        win_wake: wake_fn,
        #[cfg(windows)]
        draining_win: HashMap::new(),
    };

    #[cfg(unix)]
    let mut poll_events = Vec::with_capacity(64);

    while !state.should_exit() {
        if state.shutdown_flag.load(Ordering::Acquire) && !state.shutting_down {
            state.begin_shutdown();
        }

        // 1. Drain a bounded batch of shard events so floods cannot starve sockets, timers, or shutdown.
        let hit_shard_event_cap = drain_events_bounded(&reactor_rx, MAX_SHARD_EVENTS_PER_TURN, |ev| {
            state.handle_shard_event(ev);
        });

        // 2. Drain bounded Windows transport events (if Windows).
        #[cfg(windows)]
        let hit_win_event_cap = drain_win_events_bounded(&win_ev_rx, MAX_WIN_EVENTS_PER_TURN, |ev| {
            state.handle_win_transport_event(ev);
        });

        // 3. Fire due BrokerCore timers, finish half-closed `ClientFin` conns whose in-flight
        //    shard commands drained, resume any EMFILE-paused listener, and reap draining conns.
        state.reap_draining_conns();
        #[cfg(unix)]
        state.maybe_resume_unix_accept(false, Instant::now());
        let now = now_monotonic_ms();
        state.reap_pending_fin_conns(now);
        let mut actions = Vec::new();
        state.core.poll_timers(now, &mut actions);
        state.execute_actions(actions, now);

        // 4. Check start_grace (exit cleanly if no client ever connected).
        if !state.ever_connected && state.started_at.elapsed() >= state.start_grace {
            state.begin_shutdown();
        }

        // 5. Auto-exit when all clients disconnected, no broker lease holds us, and no sessions
        //    or in-flight spawns remain (wait for draining final replies to finish first).
        if state.ever_connected
            && state.core.connection_count() == 0
            && !state.has_draining_conns()
            && !state.core.has_active_broker_lease()
            && state.session_shard.is_empty()
            && state.pending_spawns == 0
        {
            state.begin_shutdown();
        }

        if state.should_exit() {
            break;
        }

        let wait_for = reactor_wait_timeout(
            state.core.next_deadline(),
            now,
            state.ever_connected,
            state.start_grace.saturating_sub(state.started_at.elapsed()),
            should_fast_poll(state.shutting_down, state.has_draining_conns()),
        );
        #[cfg(unix)]
        let wait_for = match state.accept_backoff_until {
            Some(until) => wait_for.min(until.saturating_duration_since(Instant::now())),
            None => wait_for,
        };
        let wait_for = clamp_reactor_wait(wait_for, hit_shard_event_cap);
        #[cfg(windows)]
        let wait_for = clamp_win_wait(wait_for, hit_win_event_cap);

        #[cfg(unix)]
        {
            poll_events.clear();
            if state.poller.poll(&mut poll_events, Some(wait_for)).is_ok() {
                state.handle_unix_events(&poll_events);
            }
        }

        #[cfg(windows)]
        wait_for_win_wake(&win_cond, wait_for);
    }

    // Drop `win_ev_rx` BEFORE shard teardown and `accept_thread.join()` so any Windows accept,
    // reader, or writer thread blocked in `ev_tx.send(...)` on a full channel unblocks immediately.
    #[cfg(windows)]
    drop(win_ev_rx);

    // Tear down all shards cleanly (sealing recordings and terminating remaining trees).
    for mut shard in state.shards.drain(..) {
        shard.shutdown();
    }
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&config.socket_path);
    }
    #[cfg(windows)]
    {
        // Drop our Arc<Listener> FIRST so if `accept_thread` already exited the pipe disappears
        // immediately (never spinning in `ERROR_PIPE_BUSY`); if `accept_thread` is still parked in
        // `accept()`, the probe connect wakes it so it observes `shutdown_flag`, drops the last
        // `Arc<Listener>`, and exits cleanly before `run_control_reactor` returns!
        state.shutdown_flag.store(true, Ordering::Release);
        drop(listener);
        let _ = apack_os::transport::connect_local_stream(&config.socket_path);
        let _ = accept_thread.join();
    }

    Ok(())
}

pub(crate) struct ReactorState {
    pub(crate) core: BrokerCore,
    shards: Vec<ShardHandle>,
    image_store: Arc<Mutex<ImageStore>>,
    pub(crate) conns: HashMap<ConnId, ConnState>,
    pub(crate) flushing_conns: HashSet<ConnId>,
    next_conn_id: u64,
    next_session_id: u64,
    next_list_id: u64,
    session_shard: HashMap<SessionId, usize>,
    session_kept: HashMap<SessionId, bool>,
    pending_spawns: usize,
    pending_lists: HashMap<u64, PendingList>,
    pub(crate) ever_connected: bool,
    started_at: Instant,
    start_grace: Duration,
    kill_escalate_after: Duration,
    pub(crate) graceful_drain_timeout: Duration,
    pub(crate) shutting_down: bool,
    shutdown_deadline: Option<Instant>,
    shutdown_flag: Arc<AtomicBool>,
    wake_reactor: Arc<dyn Fn() + Send + Sync>,
    #[cfg(unix)]
    listener: Listener,
    #[cfg(unix)]
    poller: PtyPoller,
    #[cfg(unix)]
    token_to_conn: HashMap<PollToken, ConnId>,
    #[cfg(unix)]
    draining_unix: HashMap<PollToken, DrainingUnixConn>,
    #[cfg(unix)]
    read_buf: Vec<u8>,
    #[cfg(unix)]
    accept_backoff_until: Option<Instant>,
    #[cfg(windows)]
    pub(crate) win_ev_tx: mpsc::SyncSender<WinTransportEvent>,
    #[cfg(windows)]
    pub(crate) win_wake: Arc<dyn Fn() + Send + Sync>,
    #[cfg(windows)]
    pub(crate) draining_win: HashMap<ConnId, DrainingWinConn>,
}

impl ReactorState {
    fn should_exit(&self) -> bool {
        reactor_should_exit(
            self.shutting_down,
            all_sessions_drained(self.session_shard.is_empty(), self.pending_spawns),
            Instant::now(),
            self.shutdown_deadline,
        )
    }

    fn has_draining_conns(&self) -> bool {
        #[cfg(unix)]
        {
            !self.draining_unix.is_empty()
        }
        #[cfg(windows)]
        {
            self.has_draining_win()
        }
    }

    fn reap_draining_conns(&mut self) {
        #[cfg(unix)]
        {
            let now = Instant::now();
            let expired: Vec<PollToken> = self
                .draining_unix
                .iter()
                .filter_map(|(&tok, d)| if drain_deadline_expired(now, d.deadline) { Some(tok) } else { None })
                .collect();
            for tok in expired {
                if let Some(d) = self.draining_unix.remove(&tok) {
                    let _ = self.poller.deregister(tok);
                    let _ = hang_up(&d.stream);
                    self.maybe_resume_unix_accept(true, now);
                }
            }
        }
        #[cfg(windows)]
        self.reap_draining_win_conns();
    }

    fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.shutdown_deadline
            .get_or_insert_with(|| compute_shutdown_deadline(Instant::now(), self.kill_escalate_after));
        self.shutdown_flag.store(true, Ordering::Release);
        #[cfg(unix)]
        {
            let _ = self.poller.deregister(LISTENER_TOKEN);
            for (tok, d) in self.draining_unix.drain() {
                let _ = self.poller.deregister(tok);
                let _ = hang_up(&d.stream);
            }
        }
        #[cfg(windows)]
        {
            for (_, d) in self.draining_win.drain() {
                (d.win.close_abrupt)();
            }
        }
        // Terminate ALL sessions (kept and non-kept alike) so explicit shutdown never deadlocks.
        for shard in &self.shards {
            shard.send(ShardCmd::TeardownAll);
        }
        // Close all open client connections cleanly.
        let cids: Vec<ConnId> = self.conns.keys().copied().collect();
        for cid in cids {
            self.close_conn_transport(cid);
        }
    }

    pub(crate) fn alloc_conn_id(&mut self) -> ConnId {
        let id = self.next_conn_id;
        self.next_conn_id = self.next_conn_id.saturating_add(1).max(1);
        ConnId::new(id).expect("non-zero conn id")
    }

    fn alloc_session_id(&mut self) -> SessionId {
        let id = self.next_session_id;
        self.next_session_id = self.next_session_id.saturating_add(1).max(1);
        SessionId::new(id).expect("non-zero session id")
    }

    fn alloc_list_id(&mut self) -> u64 {
        let id = self.next_list_id;
        self.next_list_id = self.next_list_id.wrapping_add(1).max(1);
        id
    }

    /// Reads lock-free per-shard telemetry atomics into `BrokerCore` before serving client requests.
    pub(crate) fn sync_shard_telemetry(&mut self) {
        let mut scrollback_bytes = 0u64;
        let mut total_pty_bytes = 0u64;
        for shard in &self.shards {
            let (sb, pty) = shard.telemetry();
            scrollback_bytes = scrollback_bytes.saturating_add(sb);
            total_pty_bytes = total_pty_bytes.saturating_add(pty);
        }
        let image_store_bytes = self.image_store.lock().unwrap_or_else(|e| e.into_inner()).used_bytes() as u64;
        self.core.update_shard_telemetry(ShardTelemetry { scrollback_bytes, image_store_bytes, total_pty_bytes });
    }

    #[cfg(unix)]
    fn handle_unix_events(&mut self, events: &[PollEvent]) {
        for ev in events {
            if ev.token == LISTENER_TOKEN {
                self.accept_unix_connections();
                continue;
            }
            if self.draining_unix.contains_key(&ev.token) {
                self.poll_draining_unix(ev.token);
                continue;
            }
            let Some(&conn) = self.token_to_conn.get(&ev.token) else {
                continue;
            };
            if ev.writable {
                self.flush_unix_writes(conn);
            }
            if ev.readable || ev.hangup {
                self.read_unix_conn(conn);
            }
        }
    }

    #[cfg(unix)]
    fn poll_draining_unix(&mut self, token: PollToken) {
        let done = match self.draining_unix.get_mut(&token) {
            Some(d) => match drain_unix_queue_nonblocking(&mut d.stream, &mut d.write_queue) {
                DrainProgress::Done | DrainProgress::Closed => true,
                DrainProgress::WouldBlock => drain_deadline_expired(Instant::now(), d.deadline),
            },
            None => false,
        };
        if done && let Some(d) = self.draining_unix.remove(&token) {
            let _ = self.poller.deregister(token);
            drop(d.stream);
            self.maybe_resume_unix_accept(true, Instant::now());
        }
    }

    #[cfg(unix)]
    fn maybe_resume_unix_accept(&mut self, force: bool, now: Instant) {
        if self.shutting_down {
            self.accept_backoff_until = None;
            return;
        }
        let Some(until) = self.accept_backoff_until else {
            return;
        };
        if !force && now < until {
            return;
        }
        if self.poller.register(LISTENER_TOKEN, listener_fd(&self.listener), Interest::READ).is_ok() {
            self.accept_backoff_until = None;
        } else {
            self.accept_backoff_until = Some(compute_accept_backoff_deadline(now));
        }
    }

    #[cfg(unix)]
    fn accept_unix_connections(&mut self) {
        loop {
            match self.listener.accept() {
                Ok(stream) => {
                    let peer_ok = matches!(verify_peer_is_current_user(&stream), Ok(true));
                    let nonblock_ok = set_stream_nonblocking(&stream, true).is_ok();
                    let active_total = self.conns.len().saturating_add(self.draining_unix.len());
                    if !can_accept_unix_conn(self.shutting_down, active_total, peer_ok, nonblock_ok) {
                        let _ = hang_up(&stream);
                        continue;
                    }
                    let mut nonce = [0u8; 32];
                    if fill_random(&mut nonce).is_err() {
                        let _ = hang_up(&stream);
                        continue;
                    }
                    let conn = self.alloc_conn_id();
                    let token = PollToken(conn.get());
                    if self.poller.register(token, stream_fd(&stream), Interest::READ).is_err() {
                        let _ = hang_up(&stream);
                        continue;
                    }
                    let now = now_monotonic_ms();
                    let _ = self.core.open_connection(conn, nonce, now);
                    self.token_to_conn.insert(token, conn);
                    self.conns.insert(
                        conn,
                        ConnState {
                            stream,
                            token,
                            write_queue: VecDeque::new(),
                            write_queue_bytes: 0,
                            stalled_frames: HashMap::new(),
                            queued_frame_streams: HashMap::new(),
                            inflight_shard_cmds: Arc::new(AtomicUsize::new(0)),
                            inflight_input_cmds: Arc::new(AtomicUsize::new(0)),
                            fin_pending: Arc::new(AtomicBool::new(false)),
                        },
                    );
                }
                Err(err) => {
                    if is_fd_exhaustion_error(&err) {
                        let _ = self.poller.deregister(LISTENER_TOKEN);
                        self.accept_backoff_until = Some(compute_accept_backoff_deadline(Instant::now()));
                        break;
                    }
                    match classify_nonblocking_err(&err) {
                        NonBlockErr::Interrupted => continue,
                        NonBlockErr::WouldBlock | NonBlockErr::Fatal => break,
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn read_unix_conn(&mut self, conn: ConnId) {
        let mut budget = CONN_READ_QUANTUM;
        let mut closed = false;

        while budget != 0 {
            let cap = budget.min(self.read_buf.len());
            let read_res = match self.conns.get_mut(&conn) {
                Some(c) => c.stream.read(&mut self.read_buf[..cap]),
                None => return,
            };
            match read_res {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => {
                    budget = budget.saturating_sub(n);
                    self.sync_shard_telemetry();
                    let now = now_monotonic_ms();
                    let mut actions = Vec::new();
                    self.core.feed_bytes(conn, &self.read_buf[..n], now, &mut actions);
                    if self.core.is_lifecycle_client_ready(conn) {
                        self.ever_connected = true;
                    }
                    self.execute_actions(actions, now);
                    if self.conns.get(&conn).is_none_or(|c| c.fin_pending.load(Ordering::Acquire)) {
                        break;
                    }
                }
                Err(err) => match classify_nonblocking_err(&err) {
                    NonBlockErr::WouldBlock => break,
                    NonBlockErr::Interrupted => continue,
                    NonBlockErr::Fatal => {
                        closed = true;
                        break;
                    }
                },
            }
        }

        if closed {
            self.on_transport_closed(conn);
        }
    }

    #[cfg(unix)]
    fn flush_unix_writes(&mut self, conn: ConnId) {
        if !self.flushing_conns.insert(conn) {
            return;
        }
        self.flush_unix_writes_inner(conn);
        self.flushing_conns.remove(&conn);
    }

    #[cfg(unix)]
    fn flush_unix_writes_inner(&mut self, conn: ConnId) {
        loop {
            // Pull outbound messages up to a bounded watermark so excess data stays in `BrokerCore`
            // where 5-tier priority + WFQ and the 16 MiB `SlowReader` limit are enforced.
            while self.conns.get(&conn).is_some_and(|c| write_queue_can_pull(c.write_queue.len(), c.write_queue_bytes))
            {
                let Some(queued) = self.core.pop_outbound(conn) else {
                    break;
                };
                let is_frame = matches!(queued.msg, BrokerMsg::Frame(_));
                let drained_frame = if is_frame
                    && let Some(att) = queued.attachment_id
                    && let Some((nz, session_id)) =
                        self.conns.get_mut(&conn).and_then(|c| c.queued_frame_streams.remove(&att))
                {
                    Some((att, nz, session_id))
                } else {
                    None
                };
                let mut encoded = Vec::with_capacity(queued.encoded_bytes);
                if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_ok()
                    && let Some(c) = self.conns.get_mut(&conn)
                {
                    c.write_queue_bytes = c.write_queue_bytes.saturating_add(encoded.len());
                    c.write_queue.push_back((encoded, 0));
                }
                // Release the frame slot and retry budget-stalled frames STRICTLY AFTER pushing
                // `encoded` onto `c.write_queue` so subsequent frames can never overtake this one.
                if let Some((att, nz, session_id)) = drained_frame {
                    self.on_outbound_frame_drained(conn, nz, att, session_id);
                }
                self.retry_budget_stalled_frames(conn, now_monotonic_ms());
            }

            let Some(c) = self.conns.get_mut(&conn) else {
                return;
            };
            if c.write_queue.is_empty() {
                let interest = if c.fin_pending.load(Ordering::Acquire) { Interest::NONE } else { Interest::READ };
                let _ = self.poller.modify(c.token, interest);
                return;
            }

            let slices: Vec<IoSlice<'_>> =
                c.write_queue.iter().take(MAX_WRITEV_IOVS).map(|(buf, off)| IoSlice::new(&buf[*off..])).collect();

            match write_vectored_some(&mut c.stream, &slices) {
                Ok(0) => {
                    self.on_transport_closed(conn);
                    return;
                }
                Ok(written) => {
                    c.write_queue_bytes = c.write_queue_bytes.saturating_sub(written);
                    advance_write_queue(&mut c.write_queue, written);
                }
                Err(err) => match classify_nonblocking_err(&err) {
                    NonBlockErr::WouldBlock => {
                        let interest =
                            if c.fin_pending.load(Ordering::Acquire) { Interest::WRITE } else { Interest::READ_WRITE };
                        let _ = self.poller.modify(c.token, interest);
                        return;
                    }
                    NonBlockErr::Interrupted => continue,
                    NonBlockErr::Fatal => {
                        self.on_transport_closed(conn);
                        return;
                    }
                },
            }
        }
    }

    fn flush_writes(&mut self, conn: ConnId) {
        #[cfg(unix)]
        self.flush_unix_writes(conn);
        #[cfg(windows)]
        self.flush_win_writes(conn);
    }

    fn take_stalled_frame(&mut self, conn: ConnId, attachment_id: StreamAttachmentId) -> Option<StalledFrame> {
        let entry = self.conns.get_mut(&conn)?.stalled_frames.remove(&attachment_id)?;
        self.core.release_stalled_frame_bytes(conn, entry.charged_bytes());
        Some(entry)
    }

    pub(crate) fn retry_stalled_frame(&mut self, conn: ConnId, attachment_id: StreamAttachmentId, now: TimestampMs) {
        let Some(entry) = self.take_stalled_frame(conn, attachment_id) else {
            return;
        };
        match entry {
            StalledFrame::Buffered { stream_id, session, frame, .. } => {
                self.try_publish_or_stall_frame(conn, stream_id, attachment_id, session, frame, now);
            }
            StalledFrame::DeferredResync { stream_id, session, needed_bytes, .. } => {
                if !self.core.is_attachment_active(conn, stream_id, attachment_id) {
                    return;
                }
                let can_pub = self.core.can_publish_frame_now(conn, stream_id, attachment_id);
                let has_room = self.core.connection_budget_remaining(conn) >= needed_bytes;
                if can_pub && has_room {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::ViewSlotFree {
                            session_id: session,
                            conn,
                            stream_id,
                            attachment_id,
                            force_full_sync: true,
                        });
                    }
                } else if let Some(c) = self.conns.get_mut(&conn) {
                    let next_reason = if !can_pub && !c.queued_frame_streams.contains_key(&attachment_id) {
                        FrameStallReason::Pacer
                    } else {
                        FrameStallReason::OutboundOrBudget
                    };
                    c.stalled_frames.insert(
                        attachment_id,
                        StalledFrame::DeferredResync { stream_id, session, reason: next_reason, needed_bytes },
                    );
                }
            }
        }
    }

    pub(crate) fn retry_budget_stalled_frames(&mut self, conn: ConnId, now: TimestampMs) {
        let ready_stalled = self
            .conns
            .get(&conn)
            .map(|c| ready_stalled_attachments(&c.stalled_frames, &c.queued_frame_streams))
            .unwrap_or_default();
        for att in ready_stalled {
            self.retry_stalled_frame(conn, att, now);
        }
    }

    pub(crate) fn on_outbound_frame_drained(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
        session_id: SessionId,
    ) {
        // Retry any outbound/budget-stalled frame held on the reactor for this exact attachment;
        // leave pacer-stalled frames parked until `FrameAck` arrives.
        let stalled_reason =
            self.conns.get(&conn).and_then(|c| c.stalled_frames.get(&attachment_id)).map(StalledFrame::reason);
        match stalled_reason {
            Some(FrameStallReason::OutboundOrBudget) => {
                self.retry_stalled_frame(conn, attachment_id, now_monotonic_ms());
            }
            Some(FrameStallReason::Pacer | FrameStallReason::Stale) => {}
            None => {
                if self.core.is_attachment_active(conn, stream_id, attachment_id)
                    && let Some(&shard_idx) = self.session_shard.get(&session_id)
                {
                    self.shards[shard_idx].send(ShardCmd::ViewSlotFree {
                        session_id,
                        conn,
                        stream_id,
                        attachment_id,
                        force_full_sync: false,
                    });
                }
            }
        }
    }

    pub(crate) fn try_publish_or_stall_frame(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
        session: SessionId,
        frame: Box<FrameUpdate>,
        now: TimestampMs,
    ) {
        // Verify `(conn, stream_id)` is still bound to the exact `attachment_id`
        // (globally unique across sessions, eliminating cross-session and same-session stale frames).
        if !self.core.is_attachment_active(conn, stream_id, attachment_id) {
            return;
        }
        let mut actions = Vec::new();
        match self.core.try_publish_frame(conn, stream_id, attachment_id, frame, now, &mut actions) {
            Ok(()) => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.queued_frame_streams.insert(attachment_id, (stream_id, session));
                }
                self.execute_actions(actions, now);
            }
            Err((frame, reason, encoded_bytes)) => {
                if reason == FrameStallReason::Stale {
                    return;
                }
                // Release any older parked frame for this attachment before charging the replacement.
                let _ = self.take_stalled_frame(conn, attachment_id);
                let charge =
                    if encoded_bytes != 0 { encoded_bytes } else { estimate_frame_approx_bytes(&frame).max(1) };
                if self.core.reserve_stalled_frame_bytes(conn, charge) {
                    if let Some(c) = self.conns.get_mut(&conn) {
                        c.stalled_frames.insert(
                            attachment_id,
                            StalledFrame::Buffered { stream_id, session, frame, charged_bytes: charge, reason },
                        );
                    } else {
                        self.core.release_stalled_frame_bytes(conn, charge);
                    }
                } else if let Some(c) = self.conns.get_mut(&conn) {
                    // MemoryBudget is full: drop the frame payload immediately and defer a fresh
                    // `full_sync` via `ViewSlotFree` once headroom opens.
                    let needed_bytes = charge.min(self.core.max_queue_bytes());
                    c.stalled_frames.insert(
                        attachment_id,
                        StalledFrame::DeferredResync { stream_id, session, reason, needed_bytes },
                    );
                }
            }
        }
    }

    fn publish_final_frame(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        attachment_id: StreamAttachmentId,
        frame: Box<FrameUpdate>,
        now: TimestampMs,
    ) {
        if !self.core.is_attachment_active(conn, stream_id, attachment_id) {
            return;
        }
        let _ = self.take_stalled_frame(conn, attachment_id);
        if let Some(c) = self.conns.get_mut(&conn) {
            c.queued_frame_streams.remove(&attachment_id);
        }
        let mut actions = Vec::new();
        self.core.publish_final_frame(conn, stream_id, attachment_id, frame, now, &mut actions);
        self.execute_actions(actions, now);
    }

    fn try_acquire_permit_from(
        &mut self,
        conn: ConnId,
        stream_id: StreamId,
        req_id: RequestId,
        now: TimestampMs,
        interactive_input: bool,
    ) -> Option<Arc<ShardCmdPermit>> {
        let (counter, fin_pending, limit) = self.conns.get(&conn).map(|c| {
            if interactive_input {
                (Arc::clone(&c.inflight_input_cmds), Arc::clone(&c.fin_pending), MAX_INFLIGHT_INPUT_CMDS_PER_CONN)
            } else {
                (Arc::clone(&c.inflight_shard_cmds), Arc::clone(&c.fin_pending), MAX_INFLIGHT_SHARD_CMDS_PER_CONN)
            }
        })?;
        match ShardCmdPermit::try_acquire_with_wake(
            &counter,
            limit,
            Some(fin_pending),
            Some(Arc::clone(&self.wake_reactor)),
        ) {
            Some(permit) => Some(Arc::new(permit)),
            None => {
                let mut actions = Vec::new();
                self.core.enqueue_message(
                    conn,
                    stream_id,
                    req_id,
                    OutboundPriority::Control,
                    BrokerMsg::Error {
                        code: ErrorCode::Busy,
                        message: String::from("too many in-flight shard commands"),
                    },
                    now,
                    &mut actions,
                );
                self.execute_actions(actions, now);
                None
            }
        }
    }

    fn try_acquire_shard_permit(
        &mut self,
        conn: ConnId,
        stream_id: StreamId,
        req_id: RequestId,
        now: TimestampMs,
    ) -> Option<Arc<ShardCmdPermit>> {
        self.try_acquire_permit_from(conn, stream_id, req_id, now, false)
    }

    fn close_pending_fin_conn(&mut self, conn: ConnId, now: TimestampMs) {
        let mut actions = Vec::new();
        self.core.close_connection(conn, CloseReason::ClientFin, now, &mut actions);
        self.execute_actions(actions, now);
    }

    fn maybe_finish_pending_fin(&mut self, conn: ConnId, now: TimestampMs) {
        let ready_to_close =
            self.conns.get(&conn).is_some_and(|c| c.fin_pending.load(Ordering::Acquire) && !c.has_inflight_cmds());
        if ready_to_close {
            self.close_pending_fin_conn(conn, now);
        }
    }

    fn reap_pending_fin_conns(&mut self, now: TimestampMs) {
        let ready: Vec<ConnId> = self
            .conns
            .iter()
            .filter_map(|(&conn, c)| (c.fin_pending.load(Ordering::Acquire) && !c.has_inflight_cmds()).then_some(conn))
            .collect();
        for conn in ready {
            self.close_pending_fin_conn(conn, now);
        }
    }

    pub(crate) fn on_transport_closed(&mut self, conn: ConnId) {
        self.close_conn_transport(conn);
        let now = now_monotonic_ms();
        let mut actions = Vec::new();
        self.core.close_connection(conn, CloseReason::TransportClosed, now, &mut actions);
        // Cancel any pending Wait requests owned by this connection across all shards.
        for shard in &self.shards {
            shard.send(ShardCmd::CancelConnWaiters { conn });
        }
        prune_pending_lists_for_conn(&mut self.pending_lists, conn);
        self.execute_actions(actions, now);
    }

    fn close_conn_transport(&mut self, conn: ConnId) {
        let Some(c) = self.conns.remove(&conn) else {
            return;
        };
        #[cfg(unix)]
        {
            let _ = self.poller.deregister(c.token);
            self.token_to_conn.remove(&c.token);
            let _ = hang_up(&c.stream);
            self.maybe_resume_unix_accept(true, Instant::now());
        }
        #[cfg(windows)]
        {
            (c.win.close_abrupt)();
        }
    }

    fn handle_shard_event(&mut self, event: ReactorEvent) {
        let now = now_monotonic_ms();
        match event {
            ReactorEvent::SendMsg { conn, stream_id, attachment, req_id, priority, msg, permit } => {
                let mut valid = true;
                if let Some(att) = attachment {
                    match NonZeroStreamId::new(stream_id.raw()) {
                        Some(nz) if self.core.is_attachment_active(conn, nz, att) => {}
                        _ => valid = false,
                    }
                }
                if valid {
                    let mut actions = Vec::new();
                    self.core.enqueue_message(conn, stream_id, req_id, priority, msg, now, &mut actions);
                    self.execute_actions(actions, now);
                }
                drop(permit);
                self.maybe_finish_pending_fin(conn, now);
            }
            ReactorEvent::PublishFrame { conn, stream_id, attachment_id, session, frame, final_frame } => {
                if final_frame {
                    self.publish_final_frame(conn, stream_id, attachment_id, frame, now);
                } else {
                    self.try_publish_or_stall_frame(conn, stream_id, attachment_id, session, frame, now);
                }
            }
            ReactorEvent::PublishRawOutput { session, bytes, session_inflight, shard_inflight } => {
                let len = bytes.len();
                let mut actions = Vec::new();
                self.core.publish_raw_output(session, &bytes, now, &mut actions);
                self.execute_actions(actions, now);
                let prev_sess = session_inflight.fetch_sub(len, Ordering::AcqRel);
                let prev_shard = shard_inflight.fetch_sub(len, Ordering::AcqRel);
                if should_notify_raw_drained(
                    prev_sess,
                    prev_sess.saturating_sub(len),
                    prev_shard,
                    prev_shard.saturating_sub(len),
                ) && let Some(&shard_idx) = self.session_shard.get(&session)
                {
                    self.shards[shard_idx].send(ShardCmd::RawOutputDrained);
                }
            }
            ReactorEvent::SessionRegistered { session, owner, shard_idx, rewound, kept } => {
                self.pending_spawns = self.pending_spawns.saturating_sub(1);
                self.session_shard.insert(session, shard_idx);
                self.session_kept.insert(session, kept);
                let lifetime = if rewound {
                    SessionLifetime::ConnBound(owner)
                } else if kept {
                    SessionLifetime::Kept
                } else {
                    SessionLifetime::LeaseBound
                };
                let mut actions = Vec::new();
                self.core.register_session(session, lifetime, rewound, &mut actions);
                self.execute_actions(actions, now);
            }
            ReactorEvent::SpawnOrRewindFailed => {
                self.pending_spawns = self.pending_spawns.saturating_sub(1);
            }
            ReactorEvent::SessionKeptUpdated { session, kept } => {
                self.session_kept.insert(session, kept);
                self.core.set_session_kept(session, kept);
            }
            ReactorEvent::SessionEnded { session } => {
                self.session_shard.remove(&session);
                self.session_kept.remove(&session);
                let mut freed_by_conn = Vec::new();
                for (&conn, c) in &mut self.conns {
                    let freed = prune_ended_session_frames(&mut c.stalled_frames, &mut c.queued_frame_streams, session);
                    freed_by_conn.push((conn, freed));
                }
                for (conn, freed) in freed_by_conn {
                    self.core.release_stalled_frame_bytes(conn, freed);
                }
                let mut actions = Vec::new();
                self.core.unregister_session(session, "session ended", now, &mut actions);
                self.execute_actions(actions, now);
            }
            ReactorEvent::SessionListPart { list_id, sessions, permit } => {
                let conn_opt = self.pending_lists.get(&list_id).map(|p| p.conn);
                let ready = if let Some(pending) = self.pending_lists.get_mut(&list_id) {
                    pending.collected.extend(sessions);
                    pending.remaining_shards = pending.remaining_shards.saturating_sub(1);
                    pending.remaining_shards == 0
                } else {
                    false
                };
                if ready && let Some(mut pending) = self.pending_lists.remove(&list_id) {
                    pending.collected.sort_by_key(|s| s.id);
                    let mut actions = Vec::new();
                    self.core.enqueue_message(
                        pending.conn,
                        StreamId::CONTROL,
                        pending.req_id,
                        OutboundPriority::Control,
                        BrokerMsg::SessionsList { sessions: pending.collected },
                        now,
                        &mut actions,
                    );
                    self.execute_actions(actions, now);
                }
                drop(permit);
                if let Some(conn) = conn_opt {
                    self.maybe_finish_pending_fin(conn, now);
                }
            }
        }
    }

    pub(crate) fn execute_actions(&mut self, actions: Vec<BrokerAction>, now: TimestampMs) {
        for action in actions {
            match action {
                BrokerAction::OutboundReady(conn) => {
                    self.flush_writes(conn);
                }
                BrokerAction::ClientFin(conn) => {
                    let has_inflight = if let Some(c) = self.conns.get(&conn) {
                        c.fin_pending.store(true, Ordering::Release);
                        #[cfg(unix)]
                        {
                            let interest = if c.write_queue.is_empty() { Interest::NONE } else { Interest::WRITE };
                            let _ = self.poller.modify(c.token, interest);
                        }
                        c.has_inflight_cmds()
                    } else {
                        false
                    };
                    if !has_inflight {
                        let mut close_actions = Vec::new();
                        self.core.close_connection(conn, CloseReason::ClientFin, now, &mut close_actions);
                        self.execute_actions(close_actions, now);
                    }
                }
                BrokerAction::CloseConn { conn, reason, final_error, drained } => {
                    #[cfg(unix)]
                    if let Some(mut c) = self.conns.remove(&conn) {
                        self.token_to_conn.remove(&c.token);
                        let want_graceful = should_graceful_drain_unix(reason, drained.len(), final_error.is_some());
                        for queued in drained {
                            let mut encoded = Vec::with_capacity(queued.encoded_bytes);
                            if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_ok() {
                                c.write_queue.push_back((encoded, 0));
                            }
                        }
                        if let Some((stream_id, req_id, msg)) = final_error {
                            let mut encoded = Vec::new();
                            if msg.encode_to_frame(stream_id, req_id, &mut encoded).is_ok() {
                                c.write_queue.push_back((encoded, 0));
                            }
                        }
                        if want_graceful {
                            if matches!(
                                drain_unix_queue_nonblocking(&mut c.stream, &mut c.write_queue),
                                DrainProgress::WouldBlock
                            ) {
                                let _ = self.poller.modify(c.token, Interest::WRITE);
                                self.draining_unix.insert(
                                    c.token,
                                    DrainingUnixConn {
                                        stream: c.stream,
                                        write_queue: c.write_queue,
                                        deadline: Instant::now() + self.graceful_drain_timeout,
                                    },
                                );
                            } else {
                                let _ = self.poller.deregister(c.token);
                                drop(c.stream);
                                self.maybe_resume_unix_accept(true, Instant::now());
                            }
                        } else {
                            let _ = self.poller.deregister(c.token);
                            let _ = hang_up(&c.stream);
                            self.maybe_resume_unix_accept(true, Instant::now());
                        }
                    }
                    #[cfg(windows)]
                    self.begin_win_close(conn, reason, drained, final_error);
                    let mut close_actions = Vec::new();
                    self.core.close_connection(conn, reason, now, &mut close_actions);
                    for shard in &self.shards {
                        shard.send(ShardCmd::CancelConnWaiters { conn });
                    }
                    prune_pending_lists_for_conn(&mut self.pending_lists, conn);
                    self.execute_actions(close_actions, now);
                }
                BrokerAction::SpawnSession { conn, req_id, spec } => {
                    let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now) else {
                        continue;
                    };
                    let session_id = self.alloc_session_id();
                    let shard_idx = pick_least_loaded_shard(&self.shards);
                    self.pending_spawns = self.pending_spawns.saturating_add(1);
                    self.shards[shard_idx]
                        .send_with_permit(ShardCmd::Spawn { session_id, conn, req_id, spec }, Some(permit));
                }
                BrokerAction::KillSession { conn, req_id, session } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session)
                        && let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now)
                    {
                        self.shards[shard_idx]
                            .send_with_permit(ShardCmd::Kill { session_id: session, conn, req_id }, Some(permit));
                    }
                }
                BrokerAction::SignalSession { conn, req_id, session, signal, scope } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session)
                        && let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now)
                    {
                        self.shards[shard_idx].send_with_permit(
                            ShardCmd::Signal { session_id: session, conn, req_id, signal, scope },
                            Some(permit),
                        );
                    }
                }
                BrokerAction::QueryOwns { conn, req_id, session, pid } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session)
                        && let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now)
                    {
                        self.shards[shard_idx]
                            .send_with_permit(ShardCmd::Owns { session_id: session, conn, req_id, pid }, Some(permit));
                    }
                }
                BrokerAction::ListSessions { conn, req_id } => {
                    let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now) else {
                        continue;
                    };
                    let list_id = self.alloc_list_id();
                    self.pending_lists.insert(
                        list_id,
                        PendingList { conn, req_id, remaining_shards: self.shards.len(), collected: Vec::new() },
                    );
                    for shard in &self.shards {
                        shard.send_with_permit(ShardCmd::ListSessions { list_id }, Some(Arc::clone(&permit)));
                    }
                }
                BrokerAction::RewindRecording { conn, req_id, record_dir, at_ms } => {
                    let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now) else {
                        continue;
                    };
                    let session_id = self.alloc_session_id();
                    let shard_idx = pick_least_loaded_shard(&self.shards);
                    self.pending_spawns = self.pending_spawns.saturating_add(1);
                    self.shards[shard_idx].send_with_permit(
                        ShardCmd::Rewind { session_id, conn, req_id, record_dir, at_ms },
                        Some(permit),
                    );
                }
                BrokerAction::SeekRecording { conn, req_id, session, at_ms } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session)
                        && let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now)
                    {
                        self.shards[shard_idx].send_with_permit(
                            ShardCmd::Seek { session_id: session, conn, req_id, at_ms },
                            Some(permit),
                        );
                    }
                }
                BrokerAction::SetSessionKept { conn, req_id, session, kept } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        if let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now) {
                            self.shards[shard_idx].send_with_permit(
                                ShardCmd::SetKept { session_id: session, conn, req_id, kept },
                                Some(permit),
                            );
                        } else if let Some(&prev) = self.session_kept.get(&session) {
                            self.core.set_session_kept(session, prev);
                        }
                    }
                }
                BrokerAction::WaitSession { conn, req_id, session, condition, timeout_ms } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session)
                        && let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now)
                    {
                        self.shards[shard_idx].send_with_permit(
                            ShardCmd::Wait { session_id: session, conn, req_id, condition, timeout_ms },
                            Some(permit),
                        );
                    }
                }
                BrokerAction::StreamAttached {
                    conn,
                    req_id,
                    stream_id,
                    attachment_id,
                    session,
                    flags,
                    initial_viewport,
                } => {
                    let Some(&shard_idx) = self.session_shard.get(&session) else {
                        self.core.abort_attach(conn, stream_id, attachment_id);
                        continue;
                    };
                    let Some(permit) = self.try_acquire_shard_permit(conn, StreamId::CONTROL, req_id, now) else {
                        self.core.abort_attach(conn, stream_id, attachment_id);
                        continue;
                    };
                    self.shards[shard_idx].send_with_permit(
                        ShardCmd::Attach {
                            session_id: session,
                            conn,
                            req_id,
                            stream_id,
                            attachment_id,
                            flags,
                            initial_viewport,
                        },
                        Some(permit),
                    );
                }
                BrokerAction::StreamDetached { conn, stream_id, attachment_id, session } => {
                    let _ = self.take_stalled_frame(conn, attachment_id);
                    if let Some(c) = self.conns.get_mut(&conn) {
                        c.queued_frame_streams.remove(&attachment_id);
                    }
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Detach {
                            session_id: session,
                            conn,
                            stream_id,
                            attachment_id,
                        });
                    }
                }
                BrokerAction::DispatchToSession { conn, stream_id, attachment_id, req_id, session, msg } => {
                    let interactive = is_interactive_input_msg(&msg);
                    if let Some(&shard_idx) = self.session_shard.get(&session)
                        && let Some(permit) =
                            self.try_acquire_permit_from(conn, stream_id.as_stream_id(), req_id, now, interactive)
                    {
                        self.shards[shard_idx].send_with_permit(
                            ShardCmd::StreamMsg { session_id: session, conn, stream_id, attachment_id, req_id, msg },
                            Some(permit),
                        );
                    }
                }
                BrokerAction::FrameCreditUpdated { conn, attachment_id, .. } => {
                    // Retry any stalled frame parked on the reactor for `attachment_id`.
                    // Never mint a `ViewSlotFree` token here for buffered frames: the shard's 1-slot
                    // token is released strictly when an outbound frame drains in `on_outbound_frame_drained`
                    // (or when a `DeferredResync` clears under budget headroom).
                    self.retry_stalled_frame(conn, attachment_id, now);
                }
                BrokerAction::SetPtyPaused { session, paused } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::SetPtyPaused { session_id: session, paused });
                    }
                }
                BrokerAction::TeardownNonKeptSessions => {
                    let pinned: HashSet<SessionId> =
                        self.session_shard.keys().copied().filter(|&s| self.core.is_session_pinned(s)).collect();
                    for shard in &self.shards {
                        shard.send(ShardCmd::TeardownNonKept { pinned: pinned.clone() });
                    }
                }
                BrokerAction::SessionUnpinned(session) => {
                    let broker_lease_active = self.core.has_active_broker_lease();
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx]
                            .send(ShardCmd::SessionUnpinned { session_id: session, broker_lease_active });
                    }
                }
                BrokerAction::TeardownSession(session) => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::TeardownSession { session_id: session });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod reactor_unit_tests {
    #[cfg(unix)]
    use super::*;
    #[cfg(unix)]
    use apack_core::auth::AuthConfig;
    #[cfg(unix)]
    use apack_os::transport::{bind_local_listener, connect_local_stream};
    #[cfg(unix)]
    use apack_proto::PROTOCOL_VERSION;
    #[cfg(unix)]
    use apack_proto::types::{CursorShape, CursorState, FrameCredit, GridSize, SubFlags, ViewModes};

    #[cfg(unix)]
    fn make_frame(text: &str) -> Box<FrameUpdate> {
        let mut rows = Vec::new();
        if !text.is_empty() {
            rows.push(apack_proto::types::PackedRow {
                line: 0,
                flags: 0,
                text: text.to_owned(),
                runs: Vec::new(),
                styles: Vec::new(),
                links: Vec::new(),
                pieces: Vec::new(),
            });
        }
        Box::new(FrameUpdate {
            seq: 0,
            epoch: 0,
            size: GridSize::new(80, 24).unwrap(),
            first_active_line: 0,
            scrollback_lines: 0,
            scroll_delta: 0,
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes::default(),
            palette: None,
            rows,
            images_gen: 0,
            full_sync: true,
        })
    }

    #[cfg(unix)]
    fn make_test_reactor(
        max_queue_bytes: usize,
    ) -> (tempfile::TempDir, String, ReactorState, Arc<Mutex<crate::shard::ShardMailboxState>>) {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("r.sock").to_string_lossy().into_owned();
        let listener = bind_local_listener(&sock).unwrap();
        set_listener_nonblocking(&listener, true).unwrap();
        let mut poller = PtyPoller::new().unwrap();
        poller.register(LISTENER_TOKEN, listener_fd(&listener), Interest::READ).unwrap();
        let (shard, mailbox) = ShardHandle::dummy_for_test();
        let state = ReactorState {
            core: BrokerCore::with_queue_cap(AuthConfig::None, max_queue_bytes),
            shards: vec![shard],
            image_store: Arc::new(Mutex::new(ImageStore::default())),
            conns: HashMap::new(),
            flushing_conns: HashSet::new(),
            next_conn_id: 1,
            next_session_id: 1,
            next_list_id: 1,
            session_shard: HashMap::new(),
            session_kept: HashMap::new(),
            pending_spawns: 0,
            pending_lists: HashMap::new(),
            ever_connected: false,
            started_at: Instant::now(),
            start_grace: Duration::from_secs(60),
            kill_escalate_after: Duration::from_secs(1),
            graceful_drain_timeout: Duration::from_millis(500),
            shutting_down: false,
            shutdown_deadline: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            wake_reactor: Arc::new(|| {}),
            listener,
            poller,
            token_to_conn: HashMap::new(),
            draining_unix: HashMap::new(),
            read_buf: vec![0u8; 4096],
            accept_backoff_until: None,
        };
        (tmp, sock, state, mailbox)
    }

    #[cfg(unix)]
    fn attach_conn_stream(
        sock: &str,
        state: &mut ReactorState,
        sid: SessionId,
        credit: FrameCredit,
    ) -> (ConnId, NonZeroStreamId, StreamAttachmentId, Stream) {
        let client_end = connect_local_stream(sock).unwrap();
        let server_end = state.listener.accept().unwrap();
        set_stream_nonblocking(&server_end, true).unwrap();
        let conn = state.alloc_conn_id();
        let tok = PollToken(conn.get());
        state.poller.register(tok, stream_fd(&server_end), Interest::READ).unwrap();
        state.token_to_conn.insert(tok, conn);
        let now = TimestampMs::from_millis(1);
        state.core.open_connection(conn, [0u8; 32], now).unwrap();
        state.conns.insert(
            conn,
            ConnState {
                stream: server_end,
                token: tok,
                write_queue: VecDeque::new(),
                write_queue_bytes: 0,
                stalled_frames: HashMap::new(),
                queued_frame_streams: HashMap::new(),
                inflight_shard_cmds: Arc::new(AtomicUsize::new(0)),
                inflight_input_cmds: Arc::new(AtomicUsize::new(0)),
                fin_pending: Arc::new(AtomicBool::new(false)),
            },
        );
        let mut wire = Vec::new();
        ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_nonce: [1u8; 32],
            client_name: String::from("test"),
        }
        .encode_to_frame(StreamId::CONTROL, RequestId::from_raw(1), &mut wire)
        .unwrap();
        let st = NonZeroStreamId::new(1).unwrap();
        ClientMsg::Attach {
            session: sid,
            stream_id: st,
            flags: SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(),
            initial_viewport: None,
            initial_output_credit: 0,
            initial_frame_credit: credit,
        }
        .encode_to_frame(StreamId::CONTROL, RequestId::from_raw(2), &mut wire)
        .unwrap();
        let mut actions = Vec::new();
        state.core.feed_bytes(conn, &wire, now, &mut actions);
        // Drain HelloOk from outbound so budget starts clean at 0.
        while state.core.pop_outbound(conn).is_some() {}
        let att = state.core.stream_attachment(conn, st).expect("attached");
        (conn, st, att, client_end)
    }

    #[cfg(unix)]
    #[test]
    fn pr3_reactor_emfile_backoff_stalled_budget_resync_fin_reap_and_stale_send() {
        let (_tmp, sock, mut state, mailbox) = make_test_reactor(512);
        let now = TimestampMs::from_millis(10);

        // 1. EMFILE backoff deadline & `maybe_resume_unix_accept` across force/deadline/exact-boundary/shutdown.
        let t0 = Instant::now();
        let dl = compute_accept_backoff_deadline(t0);
        assert_eq!(dl, t0 + ACCEPT_EMFILE_BACKOFF);

        let _ = state.poller.deregister(LISTENER_TOKEN);
        state.accept_backoff_until = Some(dl);
        state.maybe_resume_unix_accept(false, t0);
        assert!(state.accept_backoff_until.is_some(), "must not resume before deadline when force=false");
        state.maybe_resume_unix_accept(true, t0);
        assert!(state.accept_backoff_until.is_none(), "force=true must resume listener immediately");

        let _ = state.poller.deregister(LISTENER_TOKEN);
        state.accept_backoff_until = Some(dl);
        state.maybe_resume_unix_accept(false, dl);
        assert!(state.accept_backoff_until.is_none(), "exact deadline boundary (now == until) must resume listener");

        state.accept_backoff_until = Some(dl);
        state.shutting_down = true;
        state.maybe_resume_unix_accept(false, dl);
        assert!(state.accept_backoff_until.is_none(), "shutting_down must clear backoff without registering");
        state.shutting_down = false;

        // 2. Stalled frame MemoryBudget charging, DeferredResync, retry_budget_stalled_frames, and SessionEnded cleanup.
        let sid = SessionId::new(1).unwrap();
        state.session_shard.insert(sid, 0);
        let mut reg_actions = Vec::new();
        state.core.register_session(sid, SessionLifetime::Kept, false, &mut reg_actions);
        let (conn, st, att, _peer) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 0, max_bytes: 65_536 });

        // Pacer-stalled frame charges approx bytes against MemoryBudget; take_stalled_frame releases it!
        state.try_publish_or_stall_frame(conn, st, att, sid, make_frame("small"), now);
        let used = state.core.connection_budget_used(conn).unwrap();
        assert!(used > 0, "pacer-stalled frame must charge MemoryBudget");
        let taken = state.take_stalled_frame(conn, att).expect("stalled present");
        assert!(matches!(taken, StalledFrame::Buffered { reason: FrameStallReason::Pacer, .. }));
        assert_eq!(state.core.connection_budget_used(conn).unwrap(), 0, "take_stalled_frame must release budget");

        // Stalled frame exceeding 512B budget drops payload immediately into DeferredResync!
        let (conn2, st2, att2, _peer2) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 4, max_bytes: 65_536 });
        // Reserve 480 of 512 bytes so a ~200B frame fails both OutboundQueue and reserve_stalled_frame_bytes -> DeferredResync!
        assert!(state.core.reserve_stalled_frame_bytes(conn2, 480));
        state.try_publish_or_stall_frame(conn2, st2, att2, sid, make_frame(&"A".repeat(150)), now);
        assert!(matches!(
            state.conns[&conn2].stalled_frames.get(&att2),
            Some(StalledFrame::DeferredResync { reason: FrameStallReason::OutboundOrBudget, .. })
        ));
        // Still insufficient headroom (remaining=32 < needed): retry keeps DeferredResync as OutboundOrBudget!
        state.retry_budget_stalled_frames(conn2, now);
        assert!(matches!(
            state.conns[&conn2].stalled_frames.get(&att2),
            Some(StalledFrame::DeferredResync { reason: FrameStallReason::OutboundOrBudget, .. })
        ));
        assert!(mailbox.lock().unwrap().queue.is_empty());

        // When pacer closes (`max_frames = 0`) and not in queued_frame_streams, retry updates reason to Pacer!
        let mut ack_wire = Vec::new();
        ClientMsg::FrameAck { seq: 0, credit: FrameCredit { max_frames: 0, max_bytes: 65_536 } }
            .encode_to_frame(st2.as_stream_id(), RequestId::PUSH, &mut ack_wire)
            .unwrap();
        let mut ack_actions = Vec::new();
        state.core.feed_bytes(conn2, &ack_wire, now, &mut ack_actions);
        state.retry_stalled_frame(conn2, att2, now);
        assert!(matches!(
            state.conns[&conn2].stalled_frames.get(&att2),
            Some(StalledFrame::DeferredResync { reason: FrameStallReason::Pacer, .. })
        ));

        // When a P1 frame IS queued in queued_frame_streams, retry keeps reason as OutboundOrBudget!
        state.conns.get_mut(&conn2).unwrap().queued_frame_streams.insert(att2, (st2, sid));
        state.retry_stalled_frame(conn2, att2, now);
        assert!(matches!(
            state.conns[&conn2].stalled_frames.get(&att2),
            Some(StalledFrame::DeferredResync { reason: FrameStallReason::OutboundOrBudget, .. })
        ));
        state.conns.get_mut(&conn2).unwrap().queued_frame_streams.remove(&att2);

        // Re-open pacer + free budget -> retry_budget_stalled_frames emits ViewSlotFree { force_full_sync: true }!
        let mut ack_open = Vec::new();
        ClientMsg::FrameAck { seq: 0, credit: FrameCredit { max_frames: 4, max_bytes: 65_536 } }
            .encode_to_frame(st2.as_stream_id(), RequestId::PUSH, &mut ack_open)
            .unwrap();
        state.core.feed_bytes(conn2, &ack_open, now, &mut ack_actions);
        state.core.release_stalled_frame_bytes(conn2, 480);
        state.retry_budget_stalled_frames(conn2, now);
        assert!(!state.conns[&conn2].stalled_frames.contains_key(&att2));
        let cmd = mailbox.lock().unwrap().queue.pop_front().expect("ViewSlotFree sent").0;
        assert!(
            matches!(cmd, ShardCmd::ViewSlotFree { attachment_id, force_full_sync: true, .. } if attachment_id == att2)
        );

        // DeferredResync with stale attachment drops cleanly without emitting ViewSlotFree.
        let stale_att = StreamAttachmentId::new(999).unwrap();
        state.conns.get_mut(&conn2).unwrap().stalled_frames.insert(
            stale_att,
            StalledFrame::DeferredResync {
                stream_id: st2,
                session: sid,
                reason: FrameStallReason::OutboundOrBudget,
                needed_bytes: 1,
            },
        );
        state.retry_stalled_frame(conn2, stale_att, now);
        assert!(mailbox.lock().unwrap().queue.is_empty());

        // OutboundOrBudget buffered frame (P1 slot already occupied -> parks Buffered { OutboundOrBudget }),
        // then once P1 frame pops, retry_budget_stalled_frames publishes it!
        assert!(state.core.try_publish_frame(conn2, st2, att2, make_frame("p1"), now, &mut ack_actions).is_ok());
        state.try_publish_or_stall_frame(conn2, st2, att2, sid, make_frame("p2-buffered"), now);
        assert!(matches!(
            state.conns[&conn2].stalled_frames.get(&att2),
            Some(StalledFrame::Buffered { reason: FrameStallReason::OutboundOrBudget, charged_bytes, .. })
                if *charged_bytes > 0
        ));
        assert!(state.core.pop_outbound(conn2).is_some());
        state.retry_budget_stalled_frames(conn2, now);
        assert!(!state.conns[&conn2].stalled_frames.contains_key(&att2));

        // Pacer byte-window stall with `encoded_bytes != 0`: in-flight frame 1 consumes 65_530 of
        // 65_536 max_bytes so `can_send_any()` is true, `degrade_frame_to_wire_cap` measures exact
        // `encoded_bytes > 0`, `pacer.can_send` fails -> parks Buffered with exact `charged_bytes`!
        let (conn_bw, st_bw, att_bw, _p_bw) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 4, max_bytes: 65_536 });
        // Push frame 1 (already occupies ~100 bytes of pacer window); ack with tiny byte window?
        // MIN_BDP_FRAME_BYTES is 65_536, so fill pacer with a 65_500-byte frame on a 128 KiB budget reactor.
        drop((conn_bw, st_bw, att_bw, _p_bw));
        let (_tmp_bw, sock_bw, mut state_bw, _mb_bw) = make_test_reactor(262_144);
        state_bw.session_shard.insert(sid, 0);
        state_bw.core.register_session(sid, SessionLifetime::Kept, false, &mut Vec::new());
        let (cbw, sbw, abw, _pbw) =
            attach_conn_stream(&sock_bw, &mut state_bw, sid, FrameCredit { max_frames: 4, max_bytes: 65_536 });
        assert!(
            state_bw
                .core
                .try_publish_frame(cbw, sbw, abw, make_frame(&"Z".repeat(65_450)), now, &mut Vec::new())
                .is_ok()
        );
        assert!(state_bw.core.pop_outbound(cbw).is_some());
        state_bw.try_publish_or_stall_frame(cbw, sbw, abw, sid, make_frame("exact-charged"), now);
        let expected_exact = {
            let mut f = make_frame("exact-charged");
            f.seq = 2;
            let mut w = apack_proto::codec::Writer::new();
            f.encode(&mut w);
            apack_proto::FRAME_HEADER_LEN + w.as_slice().len()
        };
        assert!(matches!(
            state_bw.conns[&cbw].stalled_frames.get(&abw),
            Some(StalledFrame::Buffered { reason: FrameStallReason::Pacer, charged_bytes, .. })
                if *charged_bytes == expected_exact
        ));

        // 3. Stale attachment on ReactorEvent::SendMsg skips enqueue AND drops permit to finish pending FIN.
        //    First check on an open non-fin conn that stale SendMsg does NOT write any bytes to the socket!
        let (conn_live_stale, st_ls, _att_ls, mut peer_ls) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 1, max_bytes: 65_536 });
        set_stream_nonblocking(&peer_ls, true).unwrap();
        state.handle_shard_event(ReactorEvent::SendMsg {
            conn: conn_live_stale,
            stream_id: st_ls.as_stream_id(),
            attachment: Some(stale_att),
            req_id: RequestId::from_raw(54),
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Ok,
            permit: None,
        });
        assert!(state.core.pop_outbound(conn_live_stale).is_none(), "stale attachment must not enqueue outbound msg");
        let mut probe_buf = [0u8; 16];
        assert!(
            matches!(peer_ls.read(&mut probe_buf), Err(ref e) if e.kind() == io::ErrorKind::WouldBlock),
            "stale attachment must not flush any bytes to peer"
        );

        let permit = Arc::new(ShardCmdPermit::try_acquire(&state.conns[&conn].inflight_shard_cmds, 4).expect("permit"));
        state.conns[&conn].fin_pending.store(true, Ordering::Release);
        state.handle_shard_event(ReactorEvent::SendMsg {
            conn,
            stream_id: st.as_stream_id(),
            attachment: Some(stale_att),
            req_id: RequestId::from_raw(55),
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Ok,
            permit: Some(permit),
        });
        assert!(!state.conns.contains_key(&conn), "dropping final permit on stale SendMsg must close fin_pending conn");

        // 4. `reap_pending_fin_conns`: closes only `fin_pending && !has_inflight_cmds()`.
        let (conn_stay, _, _, _p3) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 1, max_bytes: 65_536 });
        let (conn_busy, _, _, _p4) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 1, max_bytes: 65_536 });
        let (conn_fin, _, _, _p5) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 1, max_bytes: 65_536 });
        let busy_permit = ShardCmdPermit::try_acquire(&state.conns[&conn_busy].inflight_shard_cmds, 4).unwrap();
        state.conns[&conn_busy].fin_pending.store(true, Ordering::Release);
        state.conns[&conn_fin].fin_pending.store(true, Ordering::Release);
        state.reap_pending_fin_conns(now);
        assert!(state.conns.contains_key(&conn_stay), "non-fin conn must stay open");
        assert!(state.conns.contains_key(&conn_busy), "fin_pending with inflight permit must stay open");
        assert!(!state.conns.contains_key(&conn_fin), "fin_pending with 0 inflight permits must close");
        drop(busy_permit);
        state.reap_pending_fin_conns(now);
        assert!(!state.conns.contains_key(&conn_busy), "fin_pending closes once permit drops");

        // 5. `ReactorEvent::SessionEnded` prunes stalled frames and releases charged MemoryBudget bytes.
        let (conn_end, st_end, att_end, _p6) =
            attach_conn_stream(&sock, &mut state, sid, FrameCredit { max_frames: 0, max_bytes: 65_536 });
        state.try_publish_or_stall_frame(conn_end, st_end, att_end, sid, make_frame("parked"), now);
        assert!(state.core.connection_budget_used(conn_end).unwrap() > 0);
        state.handle_shard_event(ReactorEvent::SessionEnded { session: sid });
        assert!(state.conns[&conn_end].stalled_frames.is_empty());
        assert_eq!(state.core.connection_budget_used(conn_end).unwrap(), 0);
    }
}
