//! Dumb zero-sniff single-channel stdio-to-broker `proxy`.
//!
//! Replaces v1's 3-channel protocol-sniffing proxy with a single pure byte splicer:
//! - Zero framing inspection or message parsing.
//! - Retains up to [`PROXY_REPLAY_CAP`] (`1 MiB`) of client bytes written before the first byte
//!   arrives from the broker; if an idle broker exited right as the proxy connected, reconnects
//!   once (auto-starting a detached broker if requested) and replays all buffered bytes on a
//!   dedicated writer thread so the reader thread never deadlocks on full-duplex socket buffers.
//! - Writes the universal 4-byte [`apack_proto::FIN_FRAME`] (`[0, 0, 0, 0]`) on `stdin` EOF across
//!   every OS (Unix and Windows) without aborting the broker-to-stdout read half.

use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use apack_os::transport::{
    HELD_EXIT, configure_detached_command, connect_local_stream, let_go, verify_peer_is_current_user,
};
use apack_proto::FIN_FRAME;
use interprocess::local_socket::Stream;

/// Maximum pre-first-response client bytes buffered for idle-broker exit race replay (`1 MiB`).
pub const PROXY_REPLAY_CAP: usize = 1_048_576;

const PROXY_BUF_BYTES: usize = 32_768;

/// Maximum stderr bytes captured from an auto-started broker to surface startup failures (`4 KiB`).
const AUTOSTART_STDERR_CAP: usize = 4_096;

/// Maximum time spent waiting for an auto-started local broker to begin listening (`5 s`).
const AUTOSTART_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum autostart spawn attempts within [`AUTOSTART_TIMEOUT`] on `HELD_EXIT` or clean exit `0`.
const MAX_AUTOSTART_SPAWNS: usize = 5;

/// Number of quick `connect_verified` polls after `HELD_EXIT` before re-spawning (in case the
/// concurrent winner exited cleanly before we connected).
const HELD_CONNECT_POLLS: usize = 4;

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

    /// Returns `true` if the buffer exceeded its cap and can no longer safely replay.
    pub fn is_overflowed(&self) -> bool {
        self.overflowed
    }

    /// Returns the buffered replay bytes if still within cap and non-empty.
    pub fn replay_slice(&self) -> Option<&[u8]> {
        if self.overflowed || self.bytes.is_empty() { None } else { Some(&self.bytes) }
    }

    /// Takes ownership of all buffered replay bytes without copying, releasing buffer storage.
    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }

    /// Releases buffered memory once the broker sends its first byte.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.bytes.shrink_to_fit();
    }
}

/// Connects to `socket_path`, verifying current-user ownership; if `autostart` is true and no
/// broker is listening, spawns a detached `apack-terminal serve --detached --socket <socket_path>` and retries.
pub fn connect_or_autostart(socket_path: &str, autostart: bool) -> io::Result<Stream> {
    connect_or_autostart_with(socket_path, autostart, AUTOSTART_TIMEOUT, |path| spawn_detached_broker(path).map(Some))
}

struct StderrCollector {
    buf: Arc<Mutex<Vec<u8>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StderrCollector {
    fn spawn(mut stderr: std::process::ChildStderr) -> Option<Self> {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buf);
        let handle = thread::Builder::new()
            .name(String::from("apack-proxy-stderr"))
            .spawn(move || {
                let mut chunk = [0u8; 512];
                while let Ok(n) = stderr.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    let mut captured = writer.lock().unwrap_or_else(|e| e.into_inner());
                    let room = AUTOSTART_STDERR_CAP.saturating_sub(captured.len());
                    if room != 0 {
                        captured.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            })
            .ok()?;
        Some(Self { buf, handle: Some(handle) })
    }

    fn snapshot(&self) -> String {
        let captured = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&captured).trim().to_owned()
    }

    fn join_and_take(mut self) -> String {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.snapshot()
    }
}

pub(crate) fn reap_child_in_background(mut child: Child) {
    let _ = thread::Builder::new().name(String::from("apack-proxy-reaper")).spawn(move || {
        let _ = child.wait();
    });
}

/// Testable core of [`connect_or_autostart`] accepting a custom timeout and spawner closure.
pub(crate) fn connect_or_autostart_with<F>(
    socket_path: &str,
    autostart: bool,
    timeout: Duration,
    mut spawn_broker: F,
) -> io::Result<Stream>
where
    F: FnMut(&str) -> io::Result<Option<Child>>,
{
    match connect_verified(socket_path) {
        Ok(stream) => return Ok(stream),
        Err(err) if !autostart || err.kind() == io::ErrorKind::PermissionDenied => return Err(err),
        Err(_) => {}
    }

    let mut spawns = 1usize;
    let mut child = spawn_broker(socket_path)?;
    let mut stderr_collector = child.as_mut().and_then(|c| c.stderr.take()).and_then(StderrCollector::spawn);
    let mut needs_respawn = false;
    let mut held_polls_left = 0usize;
    let started = Instant::now();
    let backoff = Duration::from_millis(15);
    loop {
        match connect_verified(socket_path) {
            Ok(stream) => {
                if let Some(c) = child.take() {
                    reap_child_in_background(c);
                }
                return Ok(stream);
            }
            Err(err) => {
                if err.kind() == io::ErrorKind::PermissionDenied {
                    if let Some(c) = child.take() {
                        reap_child_in_background(c);
                    }
                    return Err(err);
                }
                if started.elapsed() >= timeout {
                    let exited = child.as_mut().is_some_and(|c| c.try_wait().ok().flatten().is_some());
                    if let Some(c) = child.take() {
                        reap_child_in_background(c);
                    }
                    let detail = stderr_collector
                        .take()
                        .map(|sc| if exited { sc.join_and_take() } else { sc.snapshot() })
                        .filter(|s| !s.is_empty());
                    return Err(match detail {
                        Some(msg) => io::Error::new(err.kind(), format!("{err}: {msg}")),
                        None => err,
                    });
                }
                if let Some(c) = child.as_mut()
                    && let Ok(Some(status)) = c.try_wait()
                {
                    child = None;
                    if status.code() == Some(HELD_EXIT) {
                        // Another concurrent proxy won the bind race; try connecting to the winner
                        // briefly, and re-spawn if the winner exited before we connected.
                        needs_respawn = true;
                        held_polls_left = HELD_CONNECT_POLLS;
                        continue;
                    }
                    if status.success() {
                        // Spawned broker exited cleanly (e.g., idle race) before we connected.
                        needs_respawn = true;
                        held_polls_left = 0;
                        continue;
                    }
                    let detail = stderr_collector
                        .take()
                        .map(StderrCollector::join_and_take)
                        .filter(|s| !s.is_empty())
                        .map_or_else(String::new, |s| format!(": {s}"));
                    return Err(io::Error::other(format!("autostarted broker exited with {status}{detail}")));
                }
                if needs_respawn {
                    if held_polls_left > 0 {
                        held_polls_left -= 1;
                    } else if spawns < MAX_AUTOSTART_SPAWNS {
                        spawns += 1;
                        needs_respawn = false;
                        child = spawn_broker(socket_path)?;
                        stderr_collector =
                            child.as_mut().and_then(|c| c.stderr.take()).and_then(StderrCollector::spawn);
                        continue;
                    }
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

fn spawn_detached_broker(socket_path: &str) -> io::Result<Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve")
        .arg("--detached")
        .arg("--socket")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_detached_command(&mut cmd);
    cmd.spawn()
}

/// Runs the dumb single-channel stdio proxy connected to `socket_path`.
pub fn run_stdio_proxy(socket_path: &str, autostart: bool) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_proxy_over(stdin, stdout, || connect_or_autostart(socket_path, autostart))
}

/// Returns whether the proxy writer thread should still buffer client bytes for a pre-response
/// reconnect replay (and tolerate broken-pipe on the initial broker link).
pub(crate) fn should_buffer_replay(saw_broker_byte: bool, reconnected: bool) -> bool {
    !saw_broker_byte && !reconnected
}

/// Returns whether the proxy should attempt its single pre-first-response reconnect after `stream1`
/// hits EOF.
pub(crate) fn should_reconnect_proxy(
    first_read_eof: bool,
    overflowed: bool,
    stdin_eof: bool,
    has_replay: bool,
) -> bool {
    first_read_eof && !overflowed && (!stdin_eof || has_replay)
}

/// Returns whether the replay writer thread should follow replayed client bytes with `FIN_FRAME`.
pub(crate) fn should_send_replay_fin(write_ok: bool, already_eof: bool, cancelled: bool) -> bool {
    write_ok && already_eof && !cancelled
}

/// Marks the first broker byte observed and clears the replay buffer once without locking on later chunks.
pub(crate) fn mark_broker_byte_and_clear_replay(saw_broker_byte: &AtomicBool, replay: &Mutex<ActiveLink>) -> bool {
    if !saw_broker_byte.swap(true, Ordering::AcqRel) {
        replay.lock().unwrap_or_else(|e| e.into_inner()).replay.clear();
        true
    } else {
        false
    }
}

pub(crate) struct ActiveLink {
    pub(crate) stream: Arc<Stream>,
    pub(crate) replay: ReplayBuffer,
    pub(crate) stdin_eof: bool,
    pub(crate) reconnected: bool,
    pub(crate) replaying: bool,
}

/// Runs the zero-sniff single-channel byte proxy over arbitrary `stdin`/`stdout` streams, using
/// `connect` to establish the broker link (and retrying once with non-blocking off-reader replay
/// if the broker exits before emitting its first byte).
pub fn run_proxy_over<R, W, F>(mut input: R, mut output: W, mut connect: F) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send,
    F: FnMut() -> io::Result<Stream>,
{
    let stream1 = Arc::new(connect()?);
    let link = Arc::new((
        Mutex::new(ActiveLink {
            stream: Arc::clone(&stream1),
            replay: ReplayBuffer::new(PROXY_REPLAY_CAP),
            stdin_eof: false,
            reconnected: false,
            replaying: false,
        }),
        Condvar::new(),
    ));
    let saw_broker_byte = Arc::new(AtomicBool::new(false));

    let writer_link = Arc::clone(&link);
    let writer_saw = Arc::clone(&saw_broker_byte);

    let to_broker = thread::spawn(move || -> io::Result<()> {
        let mut buf = [0u8; PROXY_BUF_BYTES];
        loop {
            match crate::read_retry_eintr(&mut input, &mut buf) {
                Ok(0) | Err(_) => {
                    let current = {
                        let (lock, cond) = &*writer_link;
                        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while guard.replaying {
                            guard = cond.wait(guard).unwrap_or_else(|e| e.into_inner());
                        }
                        guard.stdin_eof = true;
                        Arc::clone(&guard.stream)
                    };
                    let mut tx: &Stream = &current;
                    let _ = tx.write_all(&FIN_FRAME);
                    let _ = tx.flush();
                    return Ok(());
                }
                Ok(n) => {
                    let current = {
                        let (lock, cond) = &*writer_link;
                        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while guard.replaying {
                            guard = cond.wait(guard).unwrap_or_else(|e| e.into_inner());
                        }
                        if should_buffer_replay(writer_saw.load(Ordering::Acquire), guard.reconnected) {
                            guard.replay.push(&buf[..n]);
                        }
                        Arc::clone(&guard.stream)
                    };
                    let mut tx: &Stream = &current;
                    let _ = tx.write_all(&buf[..n]);
                }
            }
        }
    });

    // Forward broker -> stdout on the initial connection.
    let mut out_buf = [0u8; PROXY_BUF_BYTES];
    let mut broker_rx: &Stream = &stream1;
    let first_read_eof = loop {
        match crate::read_retry_eintr(&mut broker_rx, &mut out_buf) {
            Ok(0) | Err(_) => break !saw_broker_byte.load(Ordering::Acquire),
            Ok(n) => {
                mark_broker_byte_and_clear_replay(&saw_broker_byte, &link.0);
                output.write_all(&out_buf[..n])?;
                output.flush()?;
            }
        }
    };

    // If the broker closed before sending a single byte AND replay hasn't overflowed, reconnect
    // FIRST, take the replay snapshot under `link` (zero I/O under lock), hand the write to a
    // dedicated replay writer thread, and immediately resume reading `stream2` so full-duplex
    // responses can never deadlock behind a large (up to 1 MiB) replay write.
    let (should_reconnect, overflowed_before_eof) = {
        let guard = link.0.lock().unwrap_or_else(|e| e.into_inner());
        let overflowed = first_read_eof && guard.replay.is_overflowed();
        let reconnect = should_reconnect_proxy(
            first_read_eof,
            guard.replay.is_overflowed(),
            guard.stdin_eof,
            guard.replay.replay_slice().is_some(),
        );
        (reconnect, overflowed)
    };
    if overflowed_before_eof {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "proxy replay buffer overflowed before broker responded",
        ));
    }

    if should_reconnect {
        let stream2 = Arc::new(connect()?);
        let (replay_bytes, already_eof) = {
            let mut guard = link.0.lock().unwrap_or_else(|e| e.into_inner());
            if guard.replay.is_overflowed() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "proxy replay buffer overflowed while reconnecting",
                ));
            }
            let bytes = guard.replay.take_bytes();
            let eof = guard.stdin_eof;
            guard.replaying = true;
            (bytes, eof)
        };

        let replay_cancel = Arc::new(AtomicBool::new(false));
        let replay_stream = Arc::clone(&stream2);
        let replay_link = Arc::clone(&link);
        let replay_cancel_w = Arc::clone(&replay_cancel);
        let replay_writer = match thread::Builder::new().name(String::from("apack-proxy-replay")).spawn(move || {
            if !replay_cancel_w.load(Ordering::Acquire) {
                let mut tx2: &Stream = &replay_stream;
                let write_ok = tx2.write_all(&replay_bytes).and_then(|()| tx2.flush()).is_ok();
                if should_send_replay_fin(write_ok, already_eof, replay_cancel_w.load(Ordering::Acquire)) {
                    let _ = tx2.write_all(&FIN_FRAME);
                    let _ = tx2.flush();
                }
            }
            let (lock, cond) = &*replay_link;
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.stream = replay_stream;
            guard.reconnected = true;
            guard.replaying = false;
            cond.notify_all();
        }) {
            Ok(h) => h,
            Err(err) => {
                let (lock, cond) = &*link;
                let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                guard.replaying = false;
                cond.notify_all();
                return Err(err);
            }
        };

        let mut rx2: &Stream = &stream2;
        let read_res = loop {
            match crate::read_retry_eintr(&mut rx2, &mut out_buf) {
                Ok(0) | Err(_) => break Ok(()),
                Ok(n) => {
                    saw_broker_byte.store(true, Ordering::Release);
                    if let Err(err) = output.write_all(&out_buf[..n]).and_then(|()| output.flush()) {
                        break Err(err);
                    }
                }
            }
        };
        replay_cancel.store(true, Ordering::Release);
        let _ = let_go(&stream2, || {
            let _ = replay_writer.join();
        });
        read_res?;
    }

    let _ = output.flush();
    // Only join `to_broker` if stdin already hit EOF; if the broker closed first (e.g., auth error
    // over SSH), return immediately so stdout closes without deadlocking in blocking stdin.read().
    if link.0.lock().unwrap_or_else(|e| e.into_inner()).stdin_eof {
        let _ = to_broker.join();
    }
    Ok(())
}
