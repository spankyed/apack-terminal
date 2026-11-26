//! Windows named-pipe transport edge (`#[cfg(windows)]`-only; mutated on Windows CI).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use apack_core::broker::CloseReason;
use apack_core::lease::ConnId;
use apack_core::time::TimestampMs;
use apack_os::transport::{hang_up, let_go, verify_peer_is_current_user};
use apack_proto::messages::BrokerMsg;
use apack_proto::types::NonZeroStreamId;
use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::{Listener, Stream};

use crate::transport_loop::{ConnState, MAX_CONNECTIONS, ReactorState, ready_stalled_stream_ids};
use crate::{fill_random, now_unix_ms};

const WIN_CONN_READ_BUF_BYTES: usize = 32_768;

/// Small per-connection in-flight window so backpressure stays inside `BrokerCore`'s 16 MiB
/// priority-scheduled `OutboundQueue` and never blocks the single-threaded Control Reactor.
const WIN_WRITE_WINDOW: usize = 8;

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
    pub(crate) close_graceful: Arc<dyn Fn() + Send + Sync>,
    pub(crate) close_abrupt: Arc<dyn Fn() + Send + Sync>,
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
    ev_tx: mpsc::Sender<WinTransportEvent>,
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
                    if matches!(verify_peer_is_current_user(&stream), Ok(true)) {
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
    ev_tx: mpsc::Sender<WinTransportEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> WinConnHandle {
    let stream = Arc::new(stream);
    let r_stream = Arc::clone(&stream);
    let w_stream = Arc::clone(&stream);
    let closed = Arc::new(AtomicBool::new(false));
    let r_closed = Arc::clone(&closed);
    let w_closed = Arc::clone(&closed);
    let reader_done = Arc::new(AtomicBool::new(false));
    let r_done = Arc::clone(&reader_done);
    let w_reader_done = Arc::clone(&reader_done);
    let (wtx, wrx) = mpsc::sync_channel::<WinWriteMsg>(WIN_WRITE_WINDOW);

    let r_ev_tx = ev_tx.clone();
    let r_wake = Arc::clone(&wake);
    let _ = std::thread::Builder::new().name(format!("apack-win-r-{}", conn.get())).spawn(move || {
        let mut buf = vec![0u8; WIN_CONN_READ_BUF_BYTES];
        let mut reader: &Stream = &r_stream;
        while !r_closed.load(Ordering::Acquire) {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = r_ev_tx.send(WinTransportEvent::Closed(conn));
                    r_wake();
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
    });

    let _ = std::thread::Builder::new().name(format!("apack-win-w-{}", conn.get())).spawn(move || {
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
    });

    let close_tx = wtx.clone();
    let abrupt_tx = wtx.clone();
    let hangup_stream = Arc::clone(&stream);
    WinConnHandle {
        tx: wtx,
        pending_chunk: None,
        close_graceful: Arc::new(move || {
            let _ = close_tx.try_send(WinWriteMsg::CloseGraceful);
        }),
        close_abrupt: Arc::new(move || {
            let _ = abrupt_tx.try_send(WinWriteMsg::CloseGraceful);
            let _ = hang_up(&hangup_stream);
        }),
    }
}

impl ReactorState {
    pub(crate) fn handle_win_transport_event(&mut self, ev: WinTransportEvent) {
        match ev {
            WinTransportEvent::Accepted(stream) => {
                if !can_accept_win_conn(self.shutting_down, self.conns.len()) {
                    let _ = hang_up(&stream);
                    return;
                }
                let mut nonce = [0u8; 32];
                if fill_random(&mut nonce).is_err() {
                    let _ = hang_up(&stream);
                    return;
                }
                let conn = self.alloc_conn_id();
                let now = TimestampMs::from_millis(now_unix_ms());
                let _ = self.core.open_connection(conn, nonce, now);
                let win = spawn_win_conn_io(conn, stream, self.win_ev_tx.clone(), Arc::clone(&self.win_wake));
                self.conns.insert(
                    conn,
                    ConnState { win, stalled_frames: HashMap::new(), queued_frame_streams: HashMap::new() },
                );
            }
            WinTransportEvent::Bytes(conn, bytes) => {
                self.sync_shard_telemetry();
                let now = TimestampMs::from_millis(now_unix_ms());
                let mut actions = Vec::new();
                self.core.feed_bytes(conn, &bytes, now, &mut actions);
                if self.core.is_connection_ready(conn) {
                    self.ever_connected = true;
                }
                self.execute_actions(actions, now);
            }
            WinTransportEvent::WriteReady(conn) => {
                self.flush_win_writes(conn);
            }
            WinTransportEvent::Closed(conn) => {
                self.on_transport_closed(conn);
            }
        }
    }

    pub(crate) fn flush_win_writes(&mut self, conn: ConnId) {
        while self.push_next_win_frame(conn) {}
    }

    fn push_next_win_frame(&mut self, conn: ConnId) -> bool {
        let next = if let Some(pending) = self.conns.get_mut(&conn).and_then(|c| c.win.pending_chunk.take()) {
            Some(pending)
        } else if let Some(queued) = self.core.pop_outbound(conn) {
            let is_frame = matches!(queued.msg, BrokerMsg::Frame(_));
            if is_frame
                && let Some(nz) = NonZeroStreamId::new(queued.stream_id.raw())
                && let Some(session_id) = self.conns.get_mut(&conn).and_then(|c| c.queued_frame_streams.remove(&nz))
            {
                self.on_outbound_frame_drained(conn, nz, session_id);
            }
            let mut encoded = Vec::with_capacity(queued.encoded_bytes);
            if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_err() {
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
                false
            }
        };
        if sent {
            let ready_stalled = self
                .conns
                .get(&conn)
                .map(|c| ready_stalled_stream_ids(&c.stalled_frames, &c.queued_frame_streams))
                .unwrap_or_default();
            for stream_id in ready_stalled {
                if let Some((sid, frame)) = self.conns.get_mut(&conn).and_then(|c| c.stalled_frames.remove(&stream_id))
                {
                    self.try_publish_or_stall_frame(
                        conn,
                        stream_id,
                        sid,
                        frame,
                        TimestampMs::from_millis(now_unix_ms()),
                    );
                }
            }
        }
        sent
    }
}
