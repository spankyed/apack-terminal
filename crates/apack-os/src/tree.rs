//! Kernel-enforced process-tree ownership and foreground process inspection.
//!
//! - On Windows, [`TreeGuard`] owns a Win32 Job Object configured with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so closing the guard (or crashing the broker) atomically
//!   terminates the entire process tree, and [`TreeGuard::owns`] queries
//!   `JobObjectBasicProcessIdList` natively.
//! - On Unix, [`TreeGuard`] records the session leader PID and [`TreeGuard::owns`] performs the
//!   bounded ancestry/session-id check in [`crate::signal::inside_tree`].

use std::io;

use apack_proto::types::ForegroundProcess;
use portable_pty::MasterPty;

/// Affine ownership handle for a session's process tree.
pub struct TreeGuard {
    #[cfg(unix)]
    leader: u32,
    #[cfg(windows)]
    inner: crate::tree_windows::JobGuard,
}

impl TreeGuard {
    /// Attaches process-tree ownership to the newly spawned session leader `pid`.
    ///
    /// Returns `None` if `pid < 2` or (on Windows) if the OS refuses Job Object creation or
    /// assignment.
    pub fn attach(pid: u32) -> Option<Self> {
        if pid < 2 {
            return None;
        }
        #[cfg(unix)]
        {
            Some(Self { leader: pid })
        }
        #[cfg(windows)]
        {
            crate::tree_windows::JobGuard::attach(pid).map(|inner| Self { inner })
        }
    }

    /// Returns whether `pid` belongs to this session's process tree.
    ///
    /// Supported natively on both POSIX (`sysinfo` session-id / parent walk) and Windows
    /// (`QueryInformationJobObject(JobObjectBasicProcessIdList)`).
    pub fn owns(&self, pid: u32) -> io::Result<bool> {
        #[cfg(unix)]
        {
            Ok(crate::signal::inside_tree(self.leader, pid))
        }
        #[cfg(windows)]
        {
            self.inner.owns(pid)
        }
    }
}

/// Reads the foreground process group leader (PID and executable name) of a PTY master.
///
/// Returns `None` on Windows (ConPTY has no POSIX foreground process group) or if `tcgetpgrp`
/// reports no leader.
pub fn foreground_process(master: &(dyn MasterPty + Send)) -> Option<ForegroundProcess> {
    #[cfg(not(unix))]
    {
        let _ = master;
        None
    }
    #[cfg(unix)]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

        let pid: i32 = master.process_group_leader()?;
        let target = apack_proto::types::SignalTarget::new(pid)?;
        let of = Pid::from_u32(u32::try_from(pid).ok()?);
        let mut system = System::new().ok()?;
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[of]), true, ProcessRefreshKind::nothing());
        let name = system.process(of).map(|p| p.name().to_string_lossy().into_owned());
        Some(ForegroundProcess { pid: target, name })
    }
}

/// Reads the command-line `argv` of a single PID via `sysinfo` (Unix only; safe to call off-shard).
pub fn argv_for_pid(pid: u32) -> Option<Vec<String>> {
    #[cfg(not(unix))]
    {
        let _ = pid;
        None
    }
    #[cfg(unix)]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

        let of = Pid::from_u32(pid);
        let mut system = System::new().ok()?;
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[of]),
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        let argv: Vec<String> =
            system.process(of)?.cmd().iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        (!argv.is_empty()).then_some(argv)
    }
}

/// Reads the command-line `argv` of the foreground process group leader of a PTY master.
///
/// Used by the broker to vouch for `OSC 7501` program status claims against the actual foreground
/// executable. Returns `None` on Windows or when unavailable.
pub fn foreground_argv(master: &(dyn MasterPty + Send)) -> Option<Vec<String>> {
    #[cfg(not(unix))]
    {
        let _ = master;
        None
    }
    #[cfg(unix)]
    {
        let pid: i32 = master.process_group_leader()?;
        argv_for_pid(u32::try_from(pid).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_guard_rejects_pid_below_two() {
        assert!(TreeGuard::attach(0).is_none());
        assert!(TreeGuard::attach(1).is_none());
        #[cfg(unix)]
        assert!(TreeGuard::attach(2).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn unix_tree_guard_owns_self_and_child() {
        let me = std::process::id();
        let guard = TreeGuard::attach(me).expect("attach self");
        assert!(guard.owns(me).expect("owns self"));
        assert!(!guard.owns(1).expect("does not own init"));

        struct ChildReapGuard(std::process::Child);
        impl Drop for ChildReapGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = ChildReapGuard(
            std::process::Command::new("/bin/sh").args(["-c", "sleep 10"]).spawn().expect("spawn child"),
        );
        let child_pid = child.0.id();
        assert!(guard.owns(child_pid).expect("owns child"));
        drop(child);
    }
}
