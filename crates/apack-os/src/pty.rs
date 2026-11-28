//! Cross-platform PTY spawning, sizing, and non-blocking Unix master I/O.
//!
//! - On Unix, the master PTY fd is placed into `O_NONBLOCK` so a single shard thread can multiplex
//!   many sessions via [`crate::pty_poller::PtyPoller`] with zero per-session reader/writer threads.
//! - On Windows, [`crate::pty_windows::ConPtyBridge`] isolates blocking ConPTY handles behind
//!   bounded channels and drops the master handle off-shard so session teardown never blocks.

use std::io;

use apack_proto::types::{CellPx, ForegroundProcess, GridSize};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::OsError;
use crate::tree::{TreeGuard, foreground_argv, foreground_process};

/// Default `TERM` injected before caller overrides so shells spawned from GUI launchers have
/// working terminfo.
pub const DEFAULT_TERM: &str = "xterm-256color";

/// Environment variable carrying the marks key; stripped by default unless explicitly supplied.
pub const MARKS_KEY_ENV: &str = "APACK_TERMINAL_KEY";

/// Outcome of a single non-blocking read on a Unix PTY master fd.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonBlockingRead {
    /// `n > 0` bytes were read into the caller's buffer.
    Bytes(usize),
    /// No bytes are currently available (`EAGAIN` / `EWOULDBLOCK`).
    WouldBlock,
    /// Slave side closed (EOF or Linux `EIO` after slave close).
    Eof,
}

/// Specification for spawning a new PTY-backed child process.
#[derive(Debug, Clone)]
pub struct PtySpawnConfig {
    /// Initial terminal dimensions (validated non-zero and bounded by `GridSize`).
    pub size: GridSize,
    /// Initial cell pixel size (for `TIOCSWINSZ` pixel fields).
    pub cell_px: CellPx,
    /// Optional explicit executable; defaults to the platform default shell when `None`.
    pub shell: Option<String>,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Environment overrides applied after `TERM=xterm-256color`.
    pub env: Vec<(String, String)>,
    /// Environment variable names removed after applying `env`.
    pub unset: Vec<String>,
    /// Optional working directory.
    pub cwd: Option<String>,
}

/// Owned handle to a PTY master supporting resize, foreground inspection, and non-blocking Unix I/O.
pub struct PtyMaster {
    pub(crate) master: Box<dyn MasterPty + Send>,
    #[cfg(unix)]
    raw_fd: std::os::fd::RawFd,
}

impl PtyMaster {
    /// Resizes the kernel PTY dimensions (sending `SIGWINCH` on Unix / ConPTY resize on Windows).
    pub fn resize(&self, size: GridSize, cell_px: CellPx) -> Result<(), OsError> {
        let pty_size = to_pty_size(size, cell_px);
        self.master.resize(pty_size).map_err(|err| OsError::io("resizing pty", io::Error::other(err.to_string())))
    }

    /// Queries the current kernel PTY dimensions.
    pub fn get_size(&self) -> Result<PtySize, OsError> {
        self.master.get_size().map_err(|err| OsError::io("querying pty size", io::Error::other(err.to_string())))
    }

    /// Queries the foreground process group leader PID and executable name (Unix only).
    pub fn foreground_process(&self) -> Option<ForegroundProcess> {
        foreground_process(self.master.as_ref())
    }

    /// Queries the foreground process group leader PID via `tcgetpgrp` (fast, non-blocking; Unix only).
    #[cfg(unix)]
    pub fn foreground_pgid(&self) -> Option<u32> {
        let pid = self.master.process_group_leader()?;
        u32::try_from(pid).ok()
    }

    /// Queries the foreground process group leader `argv` for status vouching (Unix only).
    pub fn foreground_argv(&self) -> Option<Vec<String>> {
        foreground_argv(self.master.as_ref())
    }

    /// Returns the non-blocking Unix master PTY file descriptor borrowed for the lifetime of
    /// `self`, suitable for registration with [`crate::pty_poller::PtyPoller`].
    #[cfg(unix)]
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        // SAFETY: `self.raw_fd` is owned by `self.master` and stays open for the lifetime of `&self`.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(self.raw_fd) }
    }

    /// Performs a single non-blocking read from the Unix PTY master fd.
    ///
    /// Treats both `0` and `EIO` (Linux's signal when the slave side closes) as
    /// [`NonBlockingRead::Eof`], retries `EINTR`, and maps `EAGAIN`/`EWOULDBLOCK` to
    /// [`NonBlockingRead::WouldBlock`].
    #[cfg(unix)]
    pub fn read_nonblocking(&self, buf: &mut [u8]) -> io::Result<NonBlockingRead> {
        read_fd_nonblocking(self.fd(), buf)
    }

    /// Performs a single non-blocking write to the Unix PTY master fd.
    ///
    /// Returns `Ok(0)` on `EAGAIN`/`EWOULDBLOCK` (caller should arm writable interest) and retries
    /// `EINTR`.
    #[cfg(unix)]
    pub fn write_nonblocking(&self, data: &[u8]) -> io::Result<usize> {
        write_fd_nonblocking(self.fd(), data)
    }
}

#[cfg(unix)]
impl std::os::fd::AsFd for PtyMaster {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd()
    }
}

/// Everything returned by [`spawn_pty`].
pub struct SpawnedPty {
    /// Leader process ID (or `0` if the OS did not report one).
    pub pid: u32,
    /// Kernel-enforced process-tree guard (`JobGuard` on Windows, leader tracker on Unix).
    pub tree: Option<TreeGuard>,
    /// Master PTY handle.
    pub master: PtyMaster,
    /// Child wait handle (typically handed to a lightweight waiter or reaped via `try_wait`).
    pub child: Box<dyn Child + Send + Sync>,
    /// Independent kill handle so termination never blocks behind `child.wait()`.
    pub killer: Box<dyn ChildKiller + Send + Sync>,
}

/// Spawns a child process inside a fresh PTY with safe environment defaults and immediate
/// [`TreeGuard`] attachment.
pub fn spawn_pty(config: &PtySpawnConfig) -> Result<SpawnedPty, OsError> {
    let pair = native_pty_system()
        .openpty(to_pty_size(config.size, config.cell_px))
        .map_err(|err| OsError::io("opening pty", io::Error::other(err.to_string())))?;

    let mut cmd = match &config.shell {
        Some(program) => CommandBuilder::new(program),
        None => CommandBuilder::new_default_prog(),
    };
    for arg in &config.args {
        cmd.arg(arg);
    }
    cmd.env("TERM", DEFAULT_TERM);
    // Strip any inherited marks key before applying explicit caller env so unkeyed sessions never
    // accidentally inherit the broker's or a parent shell's key.
    cmd.env_remove(MARKS_KEY_ENV);
    for (name, value) in &config.env {
        cmd.env(name, value);
    }
    for name in &config.unset {
        cmd.env_remove(name);
    }
    if let Some(dir) = &config.cwd {
        cmd.cwd(dir);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| OsError::io("spawning shell", io::Error::other(err.to_string())))?;
    // Drop the slave handle immediately so the master sees EOF as soon as the child exits.
    drop(pair.slave);

    let pid = child.process_id().unwrap_or(0);
    let tree = TreeGuard::attach_child(child.as_ref(), pid);
    let killer = child.clone_killer();

    #[cfg(unix)]
    let mut child = child;
    #[cfg(unix)]
    let master = {
        let Some(raw_fd) = pair.master.as_raw_fd() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(OsError::io(
                "reading pty master fd",
                io::Error::other("unix master pty did not expose a raw fd"),
            ));
        };
        // SAFETY: `raw_fd` is owned by `pair.master` which stays alive for this call.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw_fd) };
        if let Err(err) = set_fd_nonblocking(borrowed, true) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(OsError::io("setting pty master O_NONBLOCK", err));
        }
        PtyMaster { master: pair.master, raw_fd }
    };

    #[cfg(windows)]
    let master = PtyMaster { master: pair.master };

    Ok(SpawnedPty { pid, tree, master, child, killer })
}

/// Converts protocol [`GridSize`] and [`CellPx`] into a `portable_pty::PtySize`.
pub fn to_pty_size(size: GridSize, cell_px: CellPx) -> PtySize {
    let (cw, ch) = if cell_px.width != 0 && cell_px.height != 0 { (cell_px.width, cell_px.height) } else { (0, 0) };
    let cols = size.cols.get();
    let rows = size.rows.get();
    PtySize { rows, cols, pixel_width: cols.saturating_mul(cw), pixel_height: rows.saturating_mul(ch) }
}

/// Sets or clears `O_NONBLOCK` on a Unix file descriptor.
#[cfg(unix)]
pub fn set_fd_nonblocking(fd: std::os::fd::BorrowedFd<'_>, nonblocking: bool) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    // SAFETY: `fcntl(F_GETFL)` reads file status flags for a borrowed live descriptor.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let updated = if nonblocking { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
    // SAFETY: `fcntl(F_SETFL)` updates the descriptor's status flags computed above.
    if unsafe { libc::fcntl(raw, libc::F_SETFL, updated) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadErrAction {
    Retry,
    WouldBlock,
    Eof,
    Fail,
}

#[cfg(unix)]
fn classify_read_error(err: &io::Error) -> ReadErrAction {
    if err.kind() == io::ErrorKind::Interrupted {
        return ReadErrAction::Retry;
    }
    if err.kind() == io::ErrorKind::WouldBlock {
        return ReadErrAction::WouldBlock;
    }
    if err.raw_os_error() == Some(libc::EIO) {
        return ReadErrAction::Eof;
    }
    ReadErrAction::Fail
}

/// Performs a single non-blocking read on `fd`, treating `0` and `EIO` as [`NonBlockingRead::Eof`].
#[cfg(unix)]
pub fn read_fd_nonblocking(fd: std::os::fd::BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<NonBlockingRead> {
    use std::os::fd::AsRawFd;
    if buf.is_empty() {
        return Ok(NonBlockingRead::Bytes(0));
    }
    let raw = fd.as_raw_fd();
    loop {
        // SAFETY: `raw` is a borrowed live descriptor and `buf` is a live mutable byte slice.
        let n = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            return Ok(NonBlockingRead::Bytes(n as usize));
        }
        if n == 0 {
            return Ok(NonBlockingRead::Eof);
        }
        let err = io::Error::last_os_error();
        match classify_read_error(&err) {
            ReadErrAction::Retry => continue,
            ReadErrAction::WouldBlock => return Ok(NonBlockingRead::WouldBlock),
            ReadErrAction::Eof => return Ok(NonBlockingRead::Eof),
            ReadErrAction::Fail => return Err(err),
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteErrAction {
    Retry,
    WouldBlock,
    Fail,
}

#[cfg(unix)]
fn classify_write_error(err: &io::Error) -> WriteErrAction {
    if err.kind() == io::ErrorKind::Interrupted {
        return WriteErrAction::Retry;
    }
    if err.kind() == io::ErrorKind::WouldBlock {
        return WriteErrAction::WouldBlock;
    }
    WriteErrAction::Fail
}

/// Performs a single non-blocking write on `fd`, returning `Ok(0)` on `EAGAIN`/`EWOULDBLOCK`.
#[cfg(unix)]
pub fn write_fd_nonblocking(fd: std::os::fd::BorrowedFd<'_>, data: &[u8]) -> io::Result<usize> {
    use std::os::fd::AsRawFd;
    if data.is_empty() {
        return Ok(0);
    }
    let raw = fd.as_raw_fd();
    loop {
        // SAFETY: `raw` is a borrowed live descriptor and `data` is a live byte slice.
        let n = unsafe { libc::write(raw, data.as_ptr().cast(), data.len()) };
        if n != -1 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        match classify_write_error(&err) {
            WriteErrAction::Retry => continue,
            WriteErrAction::WouldBlock => return Ok(0),
            WriteErrAction::Fail => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_pty_size_computes_pixels_and_zeroes_partial_cell_px() {
        let size = GridSize::new(80, 24).expect("valid grid");
        let full = to_pty_size(size, CellPx { width: 9, height: 18 });
        assert_eq!(full.cols, 80);
        assert_eq!(full.rows, 24);
        assert_eq!(full.pixel_width, 720);
        assert_eq!(full.pixel_height, 432);

        let partial = to_pty_size(size, CellPx { width: 9, height: 0 });
        assert_eq!(partial.pixel_width, 0);
        assert_eq!(partial.pixel_height, 0);

        let partial2 = to_pty_size(size, CellPx { width: 0, height: 18 });
        assert_eq!(partial2.pixel_width, 0);
        assert_eq!(partial2.pixel_height, 0);
    }

    #[cfg(unix)]
    #[test]
    fn set_fd_nonblocking_and_error_classifiers() {
        use std::os::fd::{AsFd, AsRawFd};
        let (r, w) = crate::pty_poller::create_nonblocking_pipe().expect("pipe");
        // 1-byte write kills `n != -1` -> `n != 1` mutant in `write_fd_nonblocking`.
        assert_eq!(write_fd_nonblocking(w.as_fd(), b"x").expect("1-byte write"), 1);
        let mut one = [0u8; 1];
        assert_eq!(read_fd_nonblocking(r.as_fd(), &mut one).expect("1-byte read"), NonBlockingRead::Bytes(1));
        assert_eq!(&one, b"x");
        // Clear nonblocking, check flag cleared, then re-enable and check flag set.
        set_fd_nonblocking(r.as_fd(), false).expect("clear nonblock");
        // SAFETY: `F_GETFL` on open pipe fd.
        let cleared = unsafe { libc::fcntl(r.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(cleared & libc::O_NONBLOCK, 0);

        set_fd_nonblocking(r.as_fd(), true).expect("set nonblock");
        // SAFETY: `F_GETFL` on open pipe fd.
        let set_flags = unsafe { libc::fcntl(r.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(set_flags & libc::O_NONBLOCK, 0);
        // Setting again when already set must keep O_NONBLOCK set (kills `|` -> `^`).
        set_fd_nonblocking(r.as_fd(), true).expect("set nonblock idempotent");
        // SAFETY: `F_GETFL` on open pipe fd.
        let still_set = unsafe { libc::fcntl(r.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(still_set & libc::O_NONBLOCK, 0);

        // Error classifiers cover EINTR, WouldBlock, EIO, and hard failures.
        assert_eq!(classify_read_error(&io::Error::new(io::ErrorKind::Interrupted, "")), ReadErrAction::Retry);
        assert_eq!(classify_read_error(&io::Error::new(io::ErrorKind::WouldBlock, "")), ReadErrAction::WouldBlock);
        assert_eq!(classify_read_error(&io::Error::from_raw_os_error(libc::EIO)), ReadErrAction::Eof);
        assert_eq!(classify_read_error(&io::Error::new(io::ErrorKind::PermissionDenied, "")), ReadErrAction::Fail);

        assert_eq!(classify_write_error(&io::Error::new(io::ErrorKind::Interrupted, "")), WriteErrAction::Retry);
        assert_eq!(classify_write_error(&io::Error::new(io::ErrorKind::WouldBlock, "")), WriteErrAction::WouldBlock);
        assert_eq!(classify_write_error(&io::Error::new(io::ErrorKind::BrokenPipe, "")), WriteErrAction::Fail);
    }

    #[cfg(unix)]
    #[test]
    fn unix_pty_spawn_echo_resize_foreground_and_eof() {
        use std::time::{Duration, Instant};

        let config = PtySpawnConfig {
            size: GridSize::new(80, 24).expect("grid"),
            cell_px: CellPx { width: 8, height: 16 },
            shell: Some("/bin/sh".to_owned()),
            args: vec![
                "-c".to_owned(),
                "printf 'HI:%s:%s\\n' \"$TERM\" \"$APACK_TEST_VAR\"; read line; printf 'GOT:%s\\n' \"$line\""
                    .to_owned(),
            ],
            env: vec![("APACK_TEST_VAR".to_owned(), "ok".to_owned())],
            unset: vec![],
            cwd: None,
        };
        let mut spawned = spawn_pty(&config).expect("spawn pty");
        assert!(spawned.pid >= 2);
        {
            use std::os::fd::{AsFd, AsRawFd};
            assert!(spawned.master.as_fd().as_raw_fd() > 2, "master fd must be an opened descriptor");
        }
        assert!(spawned.tree.as_ref().expect("tree").owns(spawned.pid).expect("owns"));

        spawned.master.resize(GridSize::new(100, 30).expect("grid"), CellPx { width: 8, height: 16 }).expect("resize");
        let after_resize = spawned.master.get_size().expect("get_size");
        assert_eq!((after_resize.cols, after_resize.rows), (100, 30));

        // Wait until the shell prints its banner so we know it is running and waiting on `read line`,
        // then inspect `foreground_process` and `foreground_argv`.
        let mut output = Vec::new();
        let mut buf = [0u8; 1024];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !String::from_utf8_lossy(&output).contains("HI:xterm-256color:ok") {
            assert!(Instant::now() < deadline, "timed out waiting for banner: {:?}", String::from_utf8_lossy(&output));
            match spawned.master.read_nonblocking(&mut buf).expect("read banner") {
                NonBlockingRead::Bytes(n) => output.extend_from_slice(&buf[..n]),
                NonBlockingRead::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                NonBlockingRead::Eof => panic!("unexpected EOF before banner"),
            }
        }

        let fg = spawned.master.foreground_process().expect("foreground process while shell waits");
        assert_eq!(fg.pid.pid() as u32, spawned.pid);
        assert_eq!(spawned.master.foreground_pgid(), Some(spawned.pid));
        assert!(fg.name.is_some());
        let argv = spawned.master.foreground_argv().expect("foreground argv while shell waits");
        assert!(argv.iter().any(|arg| arg.contains("sh")), "unexpected argv: {argv:?}");

        // Send a line for `read line`.
        let mut to_write = b"ping\n".as_slice();
        while !to_write.is_empty() {
            assert!(Instant::now() < deadline, "timed out writing to pty");
            let n = spawned.master.write_nonblocking(to_write).expect("write");
            to_write = &to_write[n..];
            if !to_write.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        loop {
            assert!(Instant::now() < deadline, "timed out reading pty output: {:?}", String::from_utf8_lossy(&output));
            match spawned.master.read_nonblocking(&mut buf).expect("read") {
                NonBlockingRead::Bytes(n) => output.extend_from_slice(&buf[..n]),
                NonBlockingRead::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                NonBlockingRead::Eof => break,
            }
        }
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("HI:xterm-256color:ok"), "unexpected output: {text:?}");
        assert!(text.contains("GOT:ping"), "unexpected output: {text:?}");

        let status = spawned.child.wait().expect("wait");
        assert!(status.success());
    }
}
