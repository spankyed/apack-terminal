//! Single-threaded Control & Transport Reactor driving sans-IO [`BrokerCore`].
//!
//! - On **Unix**, one [`apack_os::pty_poller::PtyPoller`] multiplexes the non-blocking local socket
//!   listener AND every accepted client connection fd (`writev` scatter-gather writes, zero
//!   per-connection threads!).
//! - On **Windows**, a thin accept thread and per-connection blocking reader/writer threads bridge
//!   named-pipe `Stream`s into the exact same single-threaded `BrokerCore` loop (keeping the
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` listener alive during shutdown).

#[cfg(unix)]
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
#[cfg(unix)]
use std::io::IoSlice;
use std::io::{self, Read, Write};
#[cfg(windows)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use apack_core::broker::{BrokerAction, BrokerCore, CloseReason, ShardTelemetry};
use apack_core::flow::OutboundPriority;
use apack_core::lease::ConnId;
use apack_core::time::TimestampMs;
#[cfg(unix)]
use apack_os::pty_poller::{Interest, PollEvent, PollToken, PtyPoller};
#[cfg(unix)]
use apack_os::transport::{
    hang_up, listener_raw_fd, set_listener_nonblocking, set_stream_nonblocking, stream_raw_fd,
    verify_peer_is_current_user, write_vectored_some,
};
use apack_proto::messages::{BrokerMsg, FrameUpdate, SessionSummary};
use apack_proto::types::{NonZeroStreamId, RequestId, SessionId, StreamId};
use apack_vt_ghostty::ImageStore;
use interprocess::local_socket::Listener;
#[cfg(unix)]
use interprocess::local_socket::Stream;
#[cfg(unix)]
use interprocess::local_socket::traits::Listener as _;

use crate::config::BrokerConfig;
use crate::shard::{ReactorEvent, ShardCmd, ShardHandle, ShardShared, spawn_shard};
#[cfg(windows)]
use crate::transport_windows::{
    WinConnHandle, WinTransportEvent, WinWriteMsg, spawn_win_accept_thread, wait_for_win_wake,
};
use crate::{engine_abi_hash, fill_random, now_unix_ms};

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

/// In-flight `ListSessions` aggregation across all $K$ shards.
struct PendingList {
    remaining_shards: usize,
    collected: Vec<SessionSummary>,
}

/// Per-connection stalled view frames awaiting `FramePacer` credit or outbound queue clearance
/// (at most 1 frame per stream — guarantees zero lost differential frames!).
pub(crate) struct ConnState {
    #[cfg(unix)]
    stream: Stream,
    #[cfg(unix)]
    token: PollToken,
    #[cfg(unix)]
    write_queue: VecDeque<(Vec<u8>, usize)>,
    #[cfg(windows)]
    pub(crate) win: WinConnHandle,
    pub(crate) stalled_frames: HashMap<NonZeroStreamId, (SessionId, Box<FrameUpdate>)>,
    /// Set of `(stream_id, session_id)` whose view frame is currently queued in `BrokerCore::outbound`.
    pub(crate) queued_frame_streams: HashMap<NonZeroStreamId, SessionId>,
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
        poller.register(LISTENER_TOKEN, listener_raw_fd(&listener), Interest::READ)?;
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
    let (win_ev_tx, win_ev_rx) = mpsc::channel::<WinTransportEvent>();
    #[cfg(windows)]
    let listener = Arc::new(listener);
    #[cfg(windows)]
    let _accept_thread = spawn_win_accept_thread(
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
        next_conn_id: 1,
        next_session_id: 1,
        session_shard: HashMap::new(),
        session_kept: HashMap::new(),
        pending_lists: HashMap::new(),
        shard_scrollback: vec![0u64; shard_count],
        shard_pty_bytes: vec![0u64; shard_count],
        ever_connected: false,
        started_at: Instant::now(),
        start_grace: config.start_grace,
        shutting_down: false,
        shutdown_flag,
        #[cfg(unix)]
        listener,
        #[cfg(unix)]
        poller,
        #[cfg(unix)]
        token_to_conn: HashMap::new(),
        #[cfg(unix)]
        read_buf: vec![0u8; CONN_READ_BUF_BYTES],
        #[cfg(windows)]
        win_ev_tx,
        #[cfg(windows)]
        win_wake: wake_fn,
    };

    #[cfg(unix)]
    let mut poll_events = Vec::with_capacity(64);

    while !state.should_exit() {
        if state.shutdown_flag.load(Ordering::Acquire) && !state.shutting_down {
            state.begin_shutdown();
        }

        // 1. Drain all shard events first so replies & frames flush promptly.
        while let Ok(ev) = reactor_rx.try_recv() {
            state.handle_shard_event(ev);
        }

        // 2. Drain Windows transport events (if Windows).
        #[cfg(windows)]
        while let Ok(ev) = win_ev_rx.try_recv() {
            state.handle_win_transport_event(ev);
        }

        // 3. Fire due BrokerCore timers (handshake timeouts, remote lease grace timers).
        let now = TimestampMs::from_millis(now_unix_ms());
        let mut actions = Vec::new();
        state.core.poll_timers(now, &mut actions);
        state.execute_actions(actions, now);

        // 4. Check start_grace (exit cleanly if no client ever connected).
        if !state.ever_connected && state.started_at.elapsed() >= state.start_grace {
            state.begin_shutdown();
        }

        // 5. Auto-exit when all clients disconnected, no broker lease holds us, and no sessions remain.
        if state.ever_connected
            && state.core.connection_count() == 0
            && !state.core.has_active_broker_lease()
            && state.session_shard.is_empty()
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
            state.shutting_down,
        );

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
        // Keep listener alive until all shards have drained; wake accept thread via a harmless connect.
        state.shutdown_flag.store(true, Ordering::Release);
        let _ = apack_os::transport::connect_local_stream(&config.socket_path);
        drop(listener);
    }

    Ok(())
}

pub(crate) struct ReactorState {
    pub(crate) core: BrokerCore,
    shards: Vec<ShardHandle>,
    image_store: Arc<Mutex<ImageStore>>,
    pub(crate) conns: HashMap<ConnId, ConnState>,
    next_conn_id: u64,
    next_session_id: u64,
    session_shard: HashMap<SessionId, usize>,
    session_kept: HashMap<SessionId, bool>,
    pending_lists: HashMap<(ConnId, RequestId), PendingList>,
    shard_scrollback: Vec<u64>,
    shard_pty_bytes: Vec<u64>,
    pub(crate) ever_connected: bool,
    started_at: Instant,
    start_grace: Duration,
    shutting_down: bool,
    shutdown_flag: Arc<AtomicBool>,
    #[cfg(unix)]
    listener: Listener,
    #[cfg(unix)]
    poller: PtyPoller,
    #[cfg(unix)]
    token_to_conn: HashMap<PollToken, ConnId>,
    #[cfg(unix)]
    read_buf: Vec<u8>,
    #[cfg(windows)]
    pub(crate) win_ev_tx: mpsc::Sender<WinTransportEvent>,
    #[cfg(windows)]
    pub(crate) win_wake: Arc<dyn Fn() + Send + Sync>,
}

impl ReactorState {
    fn should_exit(&self) -> bool {
        self.shutting_down && self.session_shard.is_empty()
    }

    fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        self.shutdown_flag.store(true, Ordering::Release);
        for shard in &self.shards {
            shard.send(ShardCmd::TeardownNonKept { pinned: HashSet::new() });
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

    #[cfg(unix)]
    fn handle_unix_events(&mut self, events: &[PollEvent]) {
        for ev in events {
            if ev.token == LISTENER_TOKEN {
                self.accept_unix_connections();
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
    fn accept_unix_connections(&mut self) {
        loop {
            match self.listener.accept() {
                Ok(stream) => {
                    if !matches!(verify_peer_is_current_user(&stream), Ok(true)) {
                        let _ = hang_up(&stream);
                        continue;
                    }
                    if set_stream_nonblocking(&stream, true).is_err() {
                        let _ = hang_up(&stream);
                        continue;
                    }
                    let conn = self.alloc_conn_id();
                    let token = PollToken(conn.get());
                    if self.poller.register(token, stream_raw_fd(&stream), Interest::READ).is_err() {
                        let _ = hang_up(&stream);
                        continue;
                    }
                    let mut nonce = [0u8; 32];
                    let _ = fill_random(&mut nonce);
                    let now = TimestampMs::from_millis(now_unix_ms());
                    let _ = self.core.open_connection(conn, nonce, now);
                    self.token_to_conn.insert(token, conn);
                    self.conns.insert(
                        conn,
                        ConnState {
                            stream,
                            token,
                            write_queue: VecDeque::new(),
                            stalled_frames: HashMap::new(),
                            queued_frame_streams: HashMap::new(),
                        },
                    );
                }
                Err(err) => match classify_nonblocking_err(&err) {
                    NonBlockErr::Interrupted => continue,
                    NonBlockErr::WouldBlock | NonBlockErr::Fatal => break,
                },
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
                    let now = TimestampMs::from_millis(now_unix_ms());
                    let mut actions = Vec::new();
                    self.core.feed_bytes(conn, &self.read_buf[..n], now, &mut actions);
                    if self.core.is_connection_ready(conn) {
                        self.ever_connected = true;
                    }
                    self.execute_actions(actions, now);
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
        loop {
            // Pull outbound messages from BrokerCore (bounded by `max_queue_bytes`) into the writev queue.
            while let Some(queued) = self.core.pop_outbound(conn) {
                let mut encoded = Vec::with_capacity(queued.encoded_bytes);
                if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_err() {
                    continue;
                }
                if let Some(c) = self.conns.get_mut(&conn) {
                    // If this popped a view Frame, free its reactor-queued slot and notify/retry!
                    if let Some(nz) = NonZeroStreamId::new(queued.stream_id.raw())
                        && let Some(session_id) = c.queued_frame_streams.remove(&nz)
                    {
                        self.on_outbound_frame_drained(conn, nz, session_id);
                    }
                    if let Some(c) = self.conns.get_mut(&conn) {
                        c.write_queue.push_back((encoded, 0));
                    }
                }
            }

            let Some(c) = self.conns.get_mut(&conn) else {
                return;
            };
            if c.write_queue.is_empty() {
                let _ = self.poller.modify(c.token, Interest::READ);
                return;
            }

            let slices: Vec<IoSlice<'_>> =
                c.write_queue.iter().take(MAX_WRITEV_IOVS).map(|(buf, off)| IoSlice::new(&buf[*off..])).collect();

            match write_vectored_some(&mut c.stream, &slices) {
                Ok(0) => {
                    self.on_transport_closed(conn);
                    return;
                }
                Ok(written) => advance_write_queue(&mut c.write_queue, written),
                Err(err) => match classify_nonblocking_err(&err) {
                    NonBlockErr::WouldBlock => {
                        let _ = self.poller.modify(c.token, Interest::READ_WRITE);
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

    pub(crate) fn on_outbound_frame_drained(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        session_id: SessionId,
    ) {
        // First retry any stalled frame held on the reactor for this stream.
        let stalled = self.conns.get_mut(&conn).and_then(|c| c.stalled_frames.remove(&stream_id));
        if let Some((sid, frame)) = stalled {
            let now = TimestampMs::from_millis(now_unix_ms());
            self.try_publish_or_stall_frame(conn, stream_id, sid, frame, now);
        } else if let Some(&shard_idx) = self.session_shard.get(&session_id) {
            self.shards[shard_idx].send(ShardCmd::ViewSlotFree { session_id, conn, stream_id });
        }
    }

    fn try_publish_or_stall_frame(
        &mut self,
        conn: ConnId,
        stream_id: NonZeroStreamId,
        session: SessionId,
        frame: Box<FrameUpdate>,
        now: TimestampMs,
    ) {
        if !self.conns.contains_key(&conn) {
            return;
        }
        let mut actions = Vec::new();
        if self.core.publish_frame(conn, stream_id, frame.clone(), now, &mut actions) {
            if let Some(c) = self.conns.get_mut(&conn) {
                c.queued_frame_streams.insert(stream_id, session);
            }
            self.execute_actions(actions, now);
        } else if let Some(c) = self.conns.get_mut(&conn) {
            // Park this single frame until FrameAck or outbound drain frees credit; shard stays busy.
            c.stalled_frames.insert(stream_id, (session, frame));
        }
    }

    pub(crate) fn on_transport_closed(&mut self, conn: ConnId) {
        self.close_conn_transport(conn);
        let now = TimestampMs::from_millis(now_unix_ms());
        let mut actions = Vec::new();
        self.core.close_connection(conn, CloseReason::TransportClosed, now, &mut actions);
        // Cancel any pending Wait requests owned by this connection across all shards.
        for shard in &self.shards {
            shard.send(ShardCmd::CancelConnWaiters { conn });
        }
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
        }
        #[cfg(windows)]
        {
            (c.win.hangup)();
        }
    }

    fn handle_shard_event(&mut self, event: ReactorEvent) {
        let now = TimestampMs::from_millis(now_unix_ms());
        match event {
            ReactorEvent::SendMsg { conn, stream_id, req_id, priority, msg } => {
                let mut actions = Vec::new();
                self.core.enqueue_message(conn, stream_id, req_id, priority, msg, now, &mut actions);
                self.execute_actions(actions, now);
            }
            ReactorEvent::PublishFrame { conn, stream_id, session, frame } => {
                self.try_publish_or_stall_frame(conn, stream_id, session, frame, now);
            }
            ReactorEvent::PublishRawOutput { session, bytes } => {
                let mut actions = Vec::new();
                self.core.publish_raw_output(session, &bytes, now, &mut actions);
                self.execute_actions(actions, now);
            }
            ReactorEvent::SessionRegistered { session, shard_idx, rewound, kept } => {
                self.session_shard.insert(session, shard_idx);
                self.session_kept.insert(session, kept);
                self.core.register_session(session, rewound, kept);
            }
            ReactorEvent::SessionKeptUpdated { session, kept } => {
                self.session_kept.insert(session, kept);
                self.core.set_session_kept(session, kept);
            }
            ReactorEvent::SessionEnded { session } => {
                self.session_shard.remove(&session);
                self.session_kept.remove(&session);
                let mut actions = Vec::new();
                self.core.unregister_session(session, "session ended", now, &mut actions);
                self.execute_actions(actions, now);
            }
            ReactorEvent::SessionListPart { conn, req_id, sessions } => {
                let key = (conn, req_id);
                let ready = if let Some(pending) = self.pending_lists.get_mut(&key) {
                    pending.collected.extend(sessions);
                    pending.remaining_shards = pending.remaining_shards.saturating_sub(1);
                    pending.remaining_shards == 0
                } else {
                    false
                };
                if ready && let Some(mut pending) = self.pending_lists.remove(&key) {
                    pending.collected.sort_by_key(|s| s.id);
                    let mut actions = Vec::new();
                    self.core.enqueue_message(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        OutboundPriority::Control,
                        BrokerMsg::SessionsList { sessions: pending.collected },
                        now,
                        &mut actions,
                    );
                    self.execute_actions(actions, now);
                }
            }
            ReactorEvent::Telemetry { shard_idx, scrollback_bytes, total_pty_bytes } => {
                if let Some(slot) = self.shard_scrollback.get_mut(shard_idx) {
                    *slot = scrollback_bytes;
                }
                if let Some(slot) = self.shard_pty_bytes.get_mut(shard_idx) {
                    *slot = total_pty_bytes;
                }
                let img_bytes = self.image_store.lock().unwrap_or_else(|e| e.into_inner()).used_bytes() as u64;
                self.core.update_shard_telemetry(ShardTelemetry {
                    scrollback_bytes: self.shard_scrollback.iter().sum(),
                    image_store_bytes: img_bytes,
                    total_pty_bytes: self.shard_pty_bytes.iter().sum(),
                });
            }
        }
    }

    pub(crate) fn execute_actions(&mut self, actions: Vec<BrokerAction>, now: TimestampMs) {
        for action in actions {
            match action {
                BrokerAction::OutboundReady(conn) => {
                    self.flush_writes(conn);
                }
                BrokerAction::CloseConn { conn, final_error, .. } => {
                    if let Some((stream_id, req_id, msg)) = final_error {
                        let mut encoded = Vec::new();
                        if msg.encode_to_frame(stream_id, req_id, &mut encoded).is_ok() {
                            #[cfg(unix)]
                            if let Some(c) = self.conns.get_mut(&conn) {
                                let _ = c.stream.write_all(&encoded);
                            }
                            #[cfg(windows)]
                            if let Some(c) = self.conns.get(&conn) {
                                let _ = c.win.tx.try_send(WinWriteMsg::Bytes(encoded));
                            }
                        }
                    }
                    self.on_transport_closed(conn);
                }
                BrokerAction::SpawnSession { conn, req_id, spec } => {
                    let session_id = self.alloc_session_id();
                    let shard_idx = pick_least_loaded_shard(&self.shards);
                    self.shards[shard_idx].send(ShardCmd::Spawn { session_id, conn, req_id, spec });
                }
                BrokerAction::KillSession { conn, req_id, session } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Kill { session_id: session, conn, req_id });
                    }
                }
                BrokerAction::SignalSession { conn, req_id, session, signal, scope } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Signal {
                            session_id: session,
                            conn,
                            req_id,
                            signal,
                            scope,
                        });
                    }
                }
                BrokerAction::QueryOwns { conn, req_id, session, pid } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Owns { session_id: session, conn, req_id, pid });
                    }
                }
                BrokerAction::ListSessions { conn, req_id } => {
                    self.pending_lists.insert(
                        (conn, req_id),
                        PendingList { remaining_shards: self.shards.len(), collected: Vec::new() },
                    );
                    for shard in &self.shards {
                        shard.send(ShardCmd::ListSessions { conn, req_id });
                    }
                }
                BrokerAction::RewindRecording { conn, req_id, record_dir, at_ms } => {
                    let session_id = self.alloc_session_id();
                    let shard_idx = pick_least_loaded_shard(&self.shards);
                    self.shards[shard_idx].send(ShardCmd::Rewind { session_id, conn, req_id, record_dir, at_ms });
                }
                BrokerAction::SeekRecording { conn, req_id, session, at_ms } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Seek { session_id: session, conn, req_id, at_ms });
                    }
                }
                BrokerAction::SetSessionKept { conn, req_id, session, kept } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::SetKept { session_id: session, conn, req_id, kept });
                    }
                }
                BrokerAction::WaitSession { conn, req_id, session, condition, timeout_ms } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Wait {
                            session_id: session,
                            conn,
                            req_id,
                            condition,
                            timeout_ms,
                        });
                    }
                }
                BrokerAction::StreamAttached { conn, req_id, stream_id, session, flags, initial_viewport } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Attach {
                            session_id: session,
                            conn,
                            req_id,
                            stream_id,
                            flags,
                            initial_viewport,
                        });
                    }
                }
                BrokerAction::StreamDetached { conn, stream_id, session } => {
                    if let Some(c) = self.conns.get_mut(&conn) {
                        c.stalled_frames.remove(&stream_id);
                        c.queued_frame_streams.remove(&stream_id);
                    }
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::Detach { session_id: session, conn, stream_id });
                    }
                }
                BrokerAction::DispatchToSession { conn, stream_id, req_id, session, msg } => {
                    if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::StreamMsg {
                            session_id: session,
                            conn,
                            stream_id,
                            req_id,
                            msg,
                        });
                    }
                }
                BrokerAction::FrameCreditUpdated { conn, stream_id, session, .. } => {
                    // Retry any stalled frame for `(conn, stream_id)`; otherwise notify shard slot is free.
                    let stalled = self.conns.get_mut(&conn).and_then(|c| c.stalled_frames.remove(&stream_id));
                    if let Some((sid, frame)) = stalled {
                        self.try_publish_or_stall_frame(conn, stream_id, sid, frame, now);
                    } else if let Some(&shard_idx) = self.session_shard.get(&session) {
                        self.shards[shard_idx].send(ShardCmd::ViewSlotFree { session_id: session, conn, stream_id });
                    }
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
            }
        }
    }
}
