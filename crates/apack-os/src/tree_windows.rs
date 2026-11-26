//! Windows Job Object process-tree ownership and native `JobObjectBasicProcessIdList` queries.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and the Windows CI mutation
//! job tests every mutant directly on `windows-latest`.

#![cfg(windows)]

use std::io;
use std::mem::{offset_of, size_of};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, FALSE, GetLastError, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

/// Maximum retries when `QueryInformationJobObject` reports `ERROR_MORE_DATA`.
const MAX_QUERY_RETRIES: usize = 4;

/// Initial PID-list capacity (in entries) for `QueryInformationJobObject`.
const INITIAL_PID_CAPACITY: usize = 64;

/// Affine wrapper around a Windows Job Object configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
pub struct JobGuard {
    job: HANDLE,
}

// SAFETY: A Windows Job Object HANDLE is a kernel object usable across threads; it holds no
// thread-affine state and is closed exactly once in `Drop`.
unsafe impl Send for JobGuard {}

// SAFETY: `QueryInformationJobObject` is thread-safe on a live Job Object handle, and `CloseHandle`
// only runs in `Drop` when no shared references remain.
unsafe impl Sync for JobGuard {}

impl JobGuard {
    /// Creates a `KILL_ON_JOB_CLOSE` Job Object and assigns `pid` to it.
    pub fn attach(pid: u32) -> Option<Self> {
        // SAFETY: Every handle returned by Win32 is null-checked and closed on failure paths.
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

    /// Queries the kernel via `JobObjectBasicProcessIdList` to check whether `pid` is currently
    /// assigned to this Job Object.
    pub fn owns(&self, pid: u32) -> io::Result<bool> {
        self.owns_with_capacity(pid, INITIAL_PID_CAPACITY)
    }

    fn owns_with_capacity(&self, pid: u32, initial_capacity: usize) -> io::Result<bool> {
        if pid == 0 {
            return Ok(false);
        }
        let target = pid as usize;
        let header_bytes = offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut capacity = initial_capacity.max(1);

        for _ in 0..MAX_QUERY_RETRIES {
            let pid_bytes = capacity
                .checked_mul(size_of::<usize>())
                .ok_or_else(|| io::Error::other("JobObjectBasicProcessIdList size overflow"))?;
            let total_bytes = header_bytes
                .checked_add(pid_bytes)
                .ok_or_else(|| io::Error::other("JobObjectBasicProcessIdList size overflow"))?;
            let word_count = total_bytes.div_ceil(size_of::<usize>());
            let alloc_bytes = word_count
                .checked_mul(size_of::<usize>())
                .ok_or_else(|| io::Error::other("JobObjectBasicProcessIdList size overflow"))?;
            let buf_len = u32::try_from(alloc_bytes)
                .map_err(|_| io::Error::other("JobObjectBasicProcessIdList exceeds u32::MAX"))?;

            let mut buffer = vec![0usize; word_count];
            let mut returned: u32 = 0;

            // SAFETY: `self.job` is a valid open Job Object handle, `buffer` is `usize`-aligned
            // (satisfying `JOBOBJECT_BASIC_PROCESS_ID_LIST`'s alignment) and holds `buf_len` bytes.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.job,
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    buf_len,
                    &mut returned,
                )
            };

            let list_ptr = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            if ok != 0 {
                // SAFETY: `buffer` holds at least `size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>()`
                // bytes; reading the header field via raw pointer preserves full `buffer` provenance
                // for the flexible `ProcessIdList` tail slice below.
                let reported = unsafe { std::ptr::addr_of!((*list_ptr).NumberOfProcessIdsInList).read() } as usize;
                let max_entries = (alloc_bytes - header_bytes) / size_of::<usize>();
                let count = reported.min(max_entries);
                // SAFETY: `header_bytes + count * size_of::<usize>() <= alloc_bytes` within
                // `buffer`, and `header_bytes` (`8`) is aligned to `align_of::<usize>()`.
                let base = unsafe { buffer.as_ptr().cast::<u8>().add(header_bytes).cast::<usize>() };
                // SAFETY: `QueryInformationJobObject` initialized `count` `usize` entries at `base`
                // within `buffer`'s live allocation.
                let pids = unsafe { std::slice::from_raw_parts(base, count) };
                return Ok(pids.contains(&target));
            }

            // SAFETY: `GetLastError` reads the calling thread's last Win32 error code.
            let err = unsafe { GetLastError() };
            if err == ERROR_MORE_DATA {
                // SAFETY: `buffer` holds the initialized header fields written by `QueryInformationJobObject`.
                let needed = unsafe { std::ptr::addr_of!((*list_ptr).NumberOfAssignedProcesses).read() } as usize;
                capacity = needed.max(capacity.saturating_mul(2));
                continue;
            }
            return Err(io::Error::from_raw_os_error(err as i32));
        }

        Err(io::Error::other("JobObjectBasicProcessIdList exceeded retry limit"))
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // SAFETY: `self.job` was created by `CreateJobObjectW` in `attach` and is closed once here.
        unsafe { CloseHandle(self.job) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_guard_owns_assigned_child_grows_on_more_data_and_kills_on_drop() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 >nul"])
            .spawn()
            .expect("spawn child");
        let child_pid = child.id();

        let guard = JobGuard::attach(child_pid).expect("attach job to child");
        // Give `cmd.exe` a brief moment to spawn `ping.exe` so the Job Object has >= 2 processes,
        // exercising the `ERROR_MORE_DATA` growth path when queried with `initial_capacity = 1`.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(guard.owns_with_capacity(child_pid, 1).expect("query with cap 1"));
        assert!(guard.owns(child_pid).expect("query child pid"));
        assert!(!guard.owns(0).expect("pid 0 never owned"));
        assert!(!guard.owns(std::process::id()).expect("broker process not in child job"));

        // Dropping the JobGuard must terminate the child process via KILL_ON_JOB_CLOSE.
        drop(guard);
        let status = child.wait().expect("reap killed child");
        assert!(!status.success());
    }
}
