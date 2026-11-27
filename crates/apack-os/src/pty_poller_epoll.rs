//! Linux `epoll` + `eventfd` non-blocking fd multiplexer for sharded PTY and socket I/O.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::pty_poller::{
    Interest, PollEvent, PollToken, WakeErrAction, classify_wake_error, duration_to_poll_timeout_ms, is_interrupted,
};

/// Reserved `epoll_event.u64` value identifying the internal `eventfd` waker.
const WAKE_SENTINEL: u64 = u64::MAX;

/// Maximum events harvested per `epoll_wait(2)` syscall.
const EPOLL_BATCH: usize = 64;

/// Cross-thread waker backed by a non-blocking Linux `eventfd` for [`EpollPoller`].
#[derive(Clone)]
pub struct EpollWaker {
    event_fd: Arc<OwnedFd>,
    pending: Arc<AtomicBool>,
}

impl EpollWaker {
    /// Wakes a sleeping [`EpollPoller::poll`] call promptly (coalescing redundant increments).
    pub fn wake(&self) -> io::Result<()> {
        if self.pending.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let val: u64 = 1;
        loop {
            // SAFETY: `self.event_fd` is an open non-blocking `eventfd` owned by `Arc`, and
            // `val` is a valid 8-byte `u64` counter increment.
            let rc = unsafe {
                libc::write(self.event_fd.as_raw_fd(), std::ptr::from_ref(&val).cast(), std::mem::size_of::<u64>())
            };
            if rc == std::mem::size_of::<u64>() as isize {
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

/// Linux `epoll` multiplexer with a coalescing `eventfd` waker.
///
/// Entries registered or modified to [`Interest::NONE`] are removed from the kernel `epoll`
/// interest set so paused sessions never busy-spin on unconditional `EPOLLHUP`/`EPOLLERR`
/// notifications (matching [`crate::pty_poller::PollPoller`] semantics).
pub struct EpollPoller {
    epoll_fd: OwnedFd,
    event_fd: Arc<OwnedFd>,
    wake_pending: Arc<AtomicBool>,
    entries: HashMap<PollToken, (RawFd, Interest)>,
    scratch: Vec<libc::epoll_event>,
}

impl EpollPoller {
    /// Creates a new `epoll` instance (`EPOLL_CLOEXEC`) and registers its internal `eventfd`.
    pub fn new() -> io::Result<Self> {
        // SAFETY: `epoll_create1(EPOLL_CLOEXEC)` returns a fresh fd or `-1` on error.
        let raw_ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw_ep == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw_ep != -1` is exclusively owned by this function.
        let epoll_fd = unsafe { OwnedFd::from_raw_fd(raw_ep) };

        // SAFETY: `eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC)` returns a fresh fd or `-1`.
        let raw_ev = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw_ev == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw_ev != -1` is exclusively owned by this function.
        let event_fd = Arc::new(unsafe { OwnedFd::from_raw_fd(raw_ev) });

        let mut ev = libc::epoll_event { events: libc::EPOLLIN as u32, u64: WAKE_SENTINEL };
        // SAFETY: Both `epoll_fd` and `event_fd` are open valid descriptors.
        if unsafe { libc::epoll_ctl(epoll_fd.as_raw_fd(), libc::EPOLL_CTL_ADD, event_fd.as_raw_fd(), &mut ev) } != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            epoll_fd,
            event_fd,
            wake_pending: Arc::new(AtomicBool::new(false)),
            entries: HashMap::new(),
            scratch: vec![libc::epoll_event { events: 0, u64: 0 }; EPOLL_BATCH],
        })
    }

    /// Returns a cloneable thread-safe waker for this `epoll` instance.
    pub fn waker(&self) -> EpollWaker {
        EpollWaker { event_fd: Arc::clone(&self.event_fd), pending: Arc::clone(&self.wake_pending) }
    }

    /// Registers `fd` under `token` with initial `interest`.
    ///
    /// Callers must call [`Self::deregister`] before closing the underlying descriptor.
    pub fn register(&mut self, token: PollToken, fd: BorrowedFd<'_>, interest: Interest) -> io::Result<()> {
        let fd = fd.as_raw_fd();
        if token.0 == WAKE_SENTINEL {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "reserved poll token"));
        }
        if self.entries.contains_key(&token) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "duplicate poll token"));
        }
        if self.entries.values().any(|&(existing_fd, _)| existing_fd == fd) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "duplicate poll file descriptor"));
        }
        let mask = interest_to_epoll(interest);
        if mask != 0 {
            let mut ev = libc::epoll_event { events: mask, u64: token.0 };
            // SAFETY: `self.epoll_fd` is valid and `fd` is supplied by caller.
            if unsafe { libc::epoll_ctl(self.epoll_fd.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        self.entries.insert(token, (fd, interest));
        Ok(())
    }

    /// Updates readiness `interest` for an already-registered `token`.
    pub fn modify(&mut self, token: PollToken, interest: Interest) -> io::Result<()> {
        let epoll_fd = self.epoll_fd.as_raw_fd();
        let entry = self
            .entries
            .get_mut(&token)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown poll token"))?;
        let was_armed = interest_to_epoll(entry.1) != 0;
        let new_mask = interest_to_epoll(interest);
        let now_armed = new_mask != 0;
        let fd = entry.0;
        match (was_armed, now_armed) {
            (false, false) => {}
            (false, true) => {
                let mut ev = libc::epoll_event { events: new_mask, u64: token.0 };
                // SAFETY: `epoll_fd` is valid and `fd` is currently unregistered in epoll.
                if unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            (true, false) => {
                epoll_del_ignore_closed(epoll_fd, fd)?;
            }
            (true, true) => {
                let mut ev = libc::epoll_event { events: new_mask, u64: token.0 };
                // SAFETY: `epoll_fd` and `fd` are currently registered descriptors.
                if unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_MOD, fd, &mut ev) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        entry.1 = interest;
        Ok(())
    }

    /// Removes `token` from the `epoll` interest set.
    pub fn deregister(&mut self, token: PollToken) -> io::Result<()> {
        let (fd, interest) =
            self.entries.remove(&token).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown poll token"))?;
        if interest_to_epoll(interest) != 0 {
            epoll_del_ignore_closed(self.epoll_fd.as_raw_fd(), fd)?;
        }
        Ok(())
    }

    /// Returns the number of registered descriptors (excluding the internal `eventfd`).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when no descriptors are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Waits up to `timeout` (or indefinitely if `None`) for readiness events, appending ready
    /// descriptors to `out`. Returns `true` if woken by [`EpollWaker::wake`].
    pub fn poll(&mut self, out: &mut Vec<PollEvent>, timeout: Option<Duration>) -> io::Result<bool> {
        out.clear();
        let deadline = timeout.map(|d| std::time::Instant::now() + d);
        let ready = loop {
            let remaining = deadline.map(|dl| dl.saturating_duration_since(std::time::Instant::now()));
            let timeout_ms = duration_to_poll_timeout_ms(remaining);
            // SAFETY: `self.scratch` is an initialized buffer of `EPOLL_BATCH` `epoll_event`s.
            let rc = unsafe {
                libc::epoll_wait(
                    self.epoll_fd.as_raw_fd(),
                    self.scratch.as_mut_ptr(),
                    self.scratch.len() as libc::c_int,
                    timeout_ms,
                )
            };
            if rc != -1 {
                break rc as usize;
            }
            let err = io::Error::last_os_error();
            if is_interrupted(&err) {
                continue;
            }
            return Err(err);
        };

        let mut woken = false;
        for ev in &self.scratch[..ready] {
            if ev.u64 == WAKE_SENTINEL {
                woken = true;
                self.drain_event_fd();
                continue;
            }
            out.push(decode_epoll_event(PollToken(ev.u64), ev.events));
        }
        Ok(woken)
    }

    fn drain_event_fd(&self) {
        // Drain `eventfd` completely (retrying `EINTR` via `read_fd_nonblocking`) BEFORE clearing
        // `wake_pending` so a concurrent `wake()` can never be lost while leaving `wake_pending == true`.
        let mut buf = [0u8; std::mem::size_of::<u64>()];
        let _ = crate::pty::read_fd_nonblocking(self.event_fd.as_fd(), &mut buf);
        self.wake_pending.store(false, Ordering::SeqCst);
    }
}

fn epoll_del_ignore_closed(epoll_fd: RawFd, fd: RawFd) -> io::Result<()> {
    // SAFETY: `epoll_fd` is valid; `EPOLL_CTL_DEL` ignores the event pointer on Linux >= 2.6.9.
    let rc = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if !matches!(err.raw_os_error(), Some(libc::ENOENT | libc::EBADF)) {
            return Err(err);
        }
    }
    Ok(())
}

fn decode_epoll_event(token: PollToken, mask: u32) -> PollEvent {
    let hangup = (mask & (libc::EPOLLHUP as u32)) != 0 || (mask & (libc::EPOLLERR as u32)) != 0;
    let readable = (mask & (libc::EPOLLIN as u32)) != 0 || hangup;
    let writable = (mask & (libc::EPOLLOUT as u32)) != 0;
    PollEvent { token, readable, writable, hangup }
}

fn interest_to_epoll(interest: Interest) -> u32 {
    let mut mask: u32 = 0;
    if interest.readable {
        mask |= libc::EPOLLIN as u32;
    }
    if interest.writable {
        mask |= libc::EPOLLOUT as u32;
    }
    mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::{NonBlockingRead, read_fd_nonblocking, write_fd_nonblocking};
    use crate::pty_poller::create_nonblocking_pipe;

    #[test]
    fn decode_epoll_event_covers_all_flags() {
        let tok = PollToken(5);
        assert_eq!(
            decode_epoll_event(tok, libc::EPOLLIN as u32),
            PollEvent { token: tok, readable: true, writable: false, hangup: false }
        );
        assert_eq!(
            decode_epoll_event(tok, libc::EPOLLOUT as u32),
            PollEvent { token: tok, readable: false, writable: true, hangup: false }
        );
        assert_eq!(
            decode_epoll_event(tok, libc::EPOLLHUP as u32),
            PollEvent { token: tok, readable: true, writable: false, hangup: true }
        );
        assert_eq!(
            decode_epoll_event(tok, libc::EPOLLERR as u32),
            PollEvent { token: tok, readable: true, writable: false, hangup: true }
        );
    }

    #[test]
    fn epoll_poller_multiplexes_pipes_pauses_without_hup_spin_wakes_and_handles_closed_deregister() {
        let mut poller = EpollPoller::new().expect("create epoll");
        assert!(poller.is_empty());
        assert_eq!(poller.len(), 0);

        let (r1, w1) = create_nonblocking_pipe().expect("pipe 1");
        let (r2, w2) = create_nonblocking_pipe().expect("pipe 2");
        let (r3, w3) = create_nonblocking_pipe().expect("pipe 3");
        let t1 = PollToken(1);
        let t2 = PollToken(2);
        let t3 = PollToken(3);

        // Sentinel token rejected.
        assert_eq!(
            poller.register(PollToken(WAKE_SENTINEL), r1.as_fd(), Interest::READ).expect_err("sentinel").kind(),
            io::ErrorKind::InvalidInput
        );

        poller.register(t1, r1.as_fd(), Interest::READ).expect("reg t1");
        assert_eq!(
            poller.register(t1, r1.as_fd(), Interest::READ).expect_err("dup").kind(),
            io::ErrorKind::AlreadyExists
        );
        poller.register(t2, r2.as_fd(), Interest::NONE).expect("reg t2 paused");
        poller.register(t3, r3.as_fd(), Interest::READ).expect("reg t3");
        assert_eq!(poller.len(), 3);
        assert!(!poller.is_empty());

        // Pausing t3 (`READ -> NONE`) and closing w3 must NOT spin on `EPOLLHUP` while paused.
        poller.modify(t3, Interest::NONE).expect("pause t3");
        poller.modify(t3, Interest::NONE).expect("re-pause t3 noop");
        drop(w3);

        assert_eq!(
            poller.modify(PollToken(99), Interest::READ).expect_err("not found").kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(poller.deregister(PollToken(99)).expect_err("not found").kind(), io::ErrorKind::NotFound);

        let mut events = Vec::new();
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("idle"));
        assert!(events.is_empty(), "paused hung-up fd t3 must not fire EPOLLHUP: {events:?}");

        // Resuming t3 surfaces the pending hangup cleanly, then deregister before dropping `r3`.
        poller.modify(t3, Interest::READ).expect("resume t3");
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("t3 hangup"));
        assert!(events.iter().any(|e| e.token == t3 && e.hangup));
        poller.deregister(t3).expect("dereg t3");
        // Deleting an open fd that is not in `epoll_fd` returns `ENOENT`, which `epoll_del_ignore_closed`
        // suppresses without any cross-thread fd-number reuse race.
        epoll_del_ignore_closed(poller.epoll_fd.as_raw_fd(), r3.as_raw_fd()).expect("ignore ENOENT on unregistered fd");
        drop(r3);

        assert_eq!(write_fd_nonblocking(w1.as_fd(), b"one").expect("w1"), 3);
        assert_eq!(write_fd_nonblocking(w2.as_fd(), b"two").expect("w2"), 3);
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("poll t1"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token, t1);
        assert!(events[0].readable);
        assert!(!events[0].writable);

        let mut buf = [0u8; 16];
        assert_eq!(read_fd_nonblocking(r1.as_fd(), &mut buf).expect("r1"), NonBlockingRead::Bytes(3));
        assert_eq!(&buf[..3], b"one");

        poller.modify(t2, Interest::READ).expect("unpause t2");
        poller.modify(t2, Interest::READ_WRITE).expect("active -> active mod");
        poller.modify(t2, Interest::READ).expect("back to read");
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("poll t2"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token, t2);
        assert!(events[0].readable);
        assert_eq!(read_fd_nonblocking(r2.as_fd(), &mut buf).expect("r2"), NonBlockingRead::Bytes(3));
        assert_eq!(&buf[..3], b"two");

        let tw = PollToken(4);
        poller.register(tw, w1.as_fd(), Interest::WRITE).expect("reg w1");
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("poll w1"));
        assert!(events.iter().any(|e| e.token == tw && e.writable));
        poller.deregister(tw).expect("dereg w1");

        // Synchronous double-wake coalesces to one wake and leaves subsequent poll clean.
        let waker = poller.waker();
        waker.wake().expect("coalesced wake 1");
        waker.wake().expect("coalesced wake 2");
        assert!(poller.poll(&mut events, Some(Duration::from_secs(2))).expect("coalesced woken"));
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("clean after coalesced"));

        let wake_th = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake().expect("cross-thread wake");
        });
        assert!(poller.poll(&mut events, Some(Duration::from_secs(2))).expect("woken"));
        wake_th.join().expect("join");
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(10))).expect("clean"));

        drop(w1);
        assert!(!poller.poll(&mut events, Some(Duration::from_millis(50))).expect("hangup"));
        assert!(events.iter().any(|e| e.token == t1 && e.readable && e.hangup));
        assert_eq!(read_fd_nonblocking(r1.as_fd(), &mut buf).expect("eof"), NonBlockingRead::Eof);

        poller.deregister(t1).expect("dereg t1");
        poller.modify(t2, Interest::NONE).expect("pause t2 before dereg");
        poller.deregister(t2).expect("dereg paused t2");
        assert!(poller.is_empty());
    }
}
