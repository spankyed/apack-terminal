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
    /// Constructs a [`KillTarget`], refusing broadcast targets (`0`, `-1`), init (`1`), and `i32::MIN`.
    pub fn at(target: i32) -> Result<Self, OsError> {
        if target == 0 {
            return Err(OsError::InvalidTarget("pid 0 targets every process in this broker's own group".to_owned()));
        }
        if target == -1 {
            return Err(OsError::InvalidTarget("pid -1 targets every process the user owns".to_owned()));
        }
        if target == 1 {
            return Err(OsError::InvalidTarget("pid 1 is init and is not a signalable session process".to_owned()));
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

/// Non-reaping probe result for a Unix child leader via `waitid(P_PID, ..., WEXITED | WNOHANG | WNOWAIT)`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExitProbe {
    /// Child process is still running.
    StillRunning,
    /// Child process has exited and remains a zombie pinning its PID until reaped.
    ExitedZombie,
    /// `pid` is not a direct unreaped child of this process (e.g. `ECHILD` or invalid PID).
    NotChild,
}

/// Probes whether Unix child `pid` has exited WITHOUT reaping it (`WNOWAIT`), keeping any zombie
/// pinned so its PID cannot be recycled while callers signal remaining session/tree descendants.
#[cfg(unix)]
pub fn probe_child_exit_unreaped(pid: u32) -> ChildExitProbe {
    let raw_pid = pid as libc::id_t;
    loop {
        // SAFETY: zero-initialized `libc::siginfo_t` is valid for `waitid(2)`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is valid writable storage; `P_PID` targets only direct child `raw_pid`
        // (never broadcasting on 0/-1), and `WNOWAIT` leaves any exited child unreaped.
        let rc =
            unsafe { libc::waitid(libc::P_PID, raw_pid, &raw mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) };
        if rc == 0 {
            #[cfg(target_os = "linux")]
            // SAFETY: `waitid` succeeded and populated `info`; `si_pid() == 0` means still running.
            let exited_pid = unsafe { info.si_pid() };
            #[cfg(not(target_os = "linux"))]
            let exited_pid = info.si_pid;
            return if exited_pid != 0 { ChildExitProbe::ExitedZombie } else { ChildExitProbe::StillRunning };
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return ChildExitProbe::NotChild;
    }
}

/// Sends `SIGHUP` to every non-leader descendant belonging to any session or reachable process tree led by `leaders`.
///
/// Must be called **while the session leaders are still alive or unreaped zombies** so both
/// background job-control process groups (`sid == leader`) and `setsid()` descendants still
/// reachable via the live parent chain are discovered before reparenting or PID reuse. (A grandchild
/// that calls `setsid()` and is reparented to PID 1 after its intermediate parent exits prior to
/// discovery is outside standard POSIX parent-chain tracking.)
pub fn hang_up_session_descendants(leaders: &[u32]) {
    signal_session_descendants(leaders, SignalKind::Hup);
}

/// Sends `SIGKILL` to every non-leader descendant belonging to any session or reachable process tree led by `leaders`.
///
/// Must be called **while the session leaders are still alive or unreaped zombies** so `SIGHUP`-ignoring
/// background jobs and `setsid()` descendants still reachable via the live parent chain are
/// terminated before reparenting or PID reuse.
pub fn kill_session_descendants(leaders: &[u32]) {
    signal_session_descendants(leaders, SignalKind::Kill);
}

#[cfg(unix)]
fn find_descendant_leader(
    lookup: impl Fn(u32) -> Option<(Option<u32>, Option<u32>)>,
    leaders: &[u32],
    pid_u32: u32,
) -> Option<(u32, usize)> {
    if leaders.contains(&pid_u32) {
        return None;
    }
    let mut at = pid_u32;
    for depth in 0..MAX_TREE_DEPTH {
        let (session, parent) = lookup(at)?;
        if let Some(sid) = session
            && leaders.contains(&sid)
        {
            return Some((sid, depth));
        }
        let parent = parent?;
        if leaders.contains(&parent) {
            return Some((parent, depth + 1));
        }
        if parent == at {
            return None;
        }
        at = parent;
    }
    None
}

fn signal_session_descendants(leaders: &[u32], kind: SignalKind) {
    #[cfg(not(unix))]
    {
        let _ = (leaders, kind);
    }
    #[cfg(unix)]
    {
        use apack_proto::types::SignalTarget;
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

        if leaders.is_empty() {
            return;
        }
        let Ok(mut system) = System::new() else {
            return;
        };
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let processes = system.processes();
        let lookup = |p: u32| {
            processes
                .get(&Pid::from_u32(p))
                .map(|proc| (proc.session_id().map(|s| s.as_u32()), proc.parent().map(|par| par.as_u32())))
        };
        let mut others: Vec<(u32, u32, usize)> = processes
            .keys()
            .filter_map(|&pid| {
                find_descendant_leader(lookup, leaders, pid.as_u32()).map(|(l, depth)| (l, pid.as_u32(), depth))
            })
            .collect();
        // Signal deepest descendants first so killing an intermediate `setsid()` parent cannot
        // reparent its children to PID 1 before they are verified and signaled.
        others.sort_unstable_by_key(|a| std::cmp::Reverse(a.2));
        for (leader, pid, _) in others {
            if let Some(target) = i32::try_from(pid).ok().and_then(SignalTarget::new) {
                let _ = deliver_signal(leader, kind, SignalScope::Pid(target));
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
        struct ChildReapGuard(std::process::Child);
        impl Drop for ChildReapGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = ChildReapGuard(
            std::process::Command::new("/bin/sh").args(["-c", "sleep 10"]).spawn().expect("spawn sleep child"),
        );
        let child_pid = child.0.id();
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
        let status = child.0.wait().expect("reap child");
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

        struct SpawnedPtyCleanup(Option<crate::pty::SpawnedPty>);
        impl Drop for SpawnedPtyCleanup {
            fn drop(&mut self) {
                if let Some(mut s) = self.0.take() {
                    if s.pid >= 2 {
                        kill_session_descendants(&[s.pid]);
                        let _ = deliver_signal(s.pid, SignalKind::Kill, SignalScope::Group);
                    }
                    let _ = s.killer.kill();
                    drop(s.master);
                    let _ = s.child.wait();
                }
            }
        }

        // Spawn a PTY session leader that starts a background `sleep 10`, prints `BG:<pid>`, and
        // actively `wait`s on it so `sh` reaps it immediately (avoiding Linux zombie `kill(0)==0`)
        // and reports `EXITED:<status>`.
        let config = PtySpawnConfig {
            size: GridSize::new(80, 24).expect("grid"),
            cell_px: CellPx::default(),
            shell: Some("/bin/sh".to_owned()),
            args: vec!["-c".to_owned(), "sleep 10 & bg=$!; echo BG:$bg; wait $bg; echo EXITED:$?".to_owned()],
            env: vec![],
            unset: vec![],
            cwd: None,
        };
        let mut guard = SpawnedPtyCleanup(Some(spawn_pty(&config).expect("spawn pty")));
        let spawned = guard.0.as_mut().expect("spawned");
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

        // Send SIGHUP to non-leader descendants while leader is alive; `sleep 10` dies and `sh`
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
        drop(guard);

        // Also test kill_session_descendants on a SIGHUP-ignoring background job in its own group!
        let cfg_k = PtySpawnConfig {
            size: GridSize::new(80, 24).expect("80x24"),
            cell_px: CellPx::default(),
            shell: Some(String::from("/bin/sh")),
            args: vec![
                String::from("-c"),
                String::from("set -m; (trap '' HUP; sleep 10) & bg=$!; echo BG:$bg; wait $bg; echo EXITED:$?"),
            ],
            env: vec![],
            unset: vec![],
            cwd: None,
        };
        let mut guard_k = SpawnedPtyCleanup(Some(spawn_pty(&cfg_k).expect("spawn sh for kill_session_descendants")));
        let spawned_k = guard_k.0.as_mut().expect("spawned_k");
        let mut out_k = Vec::new();
        let deadline_k = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline_k, "timed out waiting for BG pid");
            match spawned_k.master.read_nonblocking(&mut buf).expect("read bg") {
                NonBlockingRead::Bytes(n) => {
                    out_k.extend_from_slice(&buf[..n]);
                    let s = String::from_utf8_lossy(&out_k);
                    if s.contains("BG:") && s.lines().any(|l| l.trim().starts_with("BG:")) {
                        break;
                    }
                }
                NonBlockingRead::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                NonBlockingRead::Eof => panic!("unexpected EOF before BG"),
            }
        }
        kill_session_descendants(&[spawned_k.pid]);
        let wait_dl_k = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < wait_dl_k, "timed out waiting for kill_session_descendants");
            match spawned_k.master.read_nonblocking(&mut buf).expect("read exit") {
                NonBlockingRead::Bytes(n) => out_k.extend_from_slice(&buf[..n]),
                NonBlockingRead::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                NonBlockingRead::Eof => break,
            }
        }
        let text_k = String::from_utf8_lossy(&out_k);
        assert!(spawned_k.child.wait().expect("reap leader").success());
        assert!(text_k.contains("EXITED:") && !text_k.contains("EXITED:0"), "expected SIGKILL exit: {text_k:?}");
        drop(guard_k);

        // 3. `probe_child_exit_unreaped` (`M6`) keeps exited zombie pinned until `wait()` reaps it,
        //    and `kill_session_descendants` (`M5`) also kills `setsid()` descendants via parent-chain walk!
        assert_eq!(probe_child_exit_unreaped(0), ChildExitProbe::NotChild);
        assert_eq!(probe_child_exit_unreaped(1), ChildExitProbe::NotChild);
        assert_eq!(probe_child_exit_unreaped(std::process::id()), ChildExitProbe::NotChild);

        let mut fast = std::process::Command::new("/bin/sh").args(["-c", "sleep 0.15"]).spawn().expect("spawn fast");
        let fast_pid = fast.id();
        assert_eq!(probe_child_exit_unreaped(fast_pid), ChildExitProbe::StillRunning);
        let probe_dl = Instant::now() + Duration::from_secs(3);
        while Instant::now() < probe_dl && probe_child_exit_unreaped(fast_pid) != ChildExitProbe::ExitedZombie {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(probe_child_exit_unreaped(fast_pid), ChildExitProbe::ExitedZombie, "zombie must stay unreaped");
        // Still unreaped on second probe (proving WNOWAIT did not reap it!):
        assert_eq!(probe_child_exit_unreaped(fast_pid), ChildExitProbe::ExitedZombie);
        assert!(fast.wait().expect("reap fast").success());
        assert_eq!(probe_child_exit_unreaped(fast_pid), ChildExitProbe::NotChild);

        struct ReapGuard(std::process::Child);
        impl Drop for ReapGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        // Spawn a parent-chain descendant in its OWN `setsid()` session (via python3 `os.setsid()`
        // or fallback to child sleep verified by `kill_session_descendants`).
        let mut parent_sh = ReapGuard(
            std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "if command -v python3 >/dev/null 2>&1; then python3 -c 'import os, time; os.setsid(); print(\"SID_READY\", flush=True); time.sleep(30)'; else printf 'SID_READY\\n'; sleep 30; fi",
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn setsid descendant test"),
        );
        let parent_pid = parent_sh.0.id();
        let mut line = String::new();
        let mut reader = std::io::BufReader::new(parent_sh.0.stdout.take().unwrap());
        let _ = std::io::BufRead::read_line(&mut reader, &mut line);
        assert!(line.contains("SID_READY"), "expected SID_READY, got {line:?}");
        kill_session_descendants(&[parent_pid]);
        // Wait briefly for the setsid() grandchild to die so `parent_sh` finishes promptly!
        let kill_dl = Instant::now() + Duration::from_secs(3);
        let mut reaped = false;
        while Instant::now() < kill_dl {
            if parent_sh.0.try_wait().expect("try_wait setsid parent").is_some() {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(reaped, "setsid() descendant survived kill_session_descendants");

        // Pure unit coverage for `find_descendant_leader`: leader itself excluded, session match,
        // `setsid()` grandchild via parent-chain walk, self-loop protection, and unrelated PID.
        let mut snap: std::collections::HashMap<u32, (Option<u32>, Option<u32>)> = std::collections::HashMap::new();
        snap.insert(100, (Some(100), Some(1))); // leader
        snap.insert(101, (Some(100), Some(1))); // reparented to init, same session 100
        snap.insert(102, (Some(102), Some(100))); // direct child that called setsid()
        snap.insert(103, (Some(103), Some(102))); // grandchild that called setsid()
        snap.insert(200, (Some(200), Some(200))); // self-loop outside tree
        snap.insert(201, (Some(201), Some(1))); // unrelated process
        let lookup = |p: u32| snap.get(&p).copied();
        assert_eq!(find_descendant_leader(lookup, &[100], 100), None);
        assert_eq!(find_descendant_leader(lookup, &[100], 101), Some((100, 0)));
        assert_eq!(find_descendant_leader(lookup, &[100], 102), Some((100, 1)));
        assert_eq!(find_descendant_leader(lookup, &[100], 103), Some((100, 2)));
        assert_eq!(find_descendant_leader(lookup, &[100], 200), None);
        assert_eq!(find_descendant_leader(lookup, &[100], 201), None);
        assert_eq!(find_descendant_leader(lookup, &[100], 999), None);
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
