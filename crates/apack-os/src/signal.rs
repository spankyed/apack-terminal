//! Safe POSIX process signaling, sealed [`KillTarget`], ancestry checks, and `strsignal` inversion.
//!
//! Safety invariant: `kill(0, sig)` (broker's own process group) and `kill(-1, sig)` (every
//! process the user owns) are unrepresentable in [`KillTarget`]. Its inner field is private and it
//! implements no `Default`, so neither caller input nor mutation testing can construct a broadcast
//! target.

use std::time::Duration;

use apack_proto::types::{SignalKind, SignalScope};

use crate::OsError;

/// Grace period between polite `SIGHUP` and forced `SIGKILL` escalation during session teardown.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// Maximum parent-chain hops walked by [`inside_tree`] to prevent infinite loops on reparenting.
pub const MAX_TREE_DEPTH: usize = 64;

/// A validated `kill(2)` target: either a single positive PID (`>= 2`) or a negative process group
/// (`<= -2`), never `0` (broker's own group), `-1` (every user process), or `i32::MIN`.
///
/// Private field and no `Default` are load-bearing: mutation testing cannot synthesize a valid
/// `KillTarget` via `Default::default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KillTarget(i32);

impl KillTarget {
    /// Constructs a [`KillTarget`], refusing broadcast targets (`0`, `-1`) and `i32::MIN`.
    pub fn at(target: i32) -> Result<Self, OsError> {
        if target == 0 {
            return Err(OsError::InvalidTarget("pid 0 targets every process in this broker's own group".to_owned()));
        }
        if target == -1 {
            return Err(OsError::InvalidTarget("pid -1 targets every process the user owns".to_owned()));
        }
        if target == i32::MIN {
            return Err(OsError::InvalidTarget("i32::MIN is not a valid signal target".to_owned()));
        }
        Ok(Self(target))
    }

    /// Returns the raw validated `kill(2)` target number.
    ///
    /// Note: [`deliver_signal`] reads `target.0` directly rather than going through this accessor
    /// so mutating `number()` cannot redirect a live syscall.
    pub fn number(self) -> i32 {
        self.0
    }
}

/// Computes the validated [`KillTarget`] for `(leader, scope)`.
///
/// Refuses PIDs below `2` (`0` = unknown/self-group, `1` = init/broadcast-group `-1`) and PIDs
/// that do not fit in `i32`.
pub fn kill_target(leader: u32, scope: SignalScope) -> Result<KillTarget, OsError> {
    let real = |pid: u32| -> Result<i32, OsError> {
        if pid < 2 {
            return Err(OsError::InvalidTarget(format!(
                "pid {pid} is not a signalable session process (unknown or init)"
            )));
        }
        i32::try_from(pid)
            .map_err(|_| OsError::InvalidTarget(format!("pid {pid} exceeds i32::MAX and cannot be signaled")))
    };
    match scope {
        SignalScope::Leader => KillTarget::at(real(leader)?),
        SignalScope::Group => KillTarget::at(-real(leader)?),
        SignalScope::Pid(target) => {
            let _ = real(leader)?;
            KillTarget::at(real(target.pid() as u32)?)
        }
    }
}

/// Converts a protocol [`SignalKind`] into its POSIX `libc` signal number.
#[cfg(unix)]
pub fn signal_number(kind: SignalKind) -> libc::c_int {
    match kind {
        SignalKind::Hup => libc::SIGHUP,
        SignalKind::Int => libc::SIGINT,
        SignalKind::Quit => libc::SIGQUIT,
        SignalKind::Term => libc::SIGTERM,
        SignalKind::Kill => libc::SIGKILL,
        SignalKind::Usr1 => libc::SIGUSR1,
        SignalKind::Usr2 => libc::SIGUSR2,
        SignalKind::Stop => libc::SIGSTOP,
        SignalKind::Cont => libc::SIGCONT,
    }
}

/// Signals recognized when inverting `portable-pty`'s `strsignal` exit descriptions.
#[cfg(unix)]
const NAMED_SIGNALS: &[(&str, libc::c_int)] = &[
    ("SIGHUP", libc::SIGHUP),
    ("SIGINT", libc::SIGINT),
    ("SIGQUIT", libc::SIGQUIT),
    ("SIGILL", libc::SIGILL),
    ("SIGABRT", libc::SIGABRT),
    ("SIGFPE", libc::SIGFPE),
    ("SIGKILL", libc::SIGKILL),
    ("SIGBUS", libc::SIGBUS),
    ("SIGSEGV", libc::SIGSEGV),
    ("SIGPIPE", libc::SIGPIPE),
    ("SIGALRM", libc::SIGALRM),
    ("SIGTERM", libc::SIGTERM),
    ("SIGUSR1", libc::SIGUSR1),
    ("SIGUSR2", libc::SIGUSR2),
    ("SIGSTOP", libc::SIGSTOP),
    ("SIGCONT", libc::SIGCONT),
];

#[cfg(unix)]
static NAMED_DESCRIPTIONS: std::sync::OnceLock<Vec<(&'static str, String)>> = std::sync::OnceLock::new();

/// Eagerly populates the `strsignal` lookup table while the broker is still single-threaded,
/// because POSIX does not require `strsignal(3)` to be thread-safe.
pub fn init_signal_descriptions() {
    #[cfg(unix)]
    {
        let _ = descriptions();
    }
}

#[cfg(unix)]
fn descriptions() -> &'static [(&'static str, String)] {
    NAMED_DESCRIPTIONS.get_or_init(|| {
        NAMED_SIGNALS.iter().filter_map(|&(name, number)| described(number).map(|prose| (name, prose))).collect()
    })
}

#[cfg(unix)]
fn described(number: libc::c_int) -> Option<String> {
    // SAFETY: `strsignal` takes a signal number and returns a pointer to a static/thread-owned
    // NUL-terminated string, or null when unsupported. We copy it immediately.
    let ptr = unsafe { libc::strsignal(number) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` was verified non-null and points to a valid NUL-terminated C string.
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// Inverts a `strsignal` prose description (such as `portable-pty`'s `ExitStatus` signal text)
/// back to a portable POSIX signal name (`"SIGTERM"`, `"SIGKILL"`, etc.), or preserves the input
/// verbatim when unrecognized.
pub fn signal_name_from_description(description: &str) -> String {
    #[cfg(unix)]
    {
        if let Some(name) = descriptions().iter().find(|(_, prose)| prose == description).map(|(name, _)| *name) {
            return name.to_owned();
        }
    }
    description.to_owned()
}

/// Checks whether `pid` is `leader` or a descendant/session-member of `leader` via a bounded
/// `MAX_TREE_DEPTH` walk through `sysinfo`.
#[cfg(unix)]
pub fn inside_tree(leader: u32, pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    if leader < 2 {
        return false;
    }
    if pid == leader {
        return true;
    }
    let Ok(mut system) = System::new() else {
        return false;
    };
    let mut at = Pid::from_u32(pid);
    for _ in 0..MAX_TREE_DEPTH {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[at]), true, ProcessRefreshKind::nothing());
        let Some(process) = system.process(at) else {
            return false;
        };
        if process.session_id().is_some_and(|session| session.as_u32() == leader) {
            return true;
        }
        let Some(parent) = process.parent() else {
            return false;
        };
        if parent.as_u32() == leader {
            return true;
        }
        at = parent;
    }
    false
}

/// On Linux, opens a `pidfd` pinning `pid` to defeat PID-reuse TOCTOU races between ancestry
/// verification and signal delivery.
#[cfg(target_os = "linux")]
pub(crate) fn try_open_pidfd(pid: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;

    // SAFETY: `SYS_pidfd_open` takes `(pid_t, unsigned int flags)` and returns either a newly
    // created file descriptor (`>= 0`) with `O_CLOEXEC` set by the kernel, or `-1` on error.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
    if fd == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd != -1` is a fresh file descriptor owned exclusively by this call.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as std::os::fd::RawFd) })
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum PidfdOpenFailure {
    FallbackLegacyKill,
    Return(OsError),
}

#[cfg(target_os = "linux")]
fn classify_pidfd_open_error(err: std::io::Error, leader: u32, pid: u32) -> PidfdOpenFailure {
    match err.raw_os_error() {
        Some(libc::ENOSYS) => PidfdOpenFailure::FallbackLegacyKill,
        Some(libc::ESRCH) => PidfdOpenFailure::Return(OsError::OutsideSession { leader, pid }),
        _ => PidfdOpenFailure::Return(OsError::io("opening pidfd for session process", err)),
    }
}

/// On Linux, sends `sig` to the process pinned by `pidfd`.
#[cfg(target_os = "linux")]
pub(crate) fn pidfd_send_signal_raw(pidfd: &std::os::fd::OwnedFd, sig: libc::c_int) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: `pidfd` is an open valid pidfd, `info` is null (kernel fills default siginfo),
    // and `flags` is 0.
    let rc = unsafe {
        libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0u32)
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Delivers `kind` to `scope` within the session led by `leader`.
///
/// - On Windows: returns `Err(OsError::Unsupported)` honestly (process trees are terminated via
///   [`crate::tree::TreeGuard`]).
/// - On Unix: validates [`kill_target`]; for [`SignalScope::Pid`], verifies [`inside_tree`]
///   (pinning via `pidfd_open` on Linux to defeat PID reuse, falling back to `libc::kill` only
///   when `pidfd_open` is unsupported with `ENOSYS`) before signaling.
pub fn deliver_signal(leader: u32, kind: SignalKind, scope: SignalScope) -> Result<(), OsError> {
    #[cfg(not(unix))]
    {
        let _ = (leader, kind, scope);
        Err(OsError::Unsupported("this platform has no POSIX signals; terminate the session with kill".to_owned()))
    }
    #[cfg(unix)]
    {
        let target = kill_target(leader, scope)?;
        let sig = signal_number(kind);

        if let SignalScope::Pid(pid_target) = scope {
            let pid = pid_target.pid() as u32;
            #[cfg(target_os = "linux")]
            let pinned_pidfd = match try_open_pidfd(target.0) {
                Ok(fd) => Some(fd),
                Err(err) => match classify_pidfd_open_error(err, leader, pid) {
                    PidfdOpenFailure::FallbackLegacyKill => None,
                    PidfdOpenFailure::Return(os_err) => return Err(os_err),
                },
            };

            if !inside_tree(leader, pid) {
                return Err(OsError::OutsideSession { leader, pid });
            }

            #[cfg(target_os = "linux")]
            if let Some(pidfd) = pinned_pidfd {
                return pidfd_send_signal_raw(&pidfd, sig).map_err(|err| OsError::io("signaling session process", err));
            }
        }

        // SAFETY: `target.0` was validated by `KillTarget::at` (never `0`, `-1`, or `i32::MIN`),
        // and we read `target.0` directly rather than via an accessor.
        if unsafe { libc::kill(target.0, sig) } == 0 {
            return Ok(());
        }
        Err(OsError::io("signaling session", std::io::Error::last_os_error()))
    }
}

/// Sends `SIGHUP` to every non-leader process belonging to any POSIX session led by `leaders`.
///
/// Must be called **while the session leaders are still alive** so background job-control process
/// groups (which the kernel does not automatically SIGHUP on leader exit) are discovered via
/// `session_id` before reparenting.
pub fn hang_up_session_descendants(leaders: &[u32]) {
    #[cfg(not(unix))]
    {
        let _ = leaders;
    }
    #[cfg(unix)]
    {
        use apack_proto::types::SignalTarget;
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

        if leaders.is_empty() {
            return;
        }
        let Ok(mut system) = System::new() else {
            return;
        };
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let others: Vec<(u32, u32)> = system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                let session = process.session_id()?.as_u32();
                let pid_u32 = pid.as_u32();
                (leaders.contains(&session) && pid_u32 != session).then_some((session, pid_u32))
            })
            .collect();
        for (leader, pid) in others {
            if let Some(target) = i32::try_from(pid).ok().and_then(SignalTarget::new) {
                let _ = deliver_signal(leader, SignalKind::Hup, SignalScope::Pid(target));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apack_proto::types::SignalTarget;

    #[test]
    fn kill_target_rejects_broadcast_and_min() {
        assert!(KillTarget::at(0).is_err());
        assert!(KillTarget::at(-1).is_err());
        assert!(KillTarget::at(i32::MIN).is_err());
        assert_eq!(KillTarget::at(2).expect("pid 2").number(), 2);
        assert_eq!(KillTarget::at(-2).expect("pgid -2").number(), -2);
        assert_eq!(KillTarget::at(i32::MAX).expect("max").number(), i32::MAX);
    }

    #[test]
    fn kill_target_validates_scopes_and_rejects_pid_below_two() {
        // Leader < 2 rejected for all scopes
        assert!(kill_target(0, SignalScope::Leader).is_err());
        assert!(kill_target(1, SignalScope::Leader).is_err());
        assert!(kill_target(0, SignalScope::Group).is_err());
        assert!(kill_target(1, SignalScope::Group).is_err());
        let p42 = SignalTarget::new(42).expect("valid");
        assert!(kill_target(0, SignalScope::Pid(p42)).is_err());
        assert!(kill_target(1, SignalScope::Pid(p42)).is_err());

        // Pid 1 (init) rejected even with valid leader
        let p1 = SignalTarget::new(1).expect("SignalTarget allows 1");
        assert!(kill_target(500, SignalScope::Pid(p1)).is_err());

        // Overflowing u32 > i32::MAX rejected
        assert!(kill_target(u32::MAX, SignalScope::Leader).is_err());
        assert!(kill_target(u32::MAX, SignalScope::Group).is_err());

        // Valid targets including boundary pid 2
        assert_eq!(kill_target(2, SignalScope::Leader).expect("leader 2").number(), 2);
        assert_eq!(kill_target(500, SignalScope::Leader).expect("leader").number(), 500);
        assert_eq!(kill_target(500, SignalScope::Group).expect("group").number(), -500);
        assert_eq!(kill_target(500, SignalScope::Pid(p42)).expect("pid").number(), 42);
    }

    #[cfg(unix)]
    #[test]
    fn init_signal_descriptions_populates_once_lock() {
        init_signal_descriptions();
        assert!(NAMED_DESCRIPTIONS.get().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn signal_numbers_and_strsignal_roundtrip() {
        init_signal_descriptions();
        assert_eq!(signal_number(SignalKind::Hup), libc::SIGHUP);
        assert_eq!(signal_number(SignalKind::Int), libc::SIGINT);
        assert_eq!(signal_number(SignalKind::Quit), libc::SIGQUIT);
        assert_eq!(signal_number(SignalKind::Term), libc::SIGTERM);
        assert_eq!(signal_number(SignalKind::Kill), libc::SIGKILL);
        assert_eq!(signal_number(SignalKind::Usr1), libc::SIGUSR1);
        assert_eq!(signal_number(SignalKind::Usr2), libc::SIGUSR2);
        assert_eq!(signal_number(SignalKind::Stop), libc::SIGSTOP);
        assert_eq!(signal_number(SignalKind::Cont), libc::SIGCONT);

        let term_prose = described(libc::SIGTERM).expect("SIGTERM has strsignal description");
        assert_eq!(signal_name_from_description(&term_prose), "SIGTERM");
        let kill_prose = described(libc::SIGKILL).expect("SIGKILL has strsignal description");
        assert_eq!(signal_name_from_description(&kill_prose), "SIGKILL");
        assert_eq!(signal_name_from_description("unknown custom prose"), "unknown custom prose");
    }

    #[cfg(unix)]
    #[test]
    fn inside_tree_and_deliver_signal_real_child() {
        let me = std::process::id();
        assert!(inside_tree(me, me));
        assert!(inside_tree(2, 2));
        assert!(!inside_tree(me, 0));
        assert!(!inside_tree(me, 1));
        assert!(!inside_tree(0, me));
        assert!(!inside_tree(1, me));
        assert!(!inside_tree(me, u32::MAX - 1));

        // Spawn a real child and verify inside_tree + signal delivery.
        let mut child =
            std::process::Command::new("/bin/sh").args(["-c", "sleep 30"]).spawn().expect("spawn sleep child");
        let child_pid = child.id();
        assert!(inside_tree(me, child_pid));
        assert!(!inside_tree(child_pid, me));

        // Signaling a PID outside the child's tree (ourselves!) is refused with OutsideSession.
        let me_target = SignalTarget::new(me as i32).expect("self pid");
        let err = deliver_signal(child_pid, SignalKind::Term, SignalScope::Pid(me_target))
            .expect_err("refuse signaling parent outside child tree");
        assert!(matches!(err, OsError::OutsideSession { leader, pid } if leader == child_pid && pid == me));

        // Signaling the child via Pid scope within our tree succeeds.
        let child_target = SignalTarget::new(child_pid as i32).expect("child pid");
        deliver_signal(me, SignalKind::Term, SignalScope::Pid(child_target)).expect("SIGTERM child in our tree");
        let status = child.wait().expect("reap child");
        assert!(!status.success());

        // Signaling an already-reaped leader returns an OS error (ESRCH).
        let err = deliver_signal(child_pid, SignalKind::Term, SignalScope::Leader).expect_err("reaped leader fails");
        assert!(matches!(err, OsError::Io { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn hang_up_session_descendants_terminates_background_job_in_session() {
        use crate::pty::{NonBlockingRead, PtySpawnConfig, spawn_pty};
        use apack_proto::types::{CellPx, GridSize};
        use std::time::Instant;

        // Spawn a PTY session leader that starts a background `sleep 30`, prints `BG:<pid>`, and
        // actively `wait`s on it so `sh` reaps it immediately (avoiding Linux zombie `kill(0)==0`)
        // and reports `EXITED:<status>`.
        let config = PtySpawnConfig {
            size: GridSize::new(80, 24).expect("grid"),
            cell_px: CellPx::default(),
            shell: Some("/bin/sh".to_owned()),
            args: vec!["-c".to_owned(), "sleep 30 & bg=$!; echo BG:$bg; wait $bg; echo EXITED:$?".to_owned()],
            env: vec![],
            unset: vec![],
            cwd: None,
        };
        let mut spawned = spawn_pty(&config).expect("spawn pty");
        let mut output = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for BG pid");
            match spawned.master.read_nonblocking(&mut buf).expect("read") {
                NonBlockingRead::Bytes(n) => {
                    output.extend_from_slice(&buf[..n]);
                    let s = String::from_utf8_lossy(&output);
                    if s.contains("BG:") && s.lines().any(|l| l.trim().starts_with("BG:")) {
                        break;
                    }
                }
                NonBlockingRead::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                NonBlockingRead::Eof => panic!("unexpected EOF before BG"),
            }
        }

        // Send SIGHUP to non-leader descendants while leader is alive; `sleep 30` dies and `sh`
        // reaps it and prints `EXITED:<non-zero>` then exits cleanly.
        hang_up_session_descendants(&[spawned.pid]);

        let wait_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < wait_deadline,
                "timed out waiting for hang_up_session_descendants to terminate background job: {:?}",
                String::from_utf8_lossy(&output)
            );
            match spawned.master.read_nonblocking(&mut buf).expect("read exit") {
                NonBlockingRead::Bytes(n) => output.extend_from_slice(&buf[..n]),
                NonBlockingRead::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                NonBlockingRead::Eof => break,
            }
        }
        let text = String::from_utf8_lossy(&output);
        let status = spawned.child.wait().expect("reap leader");
        assert!(status.success(), "leader shell should finish cleanly after wait: {text:?}");
        assert!(text.contains("EXITED:") && !text.contains("EXITED:0"), "expected signal exit: {text:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_open_and_zero_signal_on_self() {
        assert!(try_open_pidfd(0).is_err());
        assert!(try_open_pidfd(-1).is_err());
        assert!(matches!(
            classify_pidfd_open_error(std::io::Error::from_raw_os_error(libc::ENOSYS), 10, 20),
            PidfdOpenFailure::FallbackLegacyKill
        ));
        assert!(matches!(
            classify_pidfd_open_error(std::io::Error::from_raw_os_error(libc::ESRCH), 10, 20),
            PidfdOpenFailure::Return(OsError::OutsideSession { leader: 10, pid: 20 })
        ));
        assert!(matches!(
            classify_pidfd_open_error(std::io::Error::from_raw_os_error(libc::EPERM), 10, 20),
            PidfdOpenFailure::Return(OsError::Io { .. })
        ));
        let fd = try_open_pidfd(std::process::id() as i32).expect("pidfd_open on self");
        pidfd_send_signal_raw(&fd, 0).expect("pidfd_send_signal(0) liveness probe");
        assert!(pidfd_send_signal_raw(&fd, -1).is_err(), "invalid signal must return Err");
    }

    #[cfg(not(unix))]
    #[test]
    fn windows_deliver_signal_returns_unsupported() {
        let err = deliver_signal(1000, SignalKind::Term, SignalScope::Leader).expect_err("unsupported on Windows");
        assert!(matches!(err, OsError::Unsupported(_)));
        assert_eq!(signal_name_from_description("Terminated"), "Terminated");
    }
}
