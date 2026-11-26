//! Windows named-pipe transport edge (`#[cfg(windows)]`-only; mutated on Windows CI).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use apack_core::lease::ConnId;
use apack_core::time::TimestampMs;
use apack_os::transport::{hang_up, verify_peer_is_current_user};
use apack_proto::types::NonZeroStreamId;
use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::{Listener, Stream};

use crate::transport_loop::{ConnState, ReactorState};
use crate::{fill_random, now_unix_ms};

const WIN_CONN_READ_BUF_BYTES: usize = 32_768;

/// Message sent to a Windows per-connection named-pipe writer thread.
pub(crate) enum WinWriteMsg {
    /// Complete wire-encoded frame bytes.
    Bytes(Vec<u8>),
    /// Flush and terminate the writer thread cleanly.
    Close,
}

/// Transport event bridged from Windows named-pipe threads into the single-threaded reactor.
pub(crate) enum WinTransportEvent {
    /// Newly accepted and user-verified client connection.
    Accepted(Stream),
    /// Raw bytes read from `ConnId`.
    Bytes(ConnId, Vec<u8>),
    /// Connection reached EOF or read error.
    Closed(ConnId),
}

/// Per-connection Windows pipe write/close handles held by the Control Reactor.
pub(crate) struct WinConnHandle {
    pub(crate) tx: mpsc::SyncSender<WinWriteMsg>,
    pub(crate) hangup: Arc<dyn Fn() + Send + Sync>,
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
    let (wtx, wrx) = mpsc::sync_channel::<WinWriteMsg>(256);

    let _ = std::thread::Builder::new().name(format!("apack-win-r-{}", conn.get())).spawn(move || {
        let mut buf = vec![0u8; WIN_CONN_READ_BUF_BYTES];
        let mut reader: &Stream = &r_stream;
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = ev_tx.send(WinTransportEvent::Closed(conn));
                    wake();
                    break;
                }
                Ok(n) => {
                    if ev_tx.send(WinTransportEvent::Bytes(conn, buf[..n].to_vec())).is_err() {
                        break;
                    }
                    wake();
                }
            }
        }
    });

    let _ = std::thread::Builder::new().name(format!("apack-win-w-{}", conn.get())).spawn(move || {
        let mut writer: &Stream = &w_stream;
        while let Ok(msg) = wrx.recv() {
            match msg {
                WinWriteMsg::Bytes(bytes) => {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
                WinWriteMsg::Close => {
                    let _ = writer.flush();
                    break;
                }
            }
        }
    });

    let close_tx = wtx.clone();
    let hangup_stream = Arc::clone(&stream);
    WinConnHandle {
        tx: wtx,
        hangup: Arc::new(move || {
            let _ = close_tx.try_send(WinWriteMsg::Close);
            let _ = hang_up(&hangup_stream);
        }),
    }
}

impl ReactorState {
    pub(crate) fn handle_win_transport_event(&mut self, ev: WinTransportEvent) {
        match ev {
            WinTransportEvent::Accepted(stream) => {
                let conn = self.alloc_conn_id();
                let mut nonce = [0u8; 32];
                let _ = fill_random(&mut nonce);
                let now = TimestampMs::from_millis(now_unix_ms());
                let _ = self.core.open_connection(conn, nonce, now);
                let win = spawn_win_conn_io(conn, stream, self.win_ev_tx.clone(), Arc::clone(&self.win_wake));
                self.conns.insert(
                    conn,
                    ConnState { win, stalled_frames: HashMap::new(), queued_frame_streams: HashMap::new() },
                );
            }
            WinTransportEvent::Bytes(conn, bytes) => {
                let now = TimestampMs::from_millis(now_unix_ms());
                let mut actions = Vec::new();
                self.core.feed_bytes(conn, &bytes, now, &mut actions);
                if self.core.is_connection_ready(conn) {
                    self.ever_connected = true;
                }
                self.execute_actions(actions, now);
            }
            WinTransportEvent::Closed(conn) => {
                self.on_transport_closed(conn);
            }
        }
    }

    pub(crate) fn flush_win_writes(&mut self, conn: ConnId) {
        while let Some(queued) = self.core.pop_outbound(conn) {
            let mut encoded = Vec::with_capacity(queued.encoded_bytes);
            if queued.msg.encode_to_frame(queued.stream_id, queued.req_id, &mut encoded).is_err() {
                continue;
            }
            if let Some(nz) = NonZeroStreamId::new(queued.stream_id.raw())
                && let Some(c) = self.conns.get_mut(&conn)
                && let Some(session_id) = c.queued_frame_streams.remove(&nz)
            {
                self.on_outbound_frame_drained(conn, nz, session_id);
            }
            let send_err = match self.conns.get(&conn) {
                Some(c) => c.win.tx.send(WinWriteMsg::Bytes(encoded)).is_err(),
                None => return,
            };
            if send_err {
                self.on_transport_closed(conn);
                return;
            }
        }
    }
}
