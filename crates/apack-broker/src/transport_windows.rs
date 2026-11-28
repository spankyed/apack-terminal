//! Windows named-pipe transport edge (`#[cfg(windows)]`-only; mutated on Windows CI).

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use apack_core::broker::{CloseReason, StreamAttachmentId};
use apack_core::flow::QueuedMessage;
use apack_core::lease::ConnId;
use apack_os::transport::{hang_up, let_go, verify_client_is_current_user};
use apack_proto::messages::BrokerMsg;
use apack_proto::types::{NonZeroStreamId, RequestId, StreamId};
use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::{Listener, Stream};

use crate::transport_loop::{ConnState, MAX_CONNECTIONS, ReactorState, drain_deadline_expired};
use crate::{fill_random, now_monotonic_ms};

const WIN_CONN_READ_BUF_BYTES: usize = 32_768;

/// Small per-connection in-flight window so backpressure stays inside `BrokerCore`'s 16 MiB
/// priority-scheduled `OutboundQueue` and never blocks the single-threaded Control Reactor.
const WIN_WRITE_WINDOW: usize = 8;

/// Bounded capacity for Windows transport events sent from pipe threads to the Control Reactor.
pub(crate) const WIN_EVENT_CHANNEL_CAP: usize = 256;

/// Maximum Windows transport events drained per reactor turn before yielding to shard events & timers.
pub(crate) const MAX_WIN_EVENTS_PER_TURN: usize = 64;

/// Message sent to a Windows per-connection named-pipe writer thread.
pub(crate) enum WinWriteMsg {
    /// Complete wire-encoded frame bytes.
    Bytes(Vec<u8>),
    /// Flush and gracefully release the named pipe without aborting client reads.
    CloseGraceful,
}

/// Transport event bridged from Windows named-pipe threads into the single-threaded reactor.
pub(crate) enum WinTransportEvent {
    /// Newly accepted and user-verified client connection.
    Accepted(Stream),
    /// Raw bytes read from `ConnId`.
    Bytes(ConnId, Vec<u8>),
    /// Writer thread drained a chunk and freed a `WIN_WRITE_WINDOW` slot for `ConnId`.
    WriteReady(ConnId),
    /// Connection reached EOF or read/write error.
    Closed(ConnId),
}

/// Per-connection Windows pipe write/close handles held by the Control Reactor.
pub(crate) struct WinConnHandle {
    pub(crate) tx: mpsc::SyncSender<WinWriteMsg>,
    pub(crate) pending_chunk: Option<Vec<u8>>,
    pub(crate) writer_done: Arc<AtomicBool>,
    pub(crate) close_abrupt: Arc<dyn Fn() + Send + Sync>,
}

/// A closing Windows connection draining its final replies with a bounded wall-clock deadline so a
/// stalled peer can never leak reader/writer threads or pipe handles.
pub(crate) struct DrainingWinConn {
    pub(crate) win: WinConnHandle,
    pub(crate) tail_chunks: VecDeque<Vec<u8>>,
    pub(crate) close_sent: bool,
    pub(crate) deadline: Instant,
}

/// Returns `true` when a newly accepted Windows named-pipe connection satisfies shutdown and
/// connection-capacity checks.
pub(crate) fn can_accept_win_conn(shutting_down: bool, active_conns: usize) -> bool {
    !shutting_down && active_conns < MAX_CONNECTIONS
}

/// Returns `true` when closing a Windows named-pipe connection should flush gracefully (`let_go`)
/// rather than abruptly cancelling I/O (`hang_up`).
pub(crate) fn is_graceful_win_close(reason: CloseReason, has_final_error: bool) -> bool {
    matches!(reason, CloseReason::ClientFin) || has_final_error
}

/// Attempts a non-blocking push of encoded frame bytes to a Windows pipe writer channel.
/// Returns `Ok(true)` if queued, `Ok(false)` if the bounded window is full (parking `bytes` in
/// `pending` for retry on `WriteReady`), or `Err(())` if the writer thread has disconnected.
pub(crate) fn try_push_win_write(
    tx: &mpsc::SyncSender<WinWriteMsg>,
    pending: &mut Option<Vec<u8>>,
    bytes: Vec<u8>,
) -> Result<bool, ()> {
    match tx.try_send(WinWriteMsg::Bytes(bytes)) {
        Ok(()) => Ok(true),
        Err(mpsc::TrySendError::Full(WinWriteMsg::Bytes(buf))) => {
            *pending = Some(buf);
            Ok(false)
        }
        Err(_) => Err(()),
    }
}

/// Drains up to `limit` queued Windows transport events, then probes one additional event so the
/// caller knows whether more remain (`true` -> spin immediately on the next turn without sleeping).
pub(crate) fn drain_win_events_bounded<T>(rx: &mpsc::Receiver<T>, limit: usize, mut handle: impl FnMut(T)) -> bool {
    for _ in 0..limit {
        match rx.try_recv() {
            Ok(ev) => handle(ev),
            Err(_) => return false,
        }
    }
    match rx.try_recv() {
        Ok(ev) => {
            handle(ev);
            true
        }
        Err(_) => false,
    }
}

/// Clamps the reactor wait duration to zero when a turn hit the Windows transport event cap.
pub(crate) fn clamp_win_wait(wait_for: Duration, hit_cap: bool) -> Duration {
    if hit_cap { Duration::ZERO } else { wait_for }
}

/// Waits on the Windows reactor condvar until signalled or `wait_for` elapses.
pub(crate) fn wait_for_win_wake(win_cond: &(Mutex<bool>, Condvar), wait_for: Duration) {
    let (lock, cv) = win_cond;
    let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
    if !*woken {
        let (guard, _) = cv.wait_timeout(woken, wait_for).unwrap_or_else(|e| e.into_inner());
        woken = guard;
    }
    *woken = false;
}

/// Spawns the Windows named-pipe accept thread bridging connections to `ev_tx`.
pub(crate) fn spawn_win_accept_thread(
    listener: Arc<Listener>,
    shutdown: Arc<AtomicBool>,
    ev_tx: mpsc::SyncSender<WinTransportEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(String::from("apack-win-accept")).spawn(move || {
        while !shutdown.load(Ordering::Acquire) {
            match listener.accept() {
                Ok(stream) => {
                    if shutdown.load(Ordering::Acquire) {
                        let _ = hang_up(&stream);
                        break;
                    }
                    if matches!(verify_client_is_current_user(&stream), Ok(true)) {
                        let _ = ev_tx.send(WinTransportEvent::Accepted(stream));
                        wake();
                    } else {
                        let _ = hang_up(&stream);
                    }
                }
                Err(_) => {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    })
}

/// Spawns paired reader and writer threads for an accepted Windows named-pipe client stream.
pub(crate) fn spawn_win_conn_io(
    conn: ConnId,
    stream: Stream,
    ev_tx: mpsc::SyncSender<WinTransportEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> io::Result<WinConnHandle> {
    let stream = Arc::new(stream);
    let r_stream = Arc::clone(&stream);
    let w_stream = Arc::clone(&stream);
    let closed = Arc::new(AtomicBool::new(false));
    let r_closed = Arc::clone(&closed);
    let w_closed = Arc::clone(&closed);
    let reader_done = Arc::new(AtomicBool::new(false));
    let r_done = Arc::clone(&reader_done);
    let w_reader_done = Arc::clone(&reader_done);
    let writer_done = Arc::new(AtomicBool::new(false));
    let w_writer_done = Arc::clone(&writer_done);
    let (wtx, wrx) = mpsc::sync_channel::<WinWriteMsg>(WIN_WRITE_WINDOW);

    let r_ev_tx = ev_tx.clone();
    let r_wake = Arc::clone(&wake);
    if let Err(err) = std::thread::Builder::new().name(format!("apack-win-r-{}", conn.get())).spawn(move || {
        let mut buf = vec![0u8; WIN_CONN_READ_BUF_BYTES];
        let mut reader: &Stream = &r_stream;
        while !r_closed.load(Ordering::Acquire) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    if !r_closed.load(Ordering::Acquire) {
                        let _ = r_ev_tx.send(WinTransportEvent::Closed(conn));
                        r_wake();
                    }
                    break;
                }
                Ok(n) => {
                    if r_ev_tx.send(WinTransportEvent::Bytes(conn, buf[..n].to_vec())).is_err() {
                        break;
                    }
                    r_wake();
                }
            }
        }
        r_done.store(true, Ordering::Release);
    }) {
        let _ = hang_up(&stream);
        return Err(err);
    }

    if let Err(err) = std::thread::Builder::new().name(format!("apack-win-w-{}", conn.get())).spawn(move || {
        let mut writer: &Stream = &w_stream;
        while let Ok(msg) = wrx.recv() {
            match msg {
                WinWriteMsg::Bytes(bytes) => {
                    if writer.write_all(&bytes).is_err() {
                        let _ = ev_tx.send(WinTransportEvent::Closed(conn));
                        wake();
                        break;
                    }
                    let _ = ev_tx.send(WinTransportEvent::WriteReady(conn));
                    wake();
                }
                WinWriteMsg::CloseGraceful => {
                    break;
                }
            }
        }
        // Signal the reader loop to stop, cancel any parked overlapped ReadFile (retrying if the
        // reader entered ReadFile right after the first CancelIoEx), and mark `assume_flushed`
        // before dropping `w_stream` so the server pipe handle closes promptly and never hangs in
        // `FlushFileBuffers`.
        w_closed.store(true, Ordering::Release);
        let _ = let_go(&w_stream, || {});
        for _ in 0..64 {
            if w_reader_done.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
            let _ = let_go(&w_stream, || {});
        }
        w_writer_done.store(true, Ordering::Release);
        wake();
    }) {
        closed.store(true, Ordering::Release);
        let _ = hang_up(&stream);
        return Err(err);
    }

    let abrupt_tx = wtx.clone();
    let hangup_stream = Arc::clone(&stream);
    Ok(WinConnHandle {
        tx: wtx,
        pending_chunk: None,
        writer_done,
        close_abrupt: Arc::new(move || {
            let _ = abrupt_tx.try_send(WinWriteMsg::CloseGraceful);
            let _ = hang_up(&hangup_stream);
        }),
    })
}

impl ReactorState {
    pub(crate) fn handle_win_transport_event(&mut self, ev: WinTransportEvent) {
        match ev {
            WinTransportEvent::Accepted(stream) => {
                let active_total = self.conns.len().saturating_add(self.draining_win.len());
                if !can_accept_win_conn(self.shutting_down, active_total) {
                    let _ = hang_up(&stream);
                    return;
                }
                let mut nonce = [0u8; 32];
                if fill_random(&mut nonce).is_err() {
                    let _ = hang_up(&stream);
                    return;
                }
                let conn = self.alloc_conn_id();
                let win = match spawn_win_conn_io(conn, stream, self.win_ev_tx.clone(), Arc::clone(&self.win_wake)) {
                    Ok(win) => win,
                    Err(_) => return,
                };
                let now = now_monotonic_ms();
                let _ = self.core.open_connection(conn, nonce, now);
                self.conns.insert(
                    conn,
                    ConnState {
                        win,
                        stalled_frames: HashMap::new(),
                        queued_frame_streams: HashMap::new(),
                        inflight_shard_cmds: Arc::new(AtomicUsize::new(0)),
                        inflight_input_cmds: Arc::new(AtomicUsize::new(0)),
                        fin_pending: Arc::new(AtomicBool::new(false)),
                    },
                );
            }
            WinTransportEvent::Bytes(conn, bytes) => {
                self.sync_shard_telemetry();
                let now = now_monotonic_ms();
                let mut actions = Vec::new();
                self.core.feed_bytes(conn, &bytes, now, &mut actions);
                if self.core.is_lifecycle_client_ready(conn) {
                    self.ever_connected = true;
                }
                self.execute_actions(actions, now);
            }
            WinTransportEvent::WriteReady(conn) => {
                if self.draining_win.contains_key(&conn) {
                    self.pump_draining_win(conn);
                } else {
                    self.flush_win_writes(conn);
                }
            }
            WinTransportEvent::Closed(conn) => {
                if let Some(d) = self.draining_win.remove(&conn) {
                    (d.win.close_abrupt)();
                } else {
                    self.on_transport_closed(conn);
                }
            }
        }
    }

    pub(crate) fn begin_win_close(
        &mut self,
        conn: ConnId,
        reason: CloseReason,
        drained: Vec<QueuedMessage>,
        final_error: Option<(StreamId, RequestId, BrokerMsg)>,
    ) {
        let Some(c) = self.conns.remove(&conn) else {
            return;
        };
        let graceful = is_graceful_win_close(reason, final_error.is_some());
        if !graceful {
            (c.win.close_abrupt)();
            return;
        }
        let mut win = c.win;
        let mut tail_chunks = VecDeque::new();
        if let Some(pending) = win.pending_chunk.take() {
            tail_chunks.push_back(pending);
        }
        for queued in drained {
            let mut encoded = Vec::with_capacity(queued.encoded_bytes);
            if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_ok() {
                tail_chunks.push_back(encoded);
            }
        }
        if let Some((stream_id, req_id, msg)) = final_error {
            let mut encoded = Vec::new();
            if msg.encode_to_frame(stream_id, req_id, &mut encoded).is_ok() {
                tail_chunks.push_back(encoded);
            }
        }
        self.draining_win.insert(
            conn,
            DrainingWinConn {
                win,
                tail_chunks,
                close_sent: false,
                deadline: Instant::now() + self.graceful_drain_timeout,
            },
        );
        self.pump_draining_win(conn);
    }

    pub(crate) fn pump_draining_win(&mut self, conn: ConnId) {
        let Some(d) = self.draining_win.get_mut(&conn) else {
            return;
        };
        while let Some(chunk) = d.tail_chunks.pop_front() {
            match d.win.tx.try_send(WinWriteMsg::Bytes(chunk)) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(WinWriteMsg::Bytes(buf))) => {
                    d.tail_chunks.push_front(buf);
                    return;
                }
                Err(_) => {
                    if let Some(removed) = self.draining_win.remove(&conn) {
                        (removed.win.close_abrupt)();
                    }
                    return;
                }
            }
        }
        if !d.close_sent {
            match d.win.tx.try_send(WinWriteMsg::CloseGraceful) {
                Ok(()) => d.close_sent = true,
                Err(mpsc::TrySendError::Full(_)) => {}
                Err(_) => {
                    if let Some(removed) = self.draining_win.remove(&conn) {
                        (removed.win.close_abrupt)();
                    }
                }
            }
        }
    }

    pub(crate) fn has_draining_win(&self) -> bool {
        !self.draining_win.is_empty()
    }

    pub(crate) fn reap_draining_win_conns(&mut self) {
        let now = Instant::now();
        let expired: Vec<ConnId> = self
            .draining_win
            .iter()
            .filter_map(|(&conn, d)| {
                if d.win.writer_done.load(Ordering::Acquire) || drain_deadline_expired(now, d.deadline) {
                    Some(conn)
                } else {
                    None
                }
            })
            .collect();
        for conn in expired {
            if let Some(d) = self.draining_win.remove(&conn)
                && !d.win.writer_done.load(Ordering::Acquire)
            {
                (d.win.close_abrupt)();
            }
        }
    }

    pub(crate) fn flush_win_writes(&mut self, conn: ConnId) {
        if !self.flushing_conns.insert(conn) {
            return;
        }
        while self.push_next_win_frame(conn) {}
        self.flushing_conns.remove(&conn);
    }

    fn push_next_win_frame(&mut self, conn: ConnId) -> bool {
        let mut drained_frame: Option<(StreamAttachmentId, NonZeroStreamId, apack_proto::types::SessionId)> = None;
        let next = if let Some(pending) = self.conns.get_mut(&conn).and_then(|c| c.win.pending_chunk.take()) {
            Some(pending)
        } else if let Some(queued) = self.core.pop_outbound(conn) {
            let is_frame = matches!(queued.msg, BrokerMsg::Frame(_));
            if is_frame
                && let Some(att) = queued.attachment_id
                && let Some((nz, session_id)) =
                    self.conns.get_mut(&conn).and_then(|c| c.queued_frame_streams.remove(&att))
            {
                drained_frame = Some((att, nz, session_id));
            }
            let mut encoded = Vec::with_capacity(queued.encoded_bytes);
            if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_err() {
                if let Some((att, nz, session_id)) = drained_frame {
                    self.on_outbound_frame_drained(conn, nz, att, session_id);
                }
                return true;
            }
            Some(encoded)
        } else {
            None
        };
        let Some(bytes) = next else {
            return false;
        };
        let res = self.conns.get_mut(&conn).map(|c| try_push_win_write(&c.win.tx, &mut c.win.pending_chunk, bytes));
        let sent = match res {
            Some(Ok(sent)) => sent,
            _ => {
                self.on_transport_closed(conn);
                return false;
            }
        };
        // Only drain the frame slot and retry stalled frames AFTER `bytes` is queued ahead of
        // `BrokerCore::outbound` (either in `c.win.tx` or `c.win.pending_chunk`), preserving strict
        // FIFO frame sequence order!
        let skip_att = if let Some((att, nz, session_id)) = drained_frame {
            self.on_outbound_frame_drained(conn, nz, att, session_id);
            Some(att)
        } else {
            None
        };
        if sent {
            self.retry_budget_stalled_frames(conn, skip_att, now_monotonic_ms());
        }
        sent
    }
}
