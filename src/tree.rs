//! Owning a session's process tree on Windows, which is X1 and X2.
//!
//! **On POSIX nothing here is needed and nothing here exists.** Killing a session's shell makes the kernel
//! send SIGHUP to the pty's foreground process group, which takes the grandchildren with it — measured in
//! `tests/broker.rs`, including with the pty master held open, so it is the session leader's death that
//! carries it rather than the pty closing.
//!
//! **Windows has no equivalent**, and the first run of that case there said so exactly: *the grandchild 2944
//! outlived the session it belonged to*. A Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the
//! mechanism — the OS kills every process in the job when the last handle to it closes, so the tree ends
//! because this process let go of it rather than because cleanup code remembered to walk it. That is the
//! property X1 wants: it holds when the broker crashes, too.
//!
//! **`command-group` is not what does it**, though this repo retired its API against exactly this case.
//! That crate wraps `std::process::Command`, and `portable-pty` spawns the child itself — there is no
//! `Command` here to wrap. The job is assigned to the pid afterwards instead, which needs no control over
//! the spawn.

/// A session's process tree, held for as long as the session is.
pub struct Tree {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
// SAFETY: a job handle is a kernel object usable from any thread; nothing here keeps thread-affine state.
unsafe impl Send for Tree {}
#[cfg(windows)]
// SAFETY: as above — the handle is only read, and only closed once, by `Drop`.
unsafe impl Sync for Tree {}

#[cfg(windows)]
impl Tree {
    /// Puts `pid` and everything it starts into a job this process owns.
    ///
    /// Answers `None` when the OS refuses, which is reported by the caller rather than failing the spawn: a
    /// terminal that runs and leaks a background process is worse than one that does not run, but not by
    /// enough to refuse the user their shell.
    pub fn owning(pid: u32) -> Option<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

        // SAFETY: every call below is checked, and each handle is closed on the path that owns it.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_mut(&mut limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                CloseHandle(job);
                return None;
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, FALSE, pid);
            if process.is_null() {
                CloseHandle(job);
                return None;
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Self { job })
        }
    }
}

#[cfg(windows)]
impl Drop for Tree {
    fn drop(&mut self) {
        // The kill is the close: `KILL_ON_JOB_CLOSE` ends every process in the job when the last handle goes
        // SAFETY: `job` was created by `owning` and is closed exactly once, here.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
    }
}

#[cfg(not(windows))]
impl Tree {
    /// POSIX needs none: the kernel's SIGHUP to the pty's foreground group reaps the tree.
    pub fn owning(_pid: u32) -> Option<Self> {
        None
    }
}
