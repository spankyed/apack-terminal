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
