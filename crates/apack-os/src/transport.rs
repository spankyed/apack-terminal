//! User-private local socket / Windows named-pipe transport, exclusive broker locking, peer
//! verification, non-blocking configuration, and scatter-gather `writev` framing helpers.

use std::io::{self, IoSlice, Write};
#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use interprocess::local_socket::{
    GenericFilePath, Listener, ListenerNonblockingMode, ListenerOptions, Stream, ToFsName,
};

use crate::OsError;

/// Exit code used when another broker already holds the target socket/pipe.
pub const HELD_EXIT: i32 = 3;

/// Default grace period a freshly started broker waits for its first connection before exiting.
pub const START_GRACE: Duration = Duration::from_secs(10);

/// Optional environment variable overriding [`START_GRACE`] in milliseconds (for tests).
pub const START_GRACE_ENV: &str = "APACK_TERMINAL_START_GRACE_MS";

/// Parses an optional millisecond grace override, falling back to [`START_GRACE`].
pub fn start_grace(asked: Option<&str>) -> Duration {
    asked.and_then(|s| s.parse::<u64>().ok()).map_or(START_GRACE, Duration::from_millis)
}

/// Computes the default per-user broker endpoint:
/// - Unix: `$XDG_RUNTIME_DIR/apack-terminal/broker.sock`, else `$HOME/.apack-terminal/broker.sock`.
/// - Windows: `\\.\pipe\apack-terminal-<USER_SID>`.
pub fn default_socket_path() -> Option<String> {
    #[cfg(windows)]
    {
        crate::transport_windows::default_pipe_name()
    }
    #[cfg(unix)]
    {
        let non_empty = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let dir = match non_empty("XDG_RUNTIME_DIR") {
            Some(runtime) => Path::new(&runtime).join("apack-terminal"),
            None => Path::new(&non_empty("HOME")?).join(".apack-terminal"),
        };
        Some(dir.join("broker.sock").to_string_lossy().into_owned())
    }
}

/// Exclusive lock holding ownership of a broker socket for the lifetime of the process.
///
/// On Unix this holds a `flock(LOCK_EX | LOCK_NB)` on `{socket}.lock` and explicitly calls
/// `LOCK_UN` on [`Drop`] so forked children mid-`exec` cannot keep the lock alive after broker
/// exit. On Windows named-pipe first-instance creation enforces uniqueness natively.
pub struct SocketHold {
    #[cfg(unix)]
    lock: std::fs::File,
}

#[cfg(unix)]
impl Drop for SocketHold {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: `self.lock` is an open file descriptor owned exclusively by `SocketHold`.
        unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Acquires the exclusive broker hold on `socket`, returning [`OsError::AlreadyHeld`] if another
/// broker already owns it.
pub fn acquire_socket_hold(socket: &str) -> Result<SocketHold, OsError> {
    #[cfg(not(unix))]
    {
        let _ = socket;
        Ok(SocketHold {})
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let lock_path = std::path::PathBuf::from(format!("{socket}.lock"));
        let lock = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|err| OsError::io("opening socket lock file", err))?;

        // SAFETY: `lock` is an open file descriptor owned by this function.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(SocketHold { lock });
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Err(OsError::AlreadyHeld)
        } else {
            Err(OsError::io("locking socket lock file", err))
        }
    }
}

#[cfg(unix)]
fn socket_parent_dir(socket: &str) -> &Path {
    match Path::new(socket).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Ensures the parent directory of `socket` exists with mode `0o700` (if missing) and verifies it
/// is a real directory (not a symlink) owned by the current effective UID with no group/other write
/// bits (`mode & 0o022 == 0`).
pub fn ensure_private_directory(socket: &str) -> Result<(), OsError> {
    #[cfg(not(unix))]
    {
        let _ = socket;
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};

        let dir = socket_parent_dir(socket);
        if std::fs::symlink_metadata(dir).is_err() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|err| OsError::io("creating private socket directory", err))?;
        }
        let meta = std::fs::symlink_metadata(dir).map_err(|err| OsError::io("inspecting socket directory", err))?;
        if meta.file_type().is_symlink() {
            return Err(OsError::UnsafePath(format!(
                "{} is a symbolic link and cannot host a private broker socket",
                dir.display()
            )));
        }
        if !meta.is_dir() {
            return Err(OsError::UnsafePath(format!("{} is not a directory", dir.display())));
        }
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if meta.uid() != euid {
            return Err(OsError::UnsafePath(format!("{} is owned by another user", dir.display())));
        }
        if meta.mode() & 0o022 != 0 {
            return Err(OsError::UnsafePath(format!("{} is writable by group or others", dir.display())));
        }
        Ok(())
    }
}

#[cfg(unix)]
static UMASK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(unix)]
struct UmaskGuard {
    prev: libc::mode_t,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
impl UmaskGuard {
    fn acquire(mask: libc::mode_t) -> Self {
        let lock = UMASK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: `umask` takes a mode mask and returns the previous mask; serialized by `UMASK_LOCK`.
        let prev = unsafe { libc::umask(mask) };
        Self { prev, _lock: lock }
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: restores the exact previous umask captured under `UMASK_LOCK` before `_lock` drops.
        unsafe { libc::umask(self.prev) };
    }
}

/// Binds a user-private local listener at `socket`.
///
/// - On Unix: refuses to overwrite any pre-existing non-socket file, narrows process umask to
///   `0o077` around bind (serialized via `UMASK_LOCK` and restored via RAII `Drop`) so the socket
///   is owner-only from creation without stripping owner `+x` from concurrently created directories,
///   then tightens the socket permissions to `0o600`, overwriting stale socket files once
///   [`acquire_socket_hold`] has established exclusivity.
/// - On Windows: attaches [`crate::security::owner_only_security_descriptor`] so `Everyone` never
///   receives pipe access, mapping `AddrInUse` / `PermissionDenied` to [`OsError::AlreadyHeld`].
pub fn bind_local_listener(socket: &str) -> Result<Listener, OsError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if let Ok(meta) = std::fs::symlink_metadata(socket)
            && !meta.file_type().is_socket()
        {
            return Err(OsError::UnsafePath(format!("{socket} already exists and is not a socket")));
        }
    }

    let name = socket.to_fs_name::<GenericFilePath>().map_err(|err| OsError::io("parsing local socket path", err))?;
    let options = ListenerOptions::new().name(name).try_overwrite(true);

    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        let sd = crate::security::owner_only_security_descriptor()
            .map_err(|err| OsError::io("building pipe security descriptor", err))?;
        options.security_descriptor(sd)
    };

    #[cfg(unix)]
    let _umask = UmaskGuard::acquire(0o077);
    let bound = options.create_sync();

    match bound {
        Ok(listener) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{FileTypeExt, PermissionsExt};
                let meta =
                    std::fs::symlink_metadata(socket).map_err(|err| OsError::io("inspecting bound socket", err))?;
                if !meta.file_type().is_socket() {
                    return Err(OsError::io(
                        "verifying bound socket type",
                        io::Error::new(io::ErrorKind::InvalidInput, "bound path is not a Unix socket"),
                    ));
                }
                std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
                    .map_err(|err| OsError::io("tightening socket permissions", err))?;
            }
            Ok(listener)
        }
        #[cfg(windows)]
        Err(err) => Err(crate::transport_windows::map_bind_error(err)),
        #[cfg(not(windows))]
        Err(err) => Err(OsError::io("binding local listener", err)),
    }
}

/// Connects to an existing local broker socket or named pipe.
pub fn connect_local_stream(socket: &str) -> io::Result<Stream> {
    let name = socket.to_fs_name::<GenericFilePath>()?;
    Stream::connect(name)
}

/// Verifies via kernel credentials (`SO_PEERCRED`/`getpeereid` on Unix, `GetSecurityInfo` pipe
/// owner SID on Windows) that the peer endpoint belongs to the current user.
pub fn verify_peer_is_current_user(stream: &Stream) -> io::Result<bool> {
    #[cfg(windows)]
    {
        let Stream::NamedPipe(pipe) = stream;
        crate::transport_windows::owned_by_this_user(pipe)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::traits::StreamCommon as _;
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let user = unsafe { libc::geteuid() };
        Ok(stream.peer_creds()?.euid() == Some(user))
    }
}

/// Abruptly shuts down a stream in both directions, unblocking parked reads/writes.
///
/// On macOS a socket whose peer already half-closed its write side returns `ENOTCONN` for
/// `Shutdown::Both`, so we fall back to `Shutdown::Write`.
pub fn hang_up(stream: &Stream) -> bool {
    match stream {
        #[cfg(unix)]
        Stream::UdSocket(socket) => {
            let inner = socket.inner();
            inner.shutdown(std::net::Shutdown::Both).is_ok() || inner.shutdown(std::net::Shutdown::Write).is_ok()
        }
        #[cfg(windows)]
        Stream::NamedPipe(pipe) => crate::transport_windows::hang_up(pipe),
    }
}

/// Gracefully releases a stream whose peer has been written everything it is owed without
/// discarding unread buffered output (critical on Windows named pipes, where `DisconnectNamedPipe`
/// discards unread bytes and `assume_flushed` prevents `interprocess`'s drop-flush thread from
/// wedging on a silent peer).
pub fn let_go(stream: &Stream, writers_ended: impl FnOnce()) -> bool {
    match stream {
        #[cfg(unix)]
        Stream::UdSocket(_) => {
            let cancelled = hang_up(stream);
            writers_ended();
            cancelled
        }
        #[cfg(windows)]
        Stream::NamedPipe(pipe) => crate::transport_windows::let_go(pipe, writers_ended),
    }
}

/// Configures non-blocking mode on a local [`Stream`].
pub fn set_stream_nonblocking(stream: &Stream, nonblocking: bool) -> io::Result<()> {
    stream.set_nonblocking(nonblocking)
}

/// Configures non-blocking accept mode on a local [`Listener`].
pub fn set_listener_nonblocking(listener: &Listener, nonblocking: bool) -> io::Result<()> {
    let mode = if nonblocking { ListenerNonblockingMode::Accept } else { ListenerNonblockingMode::Neither };
    listener.set_nonblocking(mode)
}

/// Returns the underlying Unix raw file descriptor of a local [`Stream`].
#[cfg(unix)]
pub fn stream_raw_fd(stream: &Stream) -> std::os::fd::RawFd {
    use std::os::fd::AsRawFd;
    let Stream::UdSocket(socket) = stream;
    socket.inner().as_raw_fd()
}

/// Returns the underlying Unix raw file descriptor of a local [`Listener`].
#[cfg(unix)]
pub fn listener_raw_fd(listener: &Listener) -> std::os::fd::RawFd {
    use std::os::fd::{AsFd, AsRawFd};
    let Listener::UdSocket(socket) = listener;
    socket.as_fd().as_raw_fd()
}

/// Performs a single vectored write (`writev`) of `slices`, retrying on `EINTR`.
///
/// Returns `Ok(0)` immediately when `slices` is empty.
pub fn write_vectored_some(writer: &mut impl Write, slices: &[IoSlice<'_>]) -> io::Result<usize> {
    if slices.is_empty() {
        return Ok(0);
    }
    loop {
        match writer.write_vectored(slices) {
            Ok(n) => return Ok(n),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_grace_parses_valid_and_falls_back() {
        assert_eq!(start_grace(None), START_GRACE);
        assert_eq!(start_grace(Some("")), START_GRACE);
        assert_eq!(start_grace(Some("not-a-number")), START_GRACE);
        assert_eq!(start_grace(Some("0")), Duration::from_millis(0));
        assert_eq!(start_grace(Some("250")), Duration::from_millis(250));
    }

    #[test]
    fn default_socket_path_is_non_empty() {
        let path = default_socket_path().expect("default socket path available");
        assert!(!path.is_empty());
        assert!(path.contains("apack-terminal"));
    }

    #[test]
    fn write_vectored_some_handles_empty_interrupted_and_multi_slice() {
        struct FlakyWriter {
            fail_once: bool,
            out: Vec<u8>,
        }
        impl Write for FlakyWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.fail_once {
                    self.fail_once = false;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
                }
                self.out.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
                if self.fail_once {
                    self.fail_once = false;
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "eintr"));
                }
                let mut total = 0;
                for s in bufs {
                    self.out.extend_from_slice(s);
                    total += s.len();
                }
                Ok(total)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut w = FlakyWriter { fail_once: true, out: Vec::new() };
        assert_eq!(write_vectored_some(&mut w, &[]).expect("empty"), 0);
        let slices = [IoSlice::new(b"hello "), IoSlice::new(b"world")];
        let written = write_vectored_some(&mut w, &slices).expect("vectored write");
        assert_eq!(written, 11);
        assert_eq!(w.out, b"hello world");

        struct BrokenWriter(usize);
        impl Write for BrokenWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                assert!(self.0 <= 2, "write_vectored_some retried non-EINTR error indefinitely");
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken"))
            }
            fn write_vectored(&mut self, _bufs: &[IoSlice<'_>]) -> io::Result<usize> {
                self.0 += 1;
                assert!(self.0 <= 2, "write_vectored_some retried non-EINTR error indefinitely");
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            write_vectored_some(&mut BrokenWriter(0), &slices).expect_err("broken pipe").kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_private_dir_hold_bind_peer_and_nonblocking_roundtrip() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(socket_parent_dir("bare.sock"), Path::new("."));
        assert_eq!(socket_parent_dir("/tmp/sub/broker.sock"), Path::new("/tmp/sub"));

        let base = tempfile::tempdir().expect("tempdir");
        let nested = base.path().join("private/broker.sock");
        let sock_str = nested.to_string_lossy().into_owned();

        // Creates missing private directory with 0o700.
        ensure_private_directory(&sock_str).expect("create private dir");
        let dir_meta = std::fs::symlink_metadata(nested.parent().expect("parent")).expect("meta");
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);

        // Rejects group/other writable directory.
        let wide_dir = base.path().join("wide");
        std::fs::create_dir(&wide_dir).expect("mkdir");
        std::fs::set_permissions(&wide_dir, std::fs::Permissions::from_mode(0o770)).expect("chmod");
        let wide_sock = wide_dir.join("broker.sock");
        assert!(matches!(ensure_private_directory(&wide_sock.to_string_lossy()), Err(OsError::UnsafePath(_))));

        // Rejects symlink directory.
        let link_dir = base.path().join("link_dir");
        std::os::unix::fs::symlink(nested.parent().expect("parent"), &link_dir).expect("symlink");
        assert!(matches!(
            ensure_private_directory(&link_dir.join("broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));

        // Rejects regular file as directory.
        let file_as_dir = base.path().join("not_a_dir");
        std::fs::write(&file_as_dir, b"x").expect("write");
        assert!(matches!(
            ensure_private_directory(&file_as_dir.join("broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));

        // Rejects binding over an existing regular file.
        let reg_sock = nested.parent().expect("parent").join("regular.sock");
        std::fs::write(&reg_sock, b"not a socket").expect("write");
        assert!(matches!(bind_local_listener(&reg_sock.to_string_lossy()), Err(OsError::UnsafePath(_))));

        // Exclusive lock prevents double-hold, AND `Drop` explicitly calls `LOCK_UN` so even when
        // a duplicated fd (simulating a forked child mid-exec) stays open, the lock is released!
        let hold = acquire_socket_hold(&sock_str).expect("first hold");
        assert!(matches!(acquire_socket_hold(&sock_str), Err(OsError::AlreadyHeld)));
        let dup_lock_fd = hold.lock.try_clone().expect("dup lock fd");

        let listener = bind_local_listener(&sock_str).expect("bind listener");
        // Verify `UmaskGuard::drop` restored the process umask (not left stuck at 0o077).
        {
            let _lock = UMASK_LOCK.lock().expect("umask lock");
            // SAFETY: probe current umask and restore immediately under `UMASK_LOCK`.
            let current_umask = unsafe {
                let cur = libc::umask(0o022);
                libc::umask(cur);
                cur
            };
            assert_ne!(current_umask, 0o077, "UmaskGuard::drop must restore previous umask");
        }
        let sock_meta = std::fs::symlink_metadata(&nested).expect("socket meta");
        assert_eq!(sock_meta.permissions().mode() & 0o777, 0o600, "socket must be 0o600 owner-only");
        let lfd = listener_raw_fd(&listener);
        assert!(lfd > 2, "listener fd must be > 2");

        // Verify `O_NONBLOCK` flag directly via fcntl BEFORE calling accept() so a broken
        // `set_listener_nonblocking` fails immediately instead of hanging.
        set_listener_nonblocking(&listener, true).expect("nonblocking accept");
        // SAFETY: `F_GETFL` on open listener fd.
        assert_ne!(unsafe { libc::fcntl(lfd, libc::F_GETFL) } & libc::O_NONBLOCK, 0);
        assert_eq!(listener.accept().expect_err("would block").kind(), io::ErrorKind::WouldBlock);
        set_listener_nonblocking(&listener, false).expect("blocking accept");
        // SAFETY: `F_GETFL` on open listener fd.
        assert_eq!(unsafe { libc::fcntl(lfd, libc::F_GETFL) } & libc::O_NONBLOCK, 0);

        let connect_path = sock_str.clone();
        let client_handle = std::thread::spawn(move || connect_local_stream(&connect_path).expect("connect"));
        let mut server = listener.accept().expect("accept");
        let mut client = client_handle.join().expect("join");

        assert!(verify_peer_is_current_user(&server).expect("peer check server"));
        assert!(verify_peer_is_current_user(&client).expect("peer check client"));
        let sfd = stream_raw_fd(&server);
        assert!(sfd > 2, "stream fd must be > 2");
        let cfd = stream_raw_fd(&client);
        assert!(cfd > 2 && cfd != sfd);

        // Vectored write + read roundtrip.
        let bufs = [IoSlice::new(b"apack-"), IoSlice::new(b"v2")];
        assert_eq!(write_vectored_some(&mut server, &bufs).expect("writev"), 8);
        let mut got = [0u8; 8];
        client.read_exact(&mut got).expect("read");
        assert_eq!(&got, b"apack-v2");

        // Verify `O_NONBLOCK` flag directly via fcntl BEFORE calling read().
        set_stream_nonblocking(&client, true).expect("nonblocking stream");
        // SAFETY: `F_GETFL` on open client fd.
        assert_ne!(unsafe { libc::fcntl(cfd, libc::F_GETFL) } & libc::O_NONBLOCK, 0);
        let mut extra = [0u8; 4];
        assert_eq!(client.read(&mut extra).expect_err("would block").kind(), io::ErrorKind::WouldBlock);

        // `let_go` shuts down `server` so `client` immediately observes EOF even while `server` is alive.
        let mut called = false;
        assert!(let_go(&server, || called = true));
        assert!(called);
        assert_eq!(client.read(&mut extra).expect("eof after hang_up"), 0);

        // Dropping `hold` while `dup_lock_fd` is still open must release the flock via `LOCK_UN`.
        drop(hold);
        let _reacquired = acquire_socket_hold(&sock_str).expect("reacquire after explicit LOCK_UN");
        let _ = dup_lock_fd.as_raw_fd();
    }
}
