//! Operating-system drivers for `apack-terminal` v2.
//!
//! This crate isolates all platform-specific OS interactions behind narrow, safety-typed boundaries
//! so `apack-core` remains 100% sans-IO and deterministic:
//!
//! - [`signal`]: Sealed [`signal::KillTarget`] (making `kill(0)` / `kill(-1)` unrepresentable),
//!   TOCTOU-resistant process signaling (`pidfd_open` on Linux + bounded ancestry walk), and
//!   session-descendant `SIGHUP` escalation.
//! - [`tree`]: Kernel-enforced process-tree ownership ([`tree::TreeGuard`], using Windows Job
//!   Objects with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and native `JobObjectBasicProcessIdList`
//!   `owns(pid)`, and POSIX session/parent tree verification + foreground process inspection).
//! - [`security`]: Windows named-pipe SDDL generation and owner-only `SecurityDescriptor` helpers.
//! - [`transport`]: User-private Unix domain sockets (`0o700` dir, `flock` exclusive hold,
//!   `0o077` bind umask + `0o600` socket chmod, peer UID verification) and Windows protected named pipes (`GetSecurityInfo`
//!   owner verification, `CancelIoEx` hang-up / `assume_flushed` let-go), plus `writev` helpers.
//! - [`pty`]: Cross-platform PTY spawning (`portable-pty`), non-blocking Unix master fd I/O, and
//!   bounded-channel Windows ConPTY bridge with non-blocking drop.
//! - [`pty_poller`]: POSIX sharded non-blocking PTY reactor (`epoll` + `eventfd` on Linux,
//!   `poll(2)` + self-pipe on macOS/BSD — explicitly avoiding macOS `kqueue` `/dev/ptmx` quirks).
//! - [`image_sources`]: Sandboxed, off-shard Kitty graphics named-file and shared-memory reader
//!   with left-to-right symlink resolution, mount-device refusal, and per-session stuck-I/O circuit
//!   breaker.

pub mod image_sources;
pub mod pty;
#[cfg(unix)]
pub mod pty_poller;
#[cfg(target_os = "linux")]
pub mod pty_poller_epoll;
#[cfg(windows)]
pub mod pty_windows;
pub mod security;
#[cfg(windows)]
mod security_windows;
pub mod signal;
pub mod transport;
#[cfg(windows)]
mod transport_windows;
pub mod tree;
#[cfg(windows)]
mod tree_windows;

use std::fmt;
use std::io;

use apack_proto::types::ErrorCode;

/// Structured OS-layer failure that maps cleanly onto protocol [`ErrorCode`] values.
#[derive(Debug)]
pub enum OsError {
    /// Operation is not supported on this operating system (e.g. POSIX signals on Windows).
    Unsupported(String),
    /// Target PID does not belong to the session's process tree.
    OutsideSession {
        /// Session leader PID.
        leader: u32,
        /// Requested target PID.
        pid: u32,
    },
    /// Target PID is invalid or forbidden (`0`, `1`, or broadcast target).
    InvalidTarget(String),
    /// Socket/pipe path failed safety checks (symlink, wrong owner, group/world writable, non-socket).
    UnsafePath(String),
    /// Another broker already holds the socket lock or named-pipe instance.
    AlreadyHeld,
    /// Underlying OS I/O error with context.
    Io {
        /// Short description of the failed OS action.
        context: &'static str,
        /// Underlying system I/O error.
        source: io::Error,
    },
}

impl OsError {
    /// Wraps an `io::Error` with static context.
    pub fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }

    /// Maps this OS error to the closest v2 protocol [`ErrorCode`].
    pub fn error_code(&self) -> ErrorCode {
        match self {
            Self::Unsupported(_) => ErrorCode::Unsupported,
            Self::OutsideSession { .. } => ErrorCode::OutsideSession,
            Self::InvalidTarget(_) => ErrorCode::Malformed,
            Self::UnsafePath(_) | Self::AlreadyHeld | Self::Io { .. } => ErrorCode::Pty,
        }
    }
}

impl fmt::Display for OsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(msg) | Self::InvalidTarget(msg) | Self::UnsafePath(msg) => f.write_str(msg),
            Self::OutsideSession { leader, pid } => {
                write!(f, "pid {pid} is not in the process tree of session leader {leader}")
            }
            Self::AlreadyHeld => f.write_str("another broker already holds this socket"),
            Self::Io { context, source } => write!(f, "{context}: {source}"),
        }
    }
}

impl std::error::Error for OsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_error_maps_codes_and_displays() {
        let unsupported = OsError::Unsupported("no signals".into());
        assert_eq!(unsupported.error_code(), ErrorCode::Unsupported);
        assert_eq!(unsupported.to_string(), "no signals");
        assert!(std::error::Error::source(&unsupported).is_none());

        let outside = OsError::OutsideSession { leader: 100, pid: 200 };
        assert_eq!(outside.error_code(), ErrorCode::OutsideSession);
        assert!(outside.to_string().contains("200"));
        assert!(outside.to_string().contains("100"));

        let invalid = OsError::InvalidTarget("bad pid".into());
        assert_eq!(invalid.error_code(), ErrorCode::Malformed);
        assert_eq!(invalid.to_string(), "bad pid");

        let unsafe_path = OsError::UnsafePath("symlink".into());
        assert_eq!(unsafe_path.error_code(), ErrorCode::Pty);
        assert_eq!(unsafe_path.to_string(), "symlink");

        let held = OsError::AlreadyHeld;
        assert_eq!(held.error_code(), ErrorCode::Pty);
        assert!(held.to_string().contains("already holds"));

        let io_err = OsError::io("binding", io::Error::new(io::ErrorKind::PermissionDenied, "denied"));
        assert_eq!(io_err.error_code(), ErrorCode::Pty);
        assert!(io_err.to_string().contains("binding"));
        assert!(std::error::Error::source(&io_err).is_some());
    }
}
