//! Dumb zero-sniff single-channel stdio-to-broker `proxy`.
//!
//! Replaces v1's 3-channel protocol-sniffing proxy with a single pure byte splicer:
//! - Zero framing inspection or message parsing.
//! - Retains up to [`PROXY_REPLAY_CAP`] (`1 MiB`) of client bytes written before the first byte
//!   arrives from the broker; if an idle broker exited right as the proxy connected, reconnects
//!   once (auto-starting a detached broker if requested) and atomically replays all buffered bytes.
//! - Writes the universal 4-byte [`apack_proto::FIN_FRAME`] (`[0, 0, 0, 0]`) on `stdin` EOF across
//!   every OS (Unix and Windows) without aborting the broker-to-stdout read half.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use apack_os::transport::{connect_local_stream, verify_peer_is_current_user};
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

    /// Returns `true` if the buffer exceeded its cap and can no longer safely replay.
    pub fn is_overflowed(&self) -> bool {
        self.overflowed
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
/// broker is listening, spawns a detached `apack-terminal serve --socket <socket_path>` and retries.
pub fn connect_or_autostart(socket_path: &str, autostart: bool) -> io::Result<Stream> {
    connect_or_autostart_with(socket_path, autostart, AUTOSTART_TIMEOUT, |path| {
        let _child = spawn_detached_broker(path)?;
        Ok(())
    })
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

fn spawn_detached_broker(socket_path: &str) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve").arg("--socket").arg(socket_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Detach into its own process group so SSH/proxy SIGHUP does not kill the broker.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS (0x0000_0008) | CREATE_NEW_PROCESS_GROUP (0x0000_0200) | CREATE_NO_WINDOW (0x0800_0000)
        const WINDOWS_DETACHED_FLAGS: u32 = 0x0800_0208;
        cmd.creation_flags(WINDOWS_DETACHED_FLAGS);
    }
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

struct ActiveLink {
    stream: Arc<Stream>,
    replay: ReplayBuffer,
    stdin_eof: bool,
    reconnected: bool,
}

/// Runs the zero-sniff single-channel byte proxy over arbitrary `stdin`/`stdout` streams, using
/// `connect` to establish the broker link (and retrying once with atomic replay if the broker exits
/// before emitting its first byte).
pub fn run_proxy_over<R, W, F>(mut input: R, mut output: W, mut connect: F) -> io::Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send,
    F: FnMut() -> io::Result<Stream>,
{
    let stream1 = Arc::new(connect()?);
    let link = Arc::new(Mutex::new(ActiveLink {
        stream: Arc::clone(&stream1),
        replay: ReplayBuffer::new(PROXY_REPLAY_CAP),
        stdin_eof: false,
        reconnected: false,
    }));
    let saw_broker_byte = Arc::new(AtomicBool::new(false));

    let writer_link = Arc::clone(&link);
    let writer_saw = Arc::clone(&saw_broker_byte);

    let to_broker = thread::spawn(move || -> io::Result<()> {
        let mut buf = [0u8; PROXY_BUF_BYTES];
        loop {
            match crate::read_retry_eintr(&mut input, &mut buf) {
                Ok(0) => {
                    let current = {
                        let mut guard = writer_link.lock().unwrap_or_else(|e| e.into_inner());
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
                        let mut guard = writer_link.lock().unwrap_or_else(|e| e.into_inner());
                        if should_buffer_replay(writer_saw.load(Ordering::Acquire), guard.reconnected) {
                            guard.replay.push(&buf[..n]);
                        }
                        Arc::clone(&guard.stream)
                    };
                    let mut tx: &Stream = &current;
                    let _ = tx.write_all(&buf[..n]);
                }
                Err(err) => return Err(err),
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
                saw_broker_byte.store(true, Ordering::Release);
                link.lock().unwrap_or_else(|e| e.into_inner()).replay.clear();
                output.write_all(&out_buf[..n])?;
                output.flush()?;
            }
        }
    };

    // If the broker closed before sending a single byte AND replay hasn't overflowed, reconnect
    // FIRST, then lock `link` atomically to replay ALL bytes buffered so far (including any that
    // arrived while `connect()` was running) and swap `stream` seamlessly!
    let should_reconnect = {
        let guard = link.lock().unwrap_or_else(|e| e.into_inner());
        should_reconnect_proxy(
            first_read_eof,
            guard.replay.is_overflowed(),
            guard.stdin_eof,
            guard.replay.replay_slice().is_some(),
        )
    };

    if should_reconnect {
        let stream2 = Arc::new(connect()?);
        let already_eof = {
            let mut guard = link.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(bytes) = guard.replay.replay_slice() {
                let mut tx2: &Stream = &stream2;
                tx2.write_all(bytes)?;
                tx2.flush()?;
            }
            guard.replay.clear();
            guard.stream = Arc::clone(&stream2);
            guard.reconnected = true;
            guard.stdin_eof
        };
        if already_eof {
            let mut tx2: &Stream = &stream2;
            let _ = tx2.write_all(&FIN_FRAME);
            let _ = tx2.flush();
        }
        let mut rx2: &Stream = &stream2;
        loop {
            match crate::read_retry_eintr(&mut rx2, &mut out_buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    saw_broker_byte.store(true, Ordering::Release);
                    output.write_all(&out_buf[..n])?;
                    output.flush()?;
                }
            }
        }
    }

    let _ = output.flush();
    // Only join `to_broker` if stdin already hit EOF; if the broker closed first (e.g., auth error
    // over SSH), return immediately so stdout closes without deadlocking in blocking stdin.read().
    if link.lock().unwrap_or_else(|e| e.into_inner()).stdin_eof {
        let _ = to_broker.join();
    }
    Ok(())
}
