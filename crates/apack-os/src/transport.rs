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
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .map_err(|err| OsError::io("opening socket lock file", err))?;
        if !lock.metadata().map_err(|err| OsError::io("inspecting socket lock file", err))?.is_file() {
            return Err(OsError::io(
                "inspecting socket lock file",
                io::Error::new(io::ErrorKind::InvalidInput, "socket lock path is not a regular file"),
            ));
        }

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

#[cfg(unix)]
fn ancestor_is_safe(uid: libc::uid_t, mode: u32, euid: libc::uid_t) -> bool {
    (uid == 0 || uid == euid) && ((mode & 0o022) == 0 || (mode & 0o1000) != 0)
}

#[cfg(unix)]
fn is_safe_ancestor_dir(is_dir: bool, uid: libc::uid_t, mode: u32, euid: libc::uid_t) -> bool {
    is_dir && ancestor_is_safe(uid, mode, euid)
}

#[cfg(unix)]
fn dir_is_private(is_symlink: bool, is_dir: bool, uid: libc::uid_t, mode: u32, euid: libc::uid_t) -> bool {
    !is_symlink && is_dir && uid == euid && (mode & 0o077) == 0
}

#[cfg(unix)]
fn ancestor_symlink_is_safe(uid: libc::uid_t, euid: libc::uid_t) -> bool {
    uid == 0 || uid == euid
}

#[cfg(unix)]
const MAX_ANCESTOR_SYMLINK_HOPS: usize = 40;

#[cfg(unix)]
enum WalkSegment {
    Normal(std::ffi::OsString),
    Parent,
}

#[cfg(unix)]
fn push_path_segments_front(queue: &mut std::collections::VecDeque<WalkSegment>, path: &Path) {
    use std::path::Component;
    let mut segs = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Normal(name) => segs.push(WalkSegment::Normal(name.to_os_string())),
            Component::ParentDir => segs.push(WalkSegment::Parent),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    for seg in segs.into_iter().rev() {
        queue.push_front(seg);
    }
}

/// Ensures the parent directory of `socket` exists with mode `0o700` (if missing) and verifies it
/// is a real directory (not a symlink) owned by the current effective UID with strict owner-only
/// permissions (`mode & 0o077 == 0`) by walking left-to-right across every directory and symlink
/// hop (verifying every traversed symlink owner and every traversed real directory).
pub fn ensure_private_directory(socket: &str) -> Result<(), OsError> {
    #[cfg(not(unix))]
    {
        let _ = socket;
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::collections::VecDeque;
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        use std::path::{Component, PathBuf};

        let dir = socket_parent_dir(socket);
        if dir.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(OsError::UnsafePath(format!("{} contains parent traversal (`..`)", dir.display())));
        }
        let abs = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            std::env::current_dir().map_err(|err| OsError::io("resolving socket directory", err))?.join(dir)
        };
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        let euid = unsafe { libc::geteuid() };

        let mut cur = PathBuf::from("/");
        let root_meta = std::fs::symlink_metadata(&cur).map_err(|err| OsError::io("inspecting root directory", err))?;
        if !is_safe_ancestor_dir(root_meta.is_dir(), root_meta.uid(), root_meta.mode(), euid) {
            return Err(OsError::UnsafePath("/ is not a safe root directory".to_owned()));
        }

        let mut pending: VecDeque<WalkSegment> = VecDeque::new();
        push_path_segments_front(&mut pending, &abs);
        if pending.is_empty() {
            return Err(OsError::UnsafePath("/ cannot host a private broker socket".to_owned()));
        }
        let mut remaining_hops = MAX_ANCESTOR_SYMLINK_HOPS;

        while let Some(segment) = pending.pop_front() {
            let name = match segment {
                WalkSegment::Parent => {
                    cur.pop();
                    continue;
                }
                WalkSegment::Normal(name) => name,
            };
            let is_leaf = pending.is_empty();
            let next = cur.join(&name);

            match std::fs::symlink_metadata(&next) {
                Ok(lmeta) => {
                    if lmeta.file_type().is_symlink() {
                        if is_leaf {
                            return Err(OsError::UnsafePath(format!(
                                "{} is a symbolic link and cannot host a private broker socket",
                                next.display()
                            )));
                        }
                        if !ancestor_symlink_is_safe(lmeta.uid(), euid) {
                            return Err(OsError::UnsafePath(format!(
                                "{} is a symlink owned by another user",
                                next.display()
                            )));
                        }
                        remaining_hops = remaining_hops.checked_sub(1).ok_or_else(|| {
                            OsError::UnsafePath(format!("{} exceeds symlink hop limit", dir.display()))
                        })?;
                        let target =
                            std::fs::read_link(&next).map_err(|err| OsError::io("reading ancestor symlink", err))?;
                        if target.is_absolute() {
                            cur = PathBuf::from("/");
                        }
                        push_path_segments_front(&mut pending, &target);
                        continue;
                    }
                    if !lmeta.is_dir() {
                        return Err(OsError::UnsafePath(format!("{} is not a directory", next.display())));
                    }
                    if is_leaf {
                        if !dir_is_private(false, true, lmeta.uid(), lmeta.mode(), euid) {
                            return Err(OsError::UnsafePath(format!(
                                "{} must be owned by current user with mode 0o700",
                                next.display()
                            )));
                        }
                        return Ok(());
                    }
                    if !ancestor_is_safe(lmeta.uid(), lmeta.mode(), euid) {
                        return Err(OsError::UnsafePath(format!(
                            "{} is an unsafe ancestor directory (wrong owner or non-sticky group/world writable)",
                            next.display()
                        )));
                    }
                    cur = next;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // `cur` is already a verified real safe directory; create strictly one component at a time.
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(&next)
                        .map_err(|e| OsError::io("creating private socket directory", e))?;
                    let created = std::fs::symlink_metadata(&next)
                        .map_err(|e| OsError::io("inspecting created socket directory", e))?;
                    if !dir_is_private(
                        created.file_type().is_symlink(),
                        created.is_dir(),
                        created.uid(),
                        created.mode(),
                        euid,
                    ) {
                        return Err(OsError::UnsafePath(format!(
                            "{} failed post-create private directory verification",
                            next.display()
                        )));
                    }
                    if is_leaf {
                        return Ok(());
                    }
                    cur = next;
                }
                Err(err) => return Err(OsError::io("inspecting socket directory", err)),
            }
        }
        Err(OsError::UnsafePath(format!("{} resolved to an empty path", dir.display())))
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

/// Verifies (client-side) via kernel credentials (`SO_PEERCRED`/`getpeereid` on Unix,
/// `GetSecurityInfo` pipe owner SID on Windows) that the broker endpoint belongs to the current user.
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

/// Verifies (server-side) via kernel credentials (`SO_PEERCRED`/`getpeereid` on Unix,
/// `GetNamedPipeClientProcessId` + client token user SID on Windows) that an accepted client
/// belongs to the current user.
pub fn verify_client_is_current_user(stream: &Stream) -> io::Result<bool> {
    #[cfg(windows)]
    {
        let Stream::NamedPipe(pipe) = stream;
        crate::transport_windows::client_is_this_user(pipe)
    }
    #[cfg(unix)]
    {
        verify_peer_is_current_user(stream)
    }
}

#[cfg(windows)]
pub use crate::transport_windows::{ShutdownSignalGuard, install_shutdown_signals};

#[cfg(unix)]
mod unix_signals {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

    pub(crate) const INVALID_FD: RawFd = -1;
    static SIGNAL_PIPE_WR: AtomicI32 = AtomicI32::new(INVALID_FD);
    static SIGNAL_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SignalReadAction {
        Wake,
        Retry,
        Stop,
    }

    pub(crate) fn classify_signal_read(n: isize, err_kind: io::ErrorKind) -> SignalReadAction {
        if n > 0 {
            SignalReadAction::Wake
        } else if n < 0 && err_kind == io::ErrorKind::Interrupted {
            SignalReadAction::Retry
        } else {
            SignalReadAction::Stop
        }
    }

    const SIGNAL_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Waits up to `timeout` until every in-flight `handle_shutdown_signal` invocation has exited so
    /// closing the write end of the self-pipe can never race a signal handler `write(2)` onto a reused fd.
    pub(crate) fn wait_for_signal_handlers(in_flight: &AtomicUsize, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        while in_flight.load(Ordering::SeqCst) != 0 {
            if start.elapsed() >= timeout {
                return false;
            }
            std::hint::spin_loop();
        }
        true
    }

    #[inline]
    // SAFETY: returns the calling thread's libc `errno` pointer; valid for the lifetime of the thread.
    unsafe fn errno_ptr() -> *mut libc::c_int {
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            // SAFETY: platform libc thread-local errno accessor is async-signal-safe.
            unsafe { libc::__error() }
        }
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        {
            // SAFETY: platform libc thread-local errno accessor is async-signal-safe.
            unsafe { libc::__errno() }
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: glibc/musl thread-local errno accessor is async-signal-safe.
            unsafe { libc::__errno_location() }
        }
    }

    extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
        // SAFETY: preserving thread-local `errno` across `write(2)` is POSIX async-signal-safe.
        let err_ptr = unsafe { errno_ptr() };
        // SAFETY: `err_ptr` points to the current thread's `errno` storage.
        let saved_errno = unsafe { *err_ptr };
        SIGNAL_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        let fd = SIGNAL_PIPE_WR.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte: u8 = 1;
            // SAFETY: `write(2)` of 1 byte to a non-blocking pipe fd is POSIX async-signal-safe,
            // and `SIGNAL_IN_FLIGHT` pins `fd` open until `fetch_sub` completes below.
            let _ = unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
        }
        SIGNAL_IN_FLIGHT.fetch_sub(1, Ordering::Release);
        // SAFETY: restoring saved `errno` before returning from signal handler.
        unsafe {
            *err_ptr = saved_errno;
        }
    }

    /// Guard keeping Unix `SIGINT`/`SIGTERM` graceful shutdown handling active until dropped.
    pub struct ShutdownSignalGuard {
        prev_int: libc::sigaction,
        prev_term: libc::sigaction,
        wr_fd: Option<OwnedFd>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl ShutdownSignalGuard {
        #[cfg(test)]
        pub(crate) fn write_fd(&self) -> RawFd {
            self.wr_fd.as_ref().expect("live signal guard write fd").as_raw_fd()
        }
    }

    impl Drop for ShutdownSignalGuard {
        fn drop(&mut self) {
            // SAFETY: restoring previous signal dispositions saved in `install_shutdown_signals`.
            unsafe {
                libc::sigaction(libc::SIGINT, &self.prev_int, std::ptr::null_mut());
                libc::sigaction(libc::SIGTERM, &self.prev_term, std::ptr::null_mut());
            }
            // Clear the global fd and drain any in-flight handler BEFORE closing `wr_fd` so a
            // concurrent handler on another thread can never `write(2)` into a reused fd number.
            SIGNAL_PIPE_WR.store(INVALID_FD, Ordering::SeqCst);
            if wait_for_signal_handlers(&SIGNAL_IN_FLIGHT, SIGNAL_DRAIN_TIMEOUT) {
                drop(self.wr_fd.take());
                if let Some(handle) = self.worker.take() {
                    let _ = handle.join();
                }
            } else if let Some(fd) = self.wr_fd.take() {
                // Fail-safe: never close `wr_fd` while a handler is still executing `write(fd)`.
                std::mem::forget(fd);
            }
        }
    }

    /// Installs async-signal-safe `SIGINT` and `SIGTERM` handlers that set `flag` and invoke `wake`.
    pub fn install_shutdown_signals(
        flag: Arc<AtomicBool>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<ShutdownSignalGuard> {
        let (rd_fd, wr_fd) = crate::pty_poller::create_nonblocking_pipe()?;
        // Reader thread blocks cleanly in `read(2)` until a signal byte arrives or `wr_fd` closes.
        crate::pty::set_fd_nonblocking(rd_fd.as_fd(), false)?;
        let wr_raw = wr_fd.as_raw_fd();

        if SIGNAL_PIPE_WR.compare_exchange(INVALID_FD, wr_raw, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "shutdown signal handler already installed"));
        }

        // Spawn the reader thread BEFORE installing signal handlers so a spawn failure rolls back
        // cleanly without ever modifying process signal dispositions.
        let worker = match std::thread::Builder::new().name("apack-signals".to_owned()).spawn(move || {
            let mut buf = [0u8; 16];
            loop {
                // SAFETY: `rd_fd` is owned exclusively by this thread and `buf` is valid for 16 bytes.
                let n = unsafe { libc::read(rd_fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                let kind = io::Error::last_os_error().kind();
                match classify_signal_read(n, kind) {
                    SignalReadAction::Wake => {
                        flag.store(true, Ordering::Release);
                        wake();
                    }
                    SignalReadAction::Retry => continue,
                    SignalReadAction::Stop => break,
                }
            }
        }) {
            Ok(w) => w,
            Err(err) => {
                SIGNAL_PIPE_WR.store(INVALID_FD, Ordering::SeqCst);
                return Err(err);
            }
        };

        // SAFETY: zero-initialized `libc::sigaction` is valid before setting `sa_sigaction`/`sa_mask`/`sa_flags`.
        let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
        sa.sa_sigaction = handle_shutdown_signal as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        // SAFETY: `sigemptyset` initializes `sa.sa_mask`.
        unsafe {
            libc::sigemptyset(&raw mut sa.sa_mask);
        }
        // SAFETY: out-buffer populated by `sigaction`.
        let mut prev_int: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: out-buffer populated by `sigaction`.
        let mut prev_term: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: installing async-signal-safe handler for `SIGINT`.
        let int_ok = unsafe { libc::sigaction(libc::SIGINT, &sa, &mut prev_int) } == 0;
        // SAFETY: installing async-signal-safe handler for `SIGTERM`, rolling back `SIGINT` on error.
        let term_ok = int_ok && unsafe { libc::sigaction(libc::SIGTERM, &sa, &mut prev_term) } == 0;
        if !term_ok {
            let err = io::Error::last_os_error();
            if int_ok {
                // SAFETY: restoring `SIGINT` disposition saved above.
                unsafe {
                    libc::sigaction(libc::SIGINT, &prev_int, std::ptr::null_mut());
                }
            }
            SIGNAL_PIPE_WR.store(INVALID_FD, Ordering::SeqCst);
            if wait_for_signal_handlers(&SIGNAL_IN_FLIGHT, SIGNAL_DRAIN_TIMEOUT) {
                drop(wr_fd);
                let _ = worker.join();
            } else {
                std::mem::forget(wr_fd);
            }
            return Err(err);
        }

        Ok(ShutdownSignalGuard { prev_int, prev_term, wr_fd: Some(wr_fd), worker: Some(worker) })
    }
}

#[cfg(unix)]
pub use unix_signals::{ShutdownSignalGuard, install_shutdown_signals};

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

/// Returns the borrowed Unix file descriptor of a local [`Stream`].
#[cfg(unix)]
pub fn stream_fd(stream: &Stream) -> std::os::fd::BorrowedFd<'_> {
    use std::os::fd::AsFd;
    let Stream::UdSocket(socket) = stream;
    socket.inner().as_fd()
}

/// Returns the borrowed Unix file descriptor of a local [`Listener`].
#[cfg(unix)]
pub fn listener_fd(listener: &Listener) -> std::os::fd::BorrowedFd<'_> {
    use std::os::fd::AsFd;
    let Listener::UdSocket(socket) = listener;
    socket.as_fd()
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

        // Rejects group/other readable or writable directory (strict 0o700).
        let wide_dir = base.path().join("wide");
        std::fs::create_dir(&wide_dir).expect("mkdir");
        std::fs::set_permissions(&wide_dir, std::fs::Permissions::from_mode(0o750)).expect("chmod 0750");
        let wide_sock = wide_dir.join("broker.sock");
        assert!(matches!(ensure_private_directory(&wide_sock.to_string_lossy()), Err(OsError::UnsafePath(_))));
        std::fs::set_permissions(&wide_dir, std::fs::Permissions::from_mode(0o755)).expect("chmod 0755");
        assert!(matches!(ensure_private_directory(&wide_sock.to_string_lossy()), Err(OsError::UnsafePath(_))));

        // Ancestor safety rules: root or euid, non-writable or sticky; ancestor symlinks owned by root or euid.
        assert!(ancestor_is_safe(0, 0o755, 1000));
        assert!(ancestor_is_safe(0, 0o1777, 1000));
        assert!(!ancestor_is_safe(0, 0o777, 1000));
        assert!(!ancestor_is_safe(0, 0o775, 1000));
        assert!(ancestor_is_safe(1000, 0o700, 1000));
        assert!(ancestor_is_safe(1000, 0o1770, 1000));
        assert!(!ancestor_is_safe(1000, 0o770, 1000));
        assert!(!ancestor_is_safe(1001, 0o700, 1000));
        assert!(!ancestor_is_safe(1001, 0o1777, 1000));
        assert!(ancestor_symlink_is_safe(0, 1000));
        assert!(ancestor_symlink_is_safe(1000, 1000));
        assert!(!ancestor_symlink_is_safe(1001, 1000));
        assert!(is_safe_ancestor_dir(true, 0, 0o755, 1000));
        assert!(!is_safe_ancestor_dir(false, 0, 0o755, 1000));
        assert!(!is_safe_ancestor_dir(true, 0, 0o777, 1000));
        assert!(dir_is_private(false, true, 1000, 0o700, 1000));
        assert!(!dir_is_private(true, true, 1000, 0o700, 1000));
        assert!(!dir_is_private(false, false, 1000, 0o700, 1000));
        assert!(!dir_is_private(false, true, 1001, 0o700, 1000));
        assert!(!dir_is_private(false, true, 1000, 0o750, 1000));

        // Ancestor symlink owned by euid pointing to a safe parent is allowed, while target dir symlink is refused.
        let anc_sym = base.path().join("anc-sym");
        std::os::unix::fs::symlink(base.path(), &anc_sym).expect("anc symlink");
        ensure_private_directory(&anc_sym.join("private/broker.sock").to_string_lossy()).expect("owned anc symlink ok");

        // Rejects a 0o700 socket directory hosted under a non-sticky group/world-writable ancestor,
        // refuses to create a missing directory under an unsafe ancestor, and catches canonical
        // unsafe ancestors hidden behind an euid-owned symlink.
        let unsafe_parent = base.path().join("unsafe-parent");
        let safe_child = unsafe_parent.join("priv");
        std::fs::create_dir_all(&safe_child).expect("mkdir unsafe tree");
        std::fs::set_permissions(&safe_child, std::fs::Permissions::from_mode(0o700)).expect("chmod child");
        std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o777)).expect("chmod parent");
        assert!(matches!(
            ensure_private_directory(&safe_child.join("broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));
        let uncreated_child = unsafe_parent.join("must-not-create");
        assert!(matches!(
            ensure_private_directory(&uncreated_child.join("broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));
        assert!(!uncreated_child.exists(), "must not create directory beneath an unsafe ancestor");
        let sym_to_unsafe_target = base.path().join("sym-to-unsafe");
        std::os::unix::fs::symlink(&safe_child, &sym_to_unsafe_target).expect("symlink to safe_child");
        let deep_priv = safe_child.join("deep");
        std::fs::create_dir(&deep_priv).expect("mkdir deep");
        std::fs::set_permissions(&deep_priv, std::fs::Permissions::from_mode(0o700)).expect("chmod deep");
        assert!(matches!(
            ensure_private_directory(&sym_to_unsafe_target.join("deep/broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));
        // Multi-hop symlink bounce through an unsafe directory and back to a safe directory MUST be refused!
        let bounce_safe = base.path().join("bounce-safe");
        let bounce_priv = bounce_safe.join("priv");
        std::fs::create_dir_all(&bounce_priv).expect("mkdir bounce");
        std::fs::set_permissions(&bounce_safe, std::fs::Permissions::from_mode(0o700)).expect("chmod bounce safe");
        std::fs::set_permissions(&bounce_priv, std::fs::Permissions::from_mode(0o700)).expect("chmod bounce priv");
        let link_b = safe_child.join("link-b");
        std::os::unix::fs::symlink(&bounce_safe, &link_b).expect("link_b");
        assert!(matches!(
            ensure_private_directory(&sym_to_unsafe_target.join("link-b/priv/broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));
        // `..` in socket path is refused BEFORE creating any missing component.
        let nonexist_escape = base.path().join("nonexist/../unsafe-parent/priv-escape/broker.sock");
        assert!(matches!(ensure_private_directory(&nonexist_escape.to_string_lossy()), Err(OsError::UnsafePath(_))));
        assert!(!base.path().join("nonexist").exists());
        assert!(!unsafe_parent.join("priv-escape").exists());
        // Symlink loop in ancestor chain fails closed cleanly.
        let loop1 = base.path().join("loop1");
        let loop2 = base.path().join("loop2");
        std::os::unix::fs::symlink(&loop2, &loop1).expect("loop1");
        std::os::unix::fs::symlink(&loop1, &loop2).expect("loop2");
        assert!(matches!(
            ensure_private_directory(&loop1.join("priv/broker.sock").to_string_lossy()),
            Err(OsError::UnsafePath(_))
        ));
        // Multi-level missing directory creation under a safe ancestor creates every level with 0o700.
        let deep_new = base.path().join("nested_a/nested_b/broker.sock");
        ensure_private_directory(&deep_new.to_string_lossy()).expect("create nested 0o700 dirs");
        assert_eq!(
            std::fs::symlink_metadata(base.path().join("nested_a")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(base.path().join("nested_a/nested_b")).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Root `/` cannot host a private socket directly.
        assert!(matches!(ensure_private_directory("/broker.sock"), Err(OsError::UnsafePath(_))));
        // Non-NotFound metadata errors (e.g. ENAMETOOLONG) fail at inspection rather than attempting mkdir.
        let overlong = base.path().join("a".repeat(300)).join("broker.sock");
        assert!(matches!(
            ensure_private_directory(&overlong.to_string_lossy()),
            Err(OsError::Io { context: "inspecting socket directory", .. })
        ));
        std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o1777)).expect("chmod sticky");
        ensure_private_directory(&safe_child.join("broker.sock").to_string_lossy()).expect("sticky ancestor ok");
        std::fs::set_permissions(&unsafe_parent, std::fs::Permissions::from_mode(0o700)).expect("restore parent");

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

        // A readerless FIFO at `{socket}.lock` must fail promptly (O_NONBLOCK + is_file()) without hanging!
        let fifo_sock = nested.parent().expect("parent").join("fifo-hold.sock");
        let fifo_lock = nested.parent().expect("parent").join("fifo-hold.sock.lock");
        assert!(std::process::Command::new("mkfifo").arg(&fifo_lock).status().expect("mkfifo").success());
        assert!(acquire_socket_hold(&fifo_sock.to_string_lossy()).is_err());

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
        let lfd = listener_fd(&listener).as_raw_fd();
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
        assert!(verify_client_is_current_user(&server).expect("client check server"));
        let sfd = stream_fd(&server).as_raw_fd();
        assert!(sfd > 2, "stream fd must be > 2");
        let cfd = stream_fd(&client).as_raw_fd();
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

    #[cfg(unix)]
    #[test]
    fn unix_shutdown_signals_guard_catches_sigterm_sigint_and_reinstalls_after_drop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // Pure read-outcome classifier covers Wake / Retry (EINTR) / Stop (EOF & fatal) without stalling.
        const { assert!(unix_signals::INVALID_FD < 0) };
        assert_eq!(unix_signals::classify_signal_read(1, io::ErrorKind::Other), unix_signals::SignalReadAction::Wake);
        assert_eq!(
            unix_signals::classify_signal_read(0, io::ErrorKind::Interrupted),
            unix_signals::SignalReadAction::Stop
        );
        assert_eq!(
            unix_signals::classify_signal_read(-1, io::ErrorKind::Interrupted),
            unix_signals::SignalReadAction::Retry
        );
        assert_eq!(unix_signals::classify_signal_read(-1, io::ErrorKind::Other), unix_signals::SignalReadAction::Stop);

        let flag = Arc::new(AtomicBool::new(false));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let guard = install_shutdown_signals(Arc::clone(&flag), move || {
            wakes_cb.fetch_add(1, Ordering::AcqRel);
        })
        .expect("install shutdown signals");

        // Verify pipe write fd carries both FD_CLOEXEC and O_NONBLOCK.
        let wr = guard.write_fd();
        // SAFETY: querying flags on open owned pipe descriptor.
        assert_ne!(unsafe { libc::fcntl(wr, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        // SAFETY: querying status flags on open owned pipe descriptor.
        assert_ne!(unsafe { libc::fcntl(wr, libc::F_GETFL) } & libc::O_NONBLOCK, 0);

        // Duplicate install while guard is active fails with AlreadyExists.
        let dup = install_shutdown_signals(Arc::new(AtomicBool::new(false)), || {});
        assert_eq!(dup.err().map(|e| e.kind()), Some(io::ErrorKind::AlreadyExists));

        // Raising SIGTERM flips flag and fires wake.
        // SAFETY: sending SIGTERM to self while our async-signal-safe handler is installed.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        let start = std::time::Instant::now();
        while (!flag.load(Ordering::Acquire) || wakes.load(Ordering::Acquire) < 1)
            && start.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(flag.load(Ordering::Acquire), "SIGTERM must set shutdown flag");
        let after_term = wakes.load(Ordering::Acquire);
        assert!(after_term >= 1);

        // Raising SIGINT also wakes cleanly.
        flag.store(false, Ordering::Release);
        // SAFETY: sending SIGINT to self while our async-signal-safe handler is installed.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        let start2 = std::time::Instant::now();
        while (!flag.load(Ordering::Acquire) || wakes.load(Ordering::Acquire) <= after_term)
            && start2.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(flag.load(Ordering::Acquire), "SIGINT must set shutdown flag");
        assert!(wakes.load(Ordering::Acquire) > after_term);

        // Verify `wait_for_signal_handlers` waits until an in-flight handler decrements the counter to 0,
        // and returns `false` cleanly on timeout rather than spinning forever.
        let stuck = AtomicUsize::new(1);
        assert!(!unix_signals::wait_for_signal_handlers(&stuck, Duration::from_millis(5)));
        let idle = AtomicUsize::new(0);
        assert!(unix_signals::wait_for_signal_handlers(&idle, Duration::from_millis(50)));
        let in_flight = Arc::new(AtomicUsize::new(1));
        let releaser = Arc::clone(&in_flight);
        let t_rel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            releaser.fetch_sub(1, Ordering::Release);
        });
        assert!(unix_signals::wait_for_signal_handlers(&in_flight, Duration::from_secs(1)));
        assert_eq!(in_flight.load(Ordering::Acquire), 0);
        t_rel.join().expect("join releaser");

        // Dropping guard restores handlers and releases the global slot so reinstall succeeds.
        drop(guard);
        let guard2 = install_shutdown_signals(Arc::new(AtomicBool::new(false)), || {})
            .expect("reinstall after drop must succeed");
        drop(guard2);
    }
}
