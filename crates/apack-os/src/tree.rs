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

#[cfg(unix)]
fn parse_proc_comm(raw: &str) -> Option<String> {
    let trimmed = raw.trim_end_matches(['\r', '\n']);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Decodes a NUL-terminated UTF-8-lossy buffer returned by `libc::proc_name`.
#[cfg(unix)]
fn decode_proc_name_buf(buf: &[u8], len: i32) -> Option<String> {
    let used = usize::try_from(len).ok()?.min(buf.len());
    let nul = buf[..used].iter().position(|&b| b == 0).unwrap_or(used);
    let name = String::from_utf8_lossy(&buf[..nul]);
    parse_proc_comm(&name)
}

/// Reads the short executable name for a single Unix PID without constructing a `sysinfo::System`.
#[cfg(unix)]
pub fn process_name_for_pid(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        parse_proc_comm(&comm)
    }
    #[cfg(target_os = "macos")]
    {
        let pid_i32 = i32::try_from(pid).ok()?;
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a live writable stack buffer of length `buf.len()`; `proc_name` writes
        // at most `buffersize` bytes and returns the positive byte length on success or `<= 0` on error.
        let len = unsafe { libc::proc_name(pid_i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
        decode_proc_name_buf(&buf, len)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

        let of = Pid::from_u32(pid);
        let mut system = System::new().ok()?;
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[of]), true, ProcessRefreshKind::nothing());
        let name = system.process(of)?.name().to_string_lossy().into_owned();
        parse_proc_comm(&name)
    }
}

/// Reads the foreground process group leader (PID and executable name) of a PTY master.
///
/// Uses an O(1) single-PID OS query (`/proc/<pid>/comm` on Linux, `proc_name` on macOS) without
/// constructing `sysinfo::System` so callers on hot shard threads never scan the process table.
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
        let pid: i32 = master.process_group_leader()?;
        let target = apack_proto::types::SignalTarget::new(pid)?;
        let name = process_name_for_pid(u32::try_from(pid).ok()?);
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
        assert_eq!(parse_proc_comm("zsh\n"), Some(String::from("zsh")));
        assert_eq!(parse_proc_comm("\r\n"), None);
        assert_eq!(decode_proc_name_buf(b"zsh\0trailing", 12), Some(String::from("zsh")));
        assert_eq!(decode_proc_name_buf(b"bash", 99), Some(String::from("bash")));
        assert_eq!(decode_proc_name_buf(b"bash", 0), None);
        assert_eq!(decode_proc_name_buf(b"bash", -1), None);
        assert!(process_name_for_pid(0).is_none());
        assert!(process_name_for_pid(u32::MAX).is_none());
        let my_name = process_name_for_pid(me).expect("current process name");
        assert!(!my_name.is_empty());
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
