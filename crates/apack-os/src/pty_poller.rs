//! POSIX non-blocking fd multiplexer (`PtyPoller`) for sharded PTY and socket I/O.
//!
//! - On **Linux**, [`PtyPoller`] aliases [`crate::pty_poller_epoll::EpollPoller`] (`epoll` +
//!   `eventfd`).
//! - On **macOS and other Unix**, [`PtyPoller`] aliases [`PollPoller`] (`poll(2)` + non-blocking
//!   self-pipe). We **deliberately avoid `kqueue` on `/dev/ptmx`** because macOS/BSD `kqueue`
//!   exhibits `EINVAL` and spurious `EV_EOF` quirks on PTY masters.
//!
//! [`PollPoller`] is unconditionally compiled and tested on **all** Unix targets (including Linux)
//! so both backends are verified and mutation-tested in CI.

#![cfg(unix)]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::pty::{NonBlockingRead, read_fd_nonblocking, set_fd_nonblocking};

/// Platform-selected sharded non-blocking poller (`epoll` on Linux, `poll(2)` on macOS/BSD).
#[cfg(target_os = "linux")]
pub type PtyPoller = crate::pty_poller_epoll::EpollPoller;

/// Platform-selected sharded non-blocking poller (`epoll` on Linux, `poll(2)` on macOS/BSD).
#[cfg(not(target_os = "linux"))]
pub type PtyPoller = PollPoller;

/// Platform-selected cross-thread poller waker.
#[cfg(target_os = "linux")]
pub type PtyWaker = crate::pty_poller_epoll::EpollWaker;

/// Platform-selected cross-thread poller waker.
#[cfg(not(target_os = "linux"))]
pub type PtyWaker = PollWaker;

/// Opaque registration identifier returned in [`PollEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PollToken(pub u64);

/// Readiness interest bitmask for a registered file descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interest {
    /// Monitor for readable data or EOF.
    pub readable: bool,
    /// Monitor for writable buffer space.
    pub writable: bool,
}

impl Interest {
    /// Readable-only interest.
    pub const READ: Self = Self { readable: true, writable: false };
    /// Writable-only interest.
    pub const WRITE: Self = Self { readable: false, writable: true };
    /// Both readable and writable interest.
    pub const READ_WRITE: Self = Self { readable: true, writable: true };
    /// No readiness interest (used while a session is paused with no pending writes).
    pub const NONE: Self = Self { readable: false, writable: false };
}

/// One readiness notification returned by `poll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollEvent {
    /// Token associated with the ready descriptor.
    pub token: PollToken,
    /// Descriptor is readable (or has error/EOF data to observe via `read`).
    pub readable: bool,
    /// Descriptor is writable.
    pub writable: bool,
    /// Peer/slave hung up or reported a descriptor error (`POLLHUP` / `POLLERR` / `POLLNVAL`).
    pub hangup: bool,
}

/// Cross-thread waker backed by a non-blocking Unix self-pipe for [`PollPoller`].
#[derive(Clone)]
pub struct PollWaker {
    write_end: Arc<OwnedFd>,
    pending: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeErrAction {
    Retry,
    AlreadyQueued,
    Fail,
}

pub(crate) fn classify_wake_error(err: &io::Error) -> WakeErrAction {
    if err.kind() == io::ErrorKind::Interrupted {
        return WakeErrAction::Retry;
    }
    if err.kind() == io::ErrorKind::WouldBlock {
        return WakeErrAction::AlreadyQueued;
    }
    WakeErrAction::Fail
}

impl PollWaker {
    /// Wakes a sleeping [`PollPoller::poll`] call promptly (coalescing redundant writes).
    pub fn wake(&self) -> io::Result<()> {
        if self.pending.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let byte = [1u8; 1];
        loop {
            // SAFETY: `self.write_end` is an open non-blocking pipe write fd owned by `Arc`.
            let rc = unsafe { libc::write(self.write_end.as_raw_fd(), byte.as_ptr().cast(), 1) };
            if rc != -1 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            match classify_wake_error(&err) {
                WakeErrAction::Retry => continue,
                WakeErrAction::AlreadyQueued => return Ok(()),
                WakeErrAction::Fail => {
                    self.pending.store(false, Ordering::SeqCst);
                    return Err(err);
                }
            }
        }
    }
}

/// Portable POSIX `poll(2)` multiplexer with a coalescing non-blocking self-pipe waker.
pub struct PollPoller {
    wake_read: OwnedFd,
    wake_write: Arc<OwnedFd>,
    wake_pending: Arc<AtomicBool>,
    entries: HashMap<PollToken, (RawFd, Interest)>,
    scratch_fds: Vec<libc::pollfd>,
    scratch_tokens: Vec<PollToken>,
}

pub(crate) fn is_interrupted(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
}

impl PollPoller {
    /// Creates a new `poll(2)` multiplexer and its internal `O_NONBLOCK | O_CLOEXEC` wake pipe.
    pub fn new() -> io::Result<Self> {
        let (wake_read, wake_write) = create_nonblocking_pipe()?;
        Ok(Self {
            wake_read,
            wake_write: Arc::new(wake_write),
            wake_pending: Arc::new(AtomicBool::new(false)),
            entries: HashMap::new(),
            scratch_fds: Vec::new(),
            scratch_tokens: Vec::new(),
        })
    }

    /// Returns a cloneable thread-safe waker for this poller.
    pub fn waker(&self) -> PollWaker {
        PollWaker { write_end: Arc::clone(&self.wake_write), pending: Arc::clone(&self.wake_pending) }
    }

    /// Registers `fd` under `token` with initial `interest`.
    pub fn register(&mut self, token: PollToken, fd: RawFd, interest: Interest) -> io::Result<()> {
        if self.entries.contains_key(&token) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "duplicate poll token"));
        }
        self.entries.insert(token, (fd, interest));
        Ok(())
    }

    /// Updates readiness `interest` for an already-registered `token`.
    pub fn modify(&mut self, token: PollToken, interest: Interest) -> io::Result<()> {
        let entry = self
            .entries
            .get_mut(&token)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown poll token"))?;
        entry.1 = interest;
        Ok(())
    }

    /// Removes `token` from the poller.
    pub fn deregister(&mut self, token: PollToken) -> io::Result<()> {
        self.entries
            .remove(&token)
            .map(|_| ())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown poll token"))
    }

    /// Returns the number of currently registered descriptors (excluding the internal waker).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no descriptors are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Waits up to `timeout` (or indefinitely if `None`) for readiness events, appending ready
    /// descriptors to `out`. Returns `true` if woken by [`PollWaker::wake`].
    pub fn poll(&mut self, out: &mut Vec<PollEvent>, timeout: Option<Duration>) -> io::Result<bool> {
        out.clear();
        self.scratch_fds.clear();
        self.scratch_tokens.clear();

        // Slot 0 is always the internal wake pipe.
        self.scratch_fds.push(libc::pollfd { fd: self.wake_read.as_raw_fd(), events: libc::POLLIN, revents: 0 });

        for (&token, &(fd, interest)) in &self.entries {
            let mut events: libc::c_short = 0;
            if interest.readable {
                events |= libc::POLLIN;
            }
            if interest.writable {
                events |= libc::POLLOUT;
            }
            if events == 0 {
                continue;
            }
            self.scratch_fds.push(libc::pollfd { fd, events, revents: 0 });
            self.scratch_tokens.push(token);
        }

        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        loop {
            let remaining = deadline.map(|dl| dl.saturating_duration_since(std::time::Instant::now()));
            let timeout_ms = duration_to_poll_timeout_ms(remaining);
            // SAFETY: `scratch_fds` is a contiguous initialized array of `libc::pollfd` with
            // length `scratch_fds.len()`.
            let rc = unsafe {
                libc::poll(self.scratch_fds.as_mut_ptr(), self.scratch_fds.len() as libc::nfds_t, timeout_ms)
            };
            if rc >= 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if is_interrupted(&err) {
                continue;
            }
            return Err(err);
        }

        let woken = (self.scratch_fds[0].revents & libc::POLLIN) != 0;
        if woken {
            self.drain_wake_pipe();
        }

        for (pfd, &token) in self.scratch_fds[1..].iter().zip(self.scratch_tokens.iter()) {
            if let Some(ev) = decode_poll_revents(token, pfd.revents) {
                out.push(ev);
            }
        }

        Ok(woken)
    }

    fn drain_wake_pipe(&self) {
        // Drain the pipe completely BEFORE clearing `wake_pending` so a concurrent `wake()` cannot
        // have its byte drained while leaving `wake_pending == true` (which would cause a permanent
        // lost-wakeup deadlock).
        let mut drain = [0u8; 64];
        while matches!(read_fd_nonblocking(self.wake_read.as_raw_fd(), &mut drain), Ok(NonBlockingRead::Bytes(_))) {}
        self.wake_pending.store(false, Ordering::SeqCst);
    }
}

fn decode_poll_revents(token: PollToken, rev: libc::c_short) -> Option<PollEvent> {
    if rev == 0 {
        return None;
    }
    let hangup = (rev & libc::POLLHUP) != 0 || (rev & libc::POLLERR) != 0 || (rev & libc::POLLNVAL) != 0;
    let readable = (rev & libc::POLLIN) != 0 || hangup;
    let writable = (rev & libc::POLLOUT) != 0;
    Some(PollEvent { token, readable, writable, hangup })
}

/// Converts an optional [`Duration`] into a `poll(2)` / `epoll_wait(2)` millisecond timeout.
///
/// Rounds sub-millisecond non-zero durations up to `1ms` so timers never busy-spin.
pub fn duration_to_poll_timeout_ms(timeout: Option<Duration>) -> libc::c_int {
    let Some(dur) = timeout else {
        return -1;
    };
    if dur.is_zero() {
        return 0;
    }
    let millis = dur.as_millis();
    let clamped = if millis == 0 { 1 } else { millis.min(libc::c_int::MAX as u128) };
    clamped as libc::c_int
}

/// Creates a `O_NONBLOCK | FD_CLOEXEC` Unix pipe pair `(read_end, write_end)`.
///
/// Uses atomic `pipe2(O_NONBLOCK | O_CLOEXEC)` on Linux and portable `pipe` + `fcntl` elsewhere.
pub(crate) fn create_nonblocking_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(target_os = "linux")]
    {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a valid 2-element array for `pipe2(2)` to populate atomically with
        // `O_NONBLOCK | O_CLOEXEC`.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `pipe2(2)` succeeded and returned two freshly opened exclusive descriptors.
        let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // SAFETY: `pipe2(2)` succeeded and returned two freshly opened exclusive descriptors.
        let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Ok((read_end, write_end))
    }
    #[cfg(not(target_os = "linux"))]
    {
        create_nonblocking_pipe_posix()
    }
}

/// Portable POSIX `pipe` + `FD_CLOEXEC` + `O_NONBLOCK` helper (compiled and tested on all Unix).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn create_nonblocking_pipe_posix() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid 2-element array for `pipe(2)` to populate.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe(2)` succeeded and returned two freshly opened exclusive descriptors.
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    // SAFETY: `pipe(2)` succeeded and returned two freshly opened exclusive descriptors.
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    for fd in [read_end.as_raw_fd(), write_end.as_raw_fd()] {
        // SAFETY: setting `FD_CLOEXEC` on freshly created owned pipe fds.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        set_fd_nonblocking(fd, true)?;
    }
    Ok((read_end, write_end))
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;

    /// Shared conformance suite run against BOTH [`PollPoller`] (all Unix) and
    /// `EpollPoller` (Linux).
    pub fn assert_timeout_conversion() {
        assert_eq!(duration_to_poll_timeout_ms(None), -1);
        assert_eq!(duration_to_poll_timeout_ms(Some(Duration::ZERO)), 0);
        assert_eq!(duration_to_poll_timeout_ms(Some(Duration::from_micros(50))), 1);
        assert_eq!(duration_to_poll_timeout_ms(Some(Duration::from_millis(42))), 42);
        assert_eq!(duration_to_poll_timeout_ms(Some(Duration::MAX)), libc::c_int::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{NonBlockingRead, read_fd_nonblocking, write_fd_nonblocking};

    #[test]
    fn timeout_conversion_and_revents_and_wake_classifier() {
        test_helpers::assert_timeout_conversion();

        assert_eq!(classify_wake_error(&io::Error::new(io::ErrorKind::Interrupted, "")), WakeErrAction::Retry);
        assert_eq!(classify_wake_error(&io::Error::new(io::ErrorKind::WouldBlock, "")), WakeErrAction::AlreadyQueued);
        assert_eq!(classify_wake_error(&io::Error::new(io::ErrorKind::BrokenPipe, "")), WakeErrAction::Fail);
        assert!(is_interrupted(&io::Error::new(io::ErrorKind::Interrupted, "")));
        assert!(!is_interrupted(&io::Error::new(io::ErrorKind::BrokenPipe, "")));

        let tok = PollToken(7);
        assert_eq!(decode_poll_revents(tok, 0), None);
        assert_eq!(
            decode_poll_revents(tok, libc::POLLIN),
            Some(PollEvent { token: tok, readable: true, writable: false, hangup: false })
        );
        assert_eq!(
            decode_poll_revents(tok, libc::POLLOUT),
            Some(PollEvent { token: tok, readable: false, writable: true, hangup: false })
        );
        assert_eq!(
            decode_poll_revents(tok, libc::POLLHUP),
            Some(PollEvent { token: tok, readable: true, writable: false, hangup: true })
        );
        assert_eq!(
            decode_poll_revents(tok, libc::POLLERR),
            Some(PollEvent { token: tok, readable: true, writable: false, hangup: true })
        );
        assert_eq!(
            decode_poll_revents(tok, libc::POLLNVAL),
            Some(PollEvent { token: tok, readable: true, writable: false, hangup: true })
        );

        // Verify both `create_nonblocking_pipe` and `create_nonblocking_pipe_posix` set CLOEXEC + NONBLOCK.
        for (r, w) in [create_nonblocking_pipe().expect("pipe"), create_nonblocking_pipe_posix().expect("posix pipe")] {
            for fd in [r.as_raw_fd(), w.as_raw_fd()] {
                // SAFETY: `F_GETFD` on open pipe fd.
                assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
                // SAFETY: `F_GETFL` on open pipe fd.
                assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_NONBLOCK, 0);
            }
        }

        // Multi-chunk wake pipe drain clears > 64 bytes in a single poll call.
        let mut poller = PollPoller::new().expect("poller");
        let bulk = [1u8; 100];
        assert_eq!(write_fd_nonblocking(poller.wake_write.as_raw_fd(), &bulk).expect("write bulk"), 100);
        let mut events = Vec::new();
        assert!(poller.poll(&mut events, Some(Duration::from_millis(50))).expect("drain bulk"));
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("fully drained"));
    }

    #[test]
    fn poll_poller_multiplexes_pipes_pauses_resumes_wakes_and_detects_hangup() {
        let mut poller = PollPoller::new().expect("create poller");
        assert!(poller.is_empty());
        assert_eq!(poller.len(), 0);

        let (r1, w1) = create_nonblocking_pipe().expect("pipe 1");
        let (r2, w2) = create_nonblocking_pipe().expect("pipe 2");
        let t1 = PollToken(1);
        let t2 = PollToken(2);

        poller.register(t1, r1.as_raw_fd(), Interest::READ).expect("reg t1");
        // Duplicate token rejected.
        assert_eq!(
            poller.register(t1, r1.as_raw_fd(), Interest::READ).expect_err("dup").kind(),
            io::ErrorKind::AlreadyExists
        );
        poller.register(t2, r2.as_raw_fd(), Interest::NONE).expect("reg t2 paused");
        assert_eq!(poller.len(), 2);
        assert!(!poller.is_empty());

        // Unknown token modify/deregister rejected.
        assert_eq!(
            poller.modify(PollToken(99), Interest::READ).expect_err("not found").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(poller.deregister(PollToken(99)).expect_err("not found").kind(), io::ErrorKind::NotFound);

        // Idle timeout returns false with empty events.
        let mut events = Vec::new();
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("idle poll"));
        assert!(events.is_empty());

        // Write to both pipes; only t1 has READ interest (t2 is paused with Interest::NONE).
        assert_eq!(write_fd_nonblocking(w1.as_raw_fd(), b"one").expect("w1"), 3);
        assert_eq!(write_fd_nonblocking(w2.as_raw_fd(), b"two").expect("w2"), 3);
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("poll t1"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token, t1);
        assert!(events[0].readable);
        assert!(!events[0].writable);

        let mut buf = [0u8; 16];
        assert_eq!(read_fd_nonblocking(r1.as_raw_fd(), &mut buf).expect("r1"), NonBlockingRead::Bytes(3));
        assert_eq!(&buf[..3], b"one");

        // Unpause t2 and verify it immediately reports readable.
        poller.modify(t2, Interest::READ).expect("unpause t2");
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("poll t2"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token, t2);
        assert!(events[0].readable);
        assert_eq!(read_fd_nonblocking(r2.as_raw_fd(), &mut buf).expect("r2"), NonBlockingRead::Bytes(3));
        assert_eq!(&buf[..3], b"two");

        // Verify WRITE interest on w1.
        let tw = PollToken(3);
        poller.register(tw, w1.as_raw_fd(), Interest::WRITE).expect("reg w1");
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("poll w1"));
        assert!(events.iter().any(|e| e.token == tw && e.writable));
        poller.deregister(tw).expect("dereg w1");

        // Synchronous double-wake coalesces to one wake and leaves subsequent poll clean.
        let waker = poller.waker();
        waker.wake().expect("coalesced wake 1");
        waker.wake().expect("coalesced wake 2");
        assert!(poller.poll(&mut events, Some(Duration::from_secs(2))).expect("coalesced woken poll"));
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("clean after coalesced wake"));

        // Cross-thread single wake unblocks a blocking poll cleanly.
        let wake_th = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake().expect("cross-thread wake");
        });
        assert!(poller.poll(&mut events, Some(Duration::from_secs(2))).expect("woken poll"));
        wake_th.join().expect("join wake");
        // Subsequent poll must not observe a stale wake byte.
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("clean after wake"));

        // Dropping w1 causes hangup / EOF on t1.
        drop(w1);
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("hangup poll"));
        assert!(events.iter().any(|e| e.token == t1 && e.readable && e.hangup));
        assert_eq!(read_fd_nonblocking(r1.as_raw_fd(), &mut buf).expect("eof"), NonBlockingRead::Eof);

        poller.deregister(t1).expect("dereg t1");
        poller.deregister(t2).expect("dereg t2");
        assert!(poller.is_empty());
    }
}
