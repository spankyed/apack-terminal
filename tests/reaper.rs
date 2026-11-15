//! What stops a case's broker when the case cannot.
//!
//! **A case stops its broker in a destructor, and a destructor runs only in a process that ends of its own
//! accord.** The runner kills a case at its deadline, and kills the ones in flight when a run is told to
//! stop at its first failure — which a mutation run is, once a mutant. The broker is in a session of its
//! own (`tests/broker.rs` on why), so the signal that ends the case does not reach it, and what is left is
//! a broker to end itself by its own rules: one holding a kept session is meant not to, and one that is a
//! mutant of those rules may not. A pass over a branch left them by the hundred.
//!
//! **So each broker has a watcher, and the watcher has a pipe.** The case holds the pipe's writing end
//! and writes nothing to it; the watcher reads it, which returns when the last holder of that end is
//! gone — however the case went, a kill included, since the kernel is what closes it. Then the watcher
//! kills the one process it was started for and exits. Nothing is polled, and a case that ends normally
//! stops its watcher before it stops its broker (`stand_down`), so the watcher never acts on a pid the
//! broker has already given back.

#[cfg(unix)]
use std::process::{Child, ChildStdin, Command, Stdio};

/// The watcher of one broker, for as long as the case that started it holds this.
pub struct Reaper {
    #[cfg(unix)]
    watcher: Option<(Child, ChildStdin)>,
}

/// Starts watching `broker`, which is the program at `program`.
///
/// **The watcher kills by pid, and only a pid that is still that program.** A pid is a number the system
/// hands out again; one that no longer names the broker's executable is somebody else's process, and is
/// left alone. A single process, never a group: the broker's sessions end when it does, their terminals
/// having closed.
#[cfg(unix)]
pub fn watching(broker: &Child, program: &str) -> Reaper {
    use std::os::unix::process::CommandExt;

    let pid = broker.id();
    assert!(pid > 1, "a broker's pid is its own: {pid}");
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(r#"read _; case "$(ps -o command= -p "$1")" in "$2"*) kill -KILL "$1" ;; esac"#)
        .arg("sh")
        .arg(pid.to_string())
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // In a session of its own, as the broker is: the signal that ends a case is sent to the case's
    // group, and a watcher in it would be ended with the case it is there to outlive
    // SAFETY: runs in the child between fork and exec, and `setsid` touches nothing this process owns
    unsafe { command.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }) };
    let mut watcher = command.spawn().expect("a watcher starts");
    let held = watcher.stdin.take().expect("piped stdin");
    Reaper { watcher: Some((watcher, held)) }
}

#[cfg(not(unix))]
pub fn watching(_broker: &std::process::Child, _program: &str) -> Reaper {
    // A session's processes are in a job that ends with its broker (`src/tree.rs`), and a broker a
    // case leaves is ended by its own rules; there is no pipe to a shell to hold here
    Reaper {}
}

/// What ends the broker on a socket when the case that had one started is over, however it ended.
///
/// **For a broker the case did not start and has no pid for.** A proxy starts a broker where none is
/// listening, in a session of its own, and says nothing of it but that it is there. Such a broker goes
/// by its own rules once its connections have, and a mutant of those rules does not.
pub struct Ending {
    #[cfg(unix)]
    watcher: Option<(Child, ChildStdin)>,
    #[cfg(windows)]
    job: Option<contained::CaseJob>,
}

/// Starts watching for the end of the case on behalf of whatever broker comes to hold `socket`. Called
/// before anything is started on it, and held to the end of the case: declared before the proxies, so
/// that it is dropped after them.
///
/// **The broker is found by the lock it holds, when the case is over, and not before.** A broker keeps
/// the file beside its socket open and locked for as long as it lives (`listening::hold`), so whatever
/// has that file open is the broker on that socket: the system is asked who does, each answer is checked
/// to be `program`, and that one process is killed. Asked only then, so there is no moment a broker
/// exists and nothing knows of it, and no pid is carried from an earlier time to be somebody else's by
/// now. Where the system has no `lsof` nothing is found and nothing is ended.
///
/// **The watcher stays until the broker has gone, and not for ever.** A kill is asked for and happens
/// a moment later, and what follows a case — its directory removed, the next case's broker on a socket
/// of the same name — is owed a broker that is no longer holding anything. A process its parent has not
/// collected still answers to its pid, so the wait is counted: five seconds, then it is left.
///
/// **It acts at every ending, a clean one included.** A case that fails drops what it holds as one that
/// passes does, and a broker that will not go by itself is as much left behind by either.
#[cfg(unix)]
pub fn ending_with_the_case(socket: &std::path::Path, program: &str) -> Ending {
    use std::os::unix::process::CommandExt;

    let mut lock = socket.as_os_str().to_owned();
    lock.push(".lock");
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(
            r#"read _; for pid in $(lsof -t -- "$1" 2>/dev/null); do case "$(ps -o command= -p "$pid")" in "$2"*) kill -KILL "$pid"; n=0; while [ "$n" -lt 100 ] && kill -0 "$pid" 2>/dev/null; do n=$((n+1)); sleep 0.05; done ;; esac; done"#,
        )
        .arg("sh")
        .arg(lock)
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // In a session of its own, for the reason `watching`'s is
    // SAFETY: runs in the child between fork and exec, and `setsid` touches nothing this process owns
    unsafe { command.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }) };
    let mut watcher = command.spawn().expect("a watcher starts");
    let held = watcher.stdin.take().expect("piped stdin");
    Ending { watcher: Some((watcher, held)) }
}

/// On Windows the case's processes are in a job of its own, which the case's `Ending` ends when it is
/// dropped. A process started with `spawn_contained` while this is held is put in the job before it runs,
/// and so is the broker it starts: a job's processes inherit into their children, and nothing here breaks
/// away. A killed case ends its job when the test process ends, the handle closing with it.
#[cfg(windows)]
pub fn ending_with_the_case(_socket: &std::path::Path, _program: &str) -> Ending {
    Ending { job: Some(contained::CaseJob::new()) }
}

/// Starts `command` as the case that holds an `Ending` would, so that it and what it starts are ended with
/// that case. Without one on this thread it is started as it always is.
#[cfg(unix)]
pub fn spawn_contained(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    command.spawn()
}

#[cfg(windows)]
pub fn spawn_contained(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    contained::spawn(command)
}

/// The Windows half of `spawn_contained`: a case's job, and a process started suspended so it is in the job
/// before its first instruction runs.
///
/// **A process started and then assigned to a job can start a child first.** So it starts suspended, is
/// assigned, and only then has its threads resumed; the child it starts is in the job from its beginning.
/// `Ending`'s drop ends the job with `TerminateJobObject`, which ends every process in it at once.
#[cfg(windows)]
mod contained {
    use std::cell::RefCell;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    thread_local! {
        /// The job of the case this thread is running, if one holds an `Ending`
        static CURRENT: RefCell<Option<usize>> = const { RefCell::new(None) };
    }

    /// A job for one case, ended when the case's `Ending` is dropped.
    pub struct CaseJob(HANDLE);

    impl CaseJob {
        pub fn new() -> Self {
            // SAFETY: plain calls on handles this function creates and owns
            unsafe {
                let job = CreateJobObjectW(null(), null());
                assert!(!job.is_null(), "a job for the case is made");
                let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                assert!(set != 0, "the job ends with the case");
                CURRENT.with(|current| *current.borrow_mut() = Some(job as usize));
                CaseJob(job)
            }
        }
    }

    impl Drop for CaseJob {
        fn drop(&mut self) {
            CURRENT.with(|current| *current.borrow_mut() = None);
            // SAFETY: the handle is this job's, and is closed once, here
            unsafe {
                TerminateJobObject(self.0, 1);
            }
            // Termination is asked for, and the processes end a moment later: the case's broker is let go only
            // once the job holds none (the POSIX watcher is waited for in the same way), counted rather than timed
            let waiting_since = std::time::Instant::now();
            while active_processes(self.0) > 0 && waiting_since.elapsed() < std::time::Duration::from_secs(10) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // SAFETY: as above
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// The processes the job holds. A query that fails reports none, which ends the wait early rather than
    /// holding the case for a job that cannot be read.
    fn active_processes(job: HANDLE) -> u32 {
        // SAFETY: a query into a local the size of its record
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            let mut returned = 0u32;
            let ok = QueryInformationJobObject(
                job,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                &mut returned,
            );
            if ok == 0 { 0 } else { info.ActiveProcesses }
        }
    }

    pub fn spawn(command: &mut Command) -> std::io::Result<Child> {
        let Some(job) = CURRENT.with(|current| *current.borrow()) else { return command.spawn() };
        command.creation_flags(CREATE_SUSPENDED);
        let child = command.spawn()?;
        // SAFETY: the process handle is the child's, live while `child` is
        let assigned = unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) };
        assert!(assigned != 0, "the process joins the case's job");
        resume_threads(child.id());
        Ok(child)
    }

    /// Resumes every thread of `pid`: a suspended process starts with one, and only the threads of this process
    /// are named by its id.
    fn resume_threads(pid: u32) {
        // SAFETY: a snapshot this function takes and closes, and thread handles it opens and closes
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            assert!(snapshot != INVALID_HANDLE_VALUE, "the threads are listed");
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !thread.is_null() {
                        ResumeThread(thread);
                        CloseHandle(thread);
                    }
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
    }
}

impl Drop for Ending {
    fn drop(&mut self) {
        // Let go of, which is what the watcher is waiting for, and waited for: when this returns the
        // broker has been ended, so a directory removed next is one nothing is still using
        #[cfg(unix)]
        if let Some((mut watcher, held)) = self.watcher.take() {
            drop(held);
            let _ = watcher.wait();
        }
    }
}

impl Reaper {
    /// Stops the watcher, **before** the broker is stopped: while its pipe is still held, so it has not
    /// acted and will not.
    pub fn stand_down(&mut self) {
        #[cfg(unix)]
        if let Some((mut watcher, held)) = self.watcher.take() {
            let _ = watcher.kill();
            let _ = watcher.wait();
            drop(held);
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        self.stand_down();
    }
}
