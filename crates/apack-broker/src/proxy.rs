//! Dumb zero-sniff single-channel stdio-to-broker `proxy`.
//!
//! Replaces v1's 3-channel protocol-sniffing proxy with a single pure byte splicer:
//! - Zero framing inspection or message parsing.
//! - Retains up to [`PROXY_REPLAY_CAP`] (`1 MiB`) of client bytes written before the first byte
//!   arrives from the broker; if an idle broker exited right as the proxy connected, reconnects
//!   once (auto-starting a detached broker if requested) and replays the buffered bytes.
//! - Writes the universal 4-byte [`apack_proto::FIN_FRAME`] (`[0, 0, 0, 0]`) on `stdin` EOF across
//!   every OS (Unix and Windows) before draining remaining broker output to `stdout`.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use apack_os::transport::{connect_local_stream, let_go, verify_peer_is_current_user};
use apack_proto::FIN_FRAME;
use interprocess::local_socket::Stream;

/// Maximum pre-first-response client bytes buffered for idle-broker exit race replay (`1 MiB`).
pub const PROXY_REPLAY_CAP: usize = 1_048_576;

const PROXY_BUF_BYTES: usize = 32_768;

/// Maximum time spent waiting for an auto-started local broker to begin listening (`5 s`).
const AUTOSTART_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded buffer capturing client bytes written before the broker sends its first response byte.
#[derive(Debug, Default)]
pub struct ReplayBuffer {
    bytes: Vec<u8>,
    overflowed: bool,
    cap: usize,
}

impl ReplayBuffer {
    /// Creates an empty replay buffer capped at `cap` bytes.
    pub fn new(cap: usize) -> Self {
        Self { bytes: Vec::new(), overflowed: false, cap }
    }

    /// Records `chunk` while below `cap`; once exceeded, marks `overflowed` and clears buffered memory.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.bytes.len().saturating_add(chunk.len()) > self.cap {
            self.overflowed = true;
            self.bytes.clear();
            self.bytes.shrink_to_fit();
            return;
        }
        self.bytes.extend_from_slice(chunk);
    }

    /// Returns the buffered replay bytes if still within cap and non-empty.
    pub fn replay_slice(&self) -> Option<&[u8]> {
        if self.overflowed || self.bytes.is_empty() { None } else { Some(&self.bytes) }
    }

    /// Releases buffered memory once the broker sends its first byte.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.bytes.shrink_to_fit();
    }
}

/// Connects to `socket_path`, verifying current-user ownership; if `autostart` is true and no
/// broker is listening, spawns a detached `apack-broker serve --socket <socket_path>` and retries.
pub fn connect_or_autostart(socket_path: &str, autostart: bool) -> io::Result<Stream> {
    connect_or_autostart_with(socket_path, autostart, AUTOSTART_TIMEOUT, spawn_detached_broker)
}

/// Testable core of [`connect_or_autostart`] accepting a custom timeout and spawner closure.
pub(crate) fn connect_or_autostart_with<F>(
    socket_path: &str,
    autostart: bool,
    timeout: Duration,
    spawn_broker: F,
) -> io::Result<Stream>
where
    F: FnOnce(&str) -> io::Result<()>,
{
    match connect_verified(socket_path) {
        Ok(stream) => return Ok(stream),
        Err(err) if !autostart => return Err(err),
        Err(_) => {}
    }

    spawn_broker(socket_path)?;
    let started = Instant::now();
    let backoff = Duration::from_millis(15);
    loop {
        match connect_verified(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if started.elapsed() >= timeout {
                    return Err(err);
                }
                thread::sleep(backoff);
            }
        }
    }
}

fn connect_verified(socket_path: &str) -> io::Result<Stream> {
    let stream = connect_local_stream(socket_path)?;
    if !verify_peer_is_current_user(&stream)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker endpoint is not owned by the current user",
        ));
    }
    Ok(stream)
}

fn spawn_detached_broker(socket_path: &str) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve").arg("--socket").arg(socket_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _child = cmd.spawn()?;
    Ok(())
}

/// Runs the dumb single-channel stdio proxy connected to `socket_path`.
pub fn run_stdio_proxy(socket_path: &str, autostart: bool) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_proxy_over(stdin, stdout, || connect_or_autostart(socket_path, autostart))
}

/// Runs the zero-sniff single-channel byte proxy over arbitrary `stdin`/`stdout` streams, using
/// `connect` to establish the broker link (and retrying once with replay if the broker exits
/// before emitting its first byte).
pub fn run_proxy_over<R, W, F>(mut input: R, mut output: W, mut connect: F) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send,
    F: FnMut() -> io::Result<Stream>,
{
    let stream1 = Arc::new(connect()?);
    let active_tx = Arc::new(Mutex::new(Arc::clone(&stream1)));
    let replay = Arc::new(Mutex::new(ReplayBuffer::new(PROXY_REPLAY_CAP)));
    let saw_broker_byte = Arc::new(AtomicBool::new(false));
    let stdin_eof = Arc::new(AtomicBool::new(false));

    let writer_tx = Arc::clone(&active_tx);
    let writer_replay = Arc::clone(&replay);
    let writer_saw = Arc::clone(&saw_broker_byte);
    let writer_eof = Arc::clone(&stdin_eof);

    let to_broker = thread::spawn(move || -> io::Result<()> {
        let mut buf = [0u8; PROXY_BUF_BYTES];
        loop {
            match input.read(&mut buf) {
                Ok(0) => {
                    let current = {
                        let guard = writer_tx.lock().unwrap_or_else(|e| e.into_inner());
                        writer_eof.store(true, Ordering::Release);
                        Arc::clone(&guard)
                    };
                    let mut tx: &Stream = &current;
                    let _ = tx.write_all(&FIN_FRAME);
                    let _ = tx.flush();
                    let _ = let_go(&current, || {});
                    return Ok(());
                }
                Ok(n) => {
                    let current = {
                        let guard = writer_tx.lock().unwrap_or_else(|e| e.into_inner());
                        if !writer_saw.load(Ordering::Acquire) {
                            writer_replay.lock().unwrap_or_else(|e| e.into_inner()).push(&buf[..n]);
                        }
                        Arc::clone(&guard)
                    };
                    let mut tx: &Stream = &current;
                    if let Err(err) = tx.write_all(&buf[..n]) {
                        // If the first broker closed before responding, the main thread may swap
                        // `active_tx` to a fresh broker connection and replay buffered bytes.
                        if writer_saw.load(Ordering::Acquire) {
                            return Err(err);
                        }
                    }
                }
                Err(err) => return Err(err),
            }
        }
    });

    // Forward broker -> stdout on the initial connection.
    let mut out_buf = [0u8; PROXY_BUF_BYTES];
    let mut broker_rx: &Stream = &stream1;
    let first_read_eof = loop {
        match broker_rx.read(&mut out_buf) {
            Ok(0) | Err(_) => break !saw_broker_byte.load(Ordering::Acquire),
            Ok(n) => {
                saw_broker_byte.store(true, Ordering::Release);
                replay.lock().unwrap_or_else(|e| e.into_inner()).clear();
                output.write_all(&out_buf[..n])?;
                output.flush()?;
            }
        }
    };

    // If the broker closed before sending a single byte AND we buffered pre-response client bytes,
    // reconnect once WITHOUT blocking on `to_broker`, swap `active_tx`, replay, and keep forwarding!
    let replay_bytes = if first_read_eof {
        replay.lock().unwrap_or_else(|e| e.into_inner()).replay_slice().map(ToOwned::to_owned)
    } else {
        None
    };

    if let Some(bytes) = replay_bytes {
        let stream2 = Arc::new(connect()?);
        let already_eof = {
            let mut guard = active_tx.lock().unwrap_or_else(|e| e.into_inner());
            let mut tx2: &Stream = &stream2;
            tx2.write_all(&bytes)?;
            tx2.flush()?;
            *guard = Arc::clone(&stream2);
            stdin_eof.load(Ordering::Acquire)
        };
        if already_eof {
            let mut tx2: &Stream = &stream2;
            let _ = tx2.write_all(&FIN_FRAME);
            let _ = tx2.flush();
            let _ = let_go(&stream2, || {});
        }
        let mut rx2: &Stream = &stream2;
        loop {
            match rx2.read(&mut out_buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    saw_broker_byte.store(true, Ordering::Release);
                    replay.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    output.write_all(&out_buf[..n])?;
                    output.flush()?;
                }
            }
        }
    }

    let _ = to_broker.join();
    Ok(())
}
