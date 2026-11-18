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

/// A watcher started for a broker that does not exist yet, waiting to be told which process it is (`of`).
pub struct Watching {
    #[cfg(unix)]
    watcher: (Child, ChildStdin),
}

/// Starts a watcher for a broker that is the program at `program`, **before that broker is started**.
///
/// **A broker gives whatever started it `START_GRACE` from its ready line to connect, and leaves if
/// nothing has** (`listening::START_GRACE`). Starting a process is the slowest thing a case does: started
/// between that line and the connect, at a load of three to four hundred, the watcher took up to 5.5 s of
/// the ten (2026-11-17), and cases whose first connect was refused had each spent over eleven seconds by
/// then. So the watcher is started first, and what is left between the line and the connect is a read.
///
/// **The watcher kills by pid, and only a pid that is still that program.** A pid is a number the system
/// hands out again; one that no longer names the broker's executable is somebody else's process, and is
/// left alone. A single process, never a group: the broker's sessions end when it does, their terminals
/// having closed. A watcher never told a pid, its case having ended first, has nothing to name and kills
/// nothing.
#[cfg(unix)]
pub fn watching(program: &str) -> Watching {
    use std::os::unix::process::CommandExt;

    required_by_the_watcher(&["ps"]);
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(r#"read pid || exit; read _; case "$(ps -o command= -p "$pid")" in "$1"*) kill -KILL "$pid" ;; esac"#)
        .arg("sh")
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
    Watching { watcher: (watcher, held) }
}

impl Watching {
    /// Tells the watcher which process its broker is, and holds it for as long as the case does.
    #[cfg(unix)]
    pub fn of(self, broker: &Child) -> Reaper {
        use std::io::Write as _;

        let pid = broker.id();
        assert!(pid > 1, "a broker's pid is its own: {pid}");
        let (watcher, mut held) = self.watcher;
        writeln!(held, "{pid}").expect("the watcher is told its broker's pid");
        Reaper { watcher: Some((watcher, held)) }
    }

    #[cfg(not(unix))]
    pub fn of(self, _broker: &std::process::Child) -> Reaper {
        Reaper {}
    }
}

/// Refuses a machine without one of `tools`, which a watcher's script runs.
///
/// **A watcher that cannot run its tools ends nothing, and says nothing.** It is a shell whose output goes
/// nowhere, so `lsof` not found reads as no broker holding the lock, and `ps` not found as a pid that is no
/// longer the broker's: either way the broker it was there for is left running after the case, and only a
/// case that asserts the broker went (`proxy::a_broker_a_proxy_started_is_ended_with_the_case_that_had_it_started`)
/// would notice. So the case fails here, naming what to install, before anything is started.
///
/// **Looked for on `PATH` by this process, as the watcher's shell would find it, rather than by asking a
/// shell.** This runs for every broker a case starts, and on a loaded machine each process started is
/// a measurable share of a case's deadline: two shells per watcher were seconds of it at a load of a
/// hundred and more.
#[cfg(unix)]
fn required_by_the_watcher(tools: &[&str]) {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::var_os("PATH").unwrap_or_default();
    for tool in tools {
        let found = std::env::split_paths(&path).any(|directory| {
            std::fs::metadata(directory.join(tool))
                .is_ok_and(|found| found.is_file() && found.permissions().mode() & 0o111 != 0)
        });
        assert!(
            found,
            "`{tool}` is not on this machine's PATH: the watcher that ends a case's broker runs it, and without it leaves the broker running"
        );
    }
}

#[cfg(not(unix))]
pub fn watching(_program: &str) -> Watching {
    // A session's processes are in a job that ends with its broker (`src/tree.rs`), and a broker a
    // case leaves is ended by its own rules; there is no pipe to a shell to hold here
    Watching {}
}

/// What ends the broker on a socket when the case that had one started is over, however it ended.
///
/// **For a broker the case did not start and has no pid for.** A proxy starts a broker where none is
/// listening, in a session of its own, and says nothing of it but that it is there. Such a broker goes
/// by its own rules once its connections have, and a mutant of those rules does not.
pub struct Ending {
    #[cfg(unix)]
    watcher: Option<(Child, ChildStdin)>,
    /// The socket whose broker the watcher is for
    #[cfg(unix)]
    socket: std::path::PathBuf,
    /// Held for what dropping it does: the job, and every process in it, ends
    #[cfg(windows)]
    _job: Option<contained::CaseJob>,
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
/// now. A system without `lsof` is refused when the watch begins (`required_by_the_watcher`).
///
/// **The watcher stays until the broker has gone, and not for ever.** A kill is asked for and happens
/// a moment later, and what follows a case — its directory removed, the next case's broker on a socket
/// of the same name — is owed a broker that is no longer holding anything. A process its parent has not
/// collected still answers to its pid, so the wait is counted: five seconds, then it is left.
///
/// **It acts at every ending where a broker holds the socket, a clean one included.** A case that fails
/// drops what it holds as one that passes does, and a broker that will not go by itself is as much left
/// behind by either.
///
/// **At an ending the case reaches, what is waited for is the lock, not the watcher.** A broker lets go of
/// its lock as it exits, so the lock is the event that says it is gone, whether it went by its own rules
/// or was killed: the case takes it, blocking, while the watcher looks for a broker that will not go. Where
/// nothing holds it there is no broker, and the watcher is stood down before it looks. `lsof` reads every
/// process on the machine, which is a second and more on a loaded one, and a case's broker is almost
/// always on its way out by itself once the case's connections have closed — so a case no longer waits
/// out a search that, by the time it answers, has nothing to find. A broker that neither goes nor is ended
/// holds the case until the runner's deadline, which reports it.
#[cfg(unix)]
pub fn ending_with_the_case(socket: &std::path::Path, program: &str) -> Ending {
    use std::os::unix::process::CommandExt;

    required_by_the_watcher(&["lsof", "ps"]);
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
    Ending { watcher: Some((watcher, held)), socket: socket.to_owned() }
}

/// The lock beside `socket`, open, and `None` where there is none: nothing was started on it, or its directory
/// was never made.
#[cfg(unix)]
fn lock_beside(socket: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    let mut lock = socket.as_os_str().to_owned();
    lock.push(".lock");
    match std::fs::File::open(lock) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Takes `lock` as `operation` asks (`flock`), answering whether it was taken.
#[cfg(unix)]
fn taken(lock: &std::fs::File, operation: libc::c_int) -> bool {
    use std::os::fd::AsRawFd;

    loop {
        // SAFETY: `flock` takes a descriptor `lock` owns and two flags, and answers through errno
        if unsafe { libc::flock(lock.as_raw_fd(), operation) } == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// On Windows the case's processes are in a job of its own, which the case's `Ending` ends when it is
/// dropped. A process started with `spawn_contained` while this is held is put in the job before it runs,
/// and so is the broker it starts: a job's processes inherit into their children, and nothing here breaks
/// away. A killed case ends its job when the test process ends, the handle closing with it.
#[cfg(windows)]
pub fn ending_with_the_case(_socket: &std::path::Path, _program: &str) -> Ending {
    Ending { _job: Some(contained::CaseJob::new()) }
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
        // When this returns the broker has gone, so a directory removed next is one nothing is still using
        // (`ending_with_the_case` on why the lock is what is waited for)
        #[cfg(unix)]
        if let Some((mut watcher, held)) = self.watcher.take() {
            let gone = match lock_beside(&self.socket) {
                Ok(None) => true,
                Ok(Some(lock)) if taken(&lock, libc::LOCK_EX | libc::LOCK_NB) => true,
                // A broker holds it: the watcher is let go of, to end one that will not go by itself, and the
                // lock is taken once the broker has gone either way. The case's own hold on the file is one the
                // watcher passes over, the case not being `program`
                Ok(Some(lock)) => {
                    drop(held);
                    taken(&lock, libc::LOCK_EX)
                }
                // A lock that cannot be read or waited on is left to the watcher, which is waited for
                Err(_) => {
                    drop(held);
                    false
                }
            };
            // Stood down as a `Reaper` is: the broker is gone, and a search the watcher began finishes by
            // itself, reading only
            if gone {
                let _ = watcher.kill();
            }
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
