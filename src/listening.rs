//! One broker a socket, on a socket only its user reaches.
//!
//! **A socket is a name, and a name can be taken twice.** A broker that binds a path somebody else is
//! already listening on replaces the file and leaves the first broker running with nothing able to reach
//! it — and a broker with kept sessions is then holding shells for ever. So before it touches the socket
//! a broker takes a lock beside it and keeps it for its life: the one that holds the lock is the broker
//! for that socket, a socket file found there by its holder is one a dead broker left, and a second
//! broker is told so and exits (`HELD_EXIT`) without having touched anything.
//!
//! **And the path is the caller's to choose, so the broker says where it will not listen.** What crosses
//! the socket is a user's terminals — everything typed and printed in them, and the means to type more.
//! On a Unix system who may connect is the file's mode and the directory's, so the socket is made for its
//! owner alone, in a directory that only its owner can put files in.

use std::io;
use std::path::Path;
use std::time::Duration;

use interprocess::local_socket::{GenericFilePath, Listener, ListenerOptions, Stream, ToFsName};

#[cfg(windows)]
mod pipe;

/// What a broker exits with when another broker holds the socket it was asked to listen on.
///
/// A number of its own, so whatever started it can tell "there is already one" — which is not a failure,
/// and means connect to that one — from every way of failing to start.
pub const HELD_EXIT: i32 = 3;

/// How long a broker nothing has connected to waits before deciding nothing will.
///
/// **A point in time, and deliberately the clock's.** Every other reason a broker has to exit is an
/// event — a connection closing, a session ending — and is acted on where it happens. A broker that was
/// started and then never connected to has no event coming: whatever started it died first, and the only
/// thing that can end the wait is that enough time has passed. It measures nothing about this machine's
/// load, so a busy machine cannot make it wrong: whatever starts a broker connects as soon as the broker
/// says it is listening, which is far inside this however slowly either runs.
pub const START_GRACE: Duration = Duration::from_secs(10);

/// The variable a broker is told a grace other than `START_GRACE` by, in milliseconds.
///
/// **For a case whose subject is the wait.** That a broker nothing connects to leaves when the grace is
/// over, and not before, is the same claim at three hundred milliseconds as at ten seconds; a suite
/// that waited out the real one twice was two thirds of its own length, and every mutant a run of it
/// tests pays that again. Nothing that starts a broker for use sets it.
pub const START_GRACE_ENV: &str = "APACK_TERMINAL_START_GRACE_MS";

/// The grace a broker was started with: what `START_GRACE_ENV` holds when it is a number of
/// milliseconds, and `START_GRACE` when it is unset or anything else.
pub fn start_grace(asked: Option<&str>) -> Duration {
    asked.and_then(|asked| asked.parse().ok()).map_or(START_GRACE, Duration::from_millis)
}

/// Where a broker is when nobody says where: `$XDG_RUNTIME_DIR/apack-terminal/broker.sock` on a system
/// that gives each user a runtime directory, and `~/.apack-terminal/broker.sock` otherwise. `None` with
/// neither that nor a home directory.
///
/// **So that a proxy needs no path.** A proxy is run by whatever carries it — `ssh host`, `docker exec` —
/// on a machine nobody has set anything up on, where the one directory known to be the user's own is the
/// one the system made for them. The directory is made for its owner alone (`private_directory`).
pub fn default_socket() -> Option<String> {
    let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let directory = match set("XDG_RUNTIME_DIR") {
        Some(runtime) => Path::new(&runtime).join("apack-terminal"),
        None => Path::new(&set("HOME")?).join(".apack-terminal"),
    };
    Some(directory.join("broker.sock").to_string_lossy().into_owned())
}

/// The hold on a socket that makes this process its broker, for as long as the value lives.
pub struct Held {
    #[cfg(unix)]
    _lock: std::fs::File,
}

/// Why a broker may not listen on a socket.
#[derive(Debug)]
pub enum Refused {
    /// Another broker holds it
    Held,
    /// Something about the path, said as a sentence
    Unsafe(String),
    Failed(io::Error),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Held => write!(out, "another broker is already listening there"),
            Self::Unsafe(why) => write!(out, "{why}"),
            Self::Failed(error) => write!(out, "{error}"),
        }
    }
}

impl From<io::Error> for Refused {
    fn from(error: io::Error) -> Self {
        Self::Failed(error)
    }
}

/// Where the lock for a socket is: beside it, under its name.
#[cfg(unix)]
fn lock_file(socket: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{socket}.lock"))
}

/// Takes the hold on `socket`, or says another broker has it.
///
/// **An advisory lock on a file of its own**, which the kernel lets go of however the process ends: there
/// is no stale hold to detect and nothing to clean up, and the file is never removed — removing it would
/// let two brokers each lock a different file of the same name.
///
/// **Windows needs no file.** A named pipe is created there as the first instance of its name or not at
/// all (`interprocess` asks for exactly that), so the listener itself is the hold, and `listen` reports
/// the name already taken.
pub fn hold(socket: &str) -> Result<Held, Refused> {
    #[cfg(not(unix))]
    {
        let _ = socket;
        Ok(Held {})
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let lock =
            std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(lock_file(socket))?;
        // SAFETY: `flock` takes a descriptor this function owns and two flags, and answers through errno
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Held { _lock: lock });
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock { Err(Refused::Held) } else { Err(Refused::Failed(error)) }
    }
}

/// Makes the directory a socket goes in, where it is missing, for its owner alone; and refuses a
/// directory that is there and is not this user's to keep others out of.
///
/// **Owned by this user and writable by nobody else.** In a directory another user can write to, the
/// socket can be renamed away and one of theirs put in its place, and whatever connects next is typing
/// into their process. That rules out a shared directory like `/tmp` even though its sticky bit stops
/// the rename: the directory is not this user's, and what it allows is whatever its owner next decides.
///
/// **A bare name is a socket in the directory this process was started in**, and that directory is asked
/// the same question as any other.
pub fn private_directory(socket: &str) -> Result<(), Refused> {
    let directory = match Path::new(socket).parent() {
        Some(directory) if !directory.as_os_str().is_empty() => directory,
        _ => Path::new("."),
    };
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory)?;
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};

        if !directory.exists() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
        }
        let found = std::fs::metadata(directory)?;
        // SAFETY: `geteuid` takes nothing and cannot fail
        let user = unsafe { libc::geteuid() };
        if found.uid() != user {
            return Err(Refused::Unsafe(format!(
                "{} belongs to another user, who could replace a socket put there",
                directory.display()
            )));
        }
        if found.mode() & 0o022 != 0 {
            return Err(Refused::Unsafe(format!(
                "{} can be written to by other users, who could replace a socket put there",
                directory.display()
            )));
        }
        Ok(())
    }
}

/// Listens on `socket`, which this process holds (`hold`), replacing a socket file a dead broker left.
///
/// **Created for its owner alone, from the moment it exists.** A socket file gets its mode from the
/// process's umask when it is bound; narrowed afterwards it would be open to anyone for the time between.
/// The umask is the whole process's, so this is called before the broker has started a thread or a shell
/// that could create a file meanwhile.
pub fn listen(socket: &str) -> Result<Listener, Refused> {
    let name = socket.to_fs_name::<GenericFilePath>()?;
    // `try_overwrite` replaces the file a previous broker left behind, which would otherwise refuse the
    // bind. That it is a leftover and not a live broker's is what holding the lock established
    let options = ListenerOptions::new().name(name).try_overwrite(true);
    // **On Windows the pipe is created with a descriptor of its own**, because the default one grants read
    // access to Everyone and what crosses here is the user's terminal output (`security.rs`). A descriptor
    // that cannot be built refuses the listen: the alternative is serving every local account a terminal
    // because a token could not be read
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        options.security_descriptor(crate::security::owner_only()?)
    };
    // SAFETY: `umask` takes a mode and answers the one it replaced; it cannot fail
    #[cfg(unix)]
    let before = unsafe { libc::umask(0o177) };
    let listener = options.create_sync();
    // SAFETY: as above
    #[cfg(unix)]
    unsafe {
        libc::umask(before)
    };
    match listener {
        Ok(listener) => Ok(listener),
        // The pipe's name is taken, which on Windows is the only way a second broker finds out
        #[cfg(windows)]
        Err(error) if matches!(error.kind(), io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied) => {
            Err(Refused::Held)
        }
        Err(error) => Err(Refused::Failed(error)),
    }
}

/// Whether what is on the other end of `stream` runs as the user this process does.
///
/// **Asked of the kernel, about the connection.** A proxy carries whatever it is given to whatever it
/// connected to, and a path only says where it connected, not to whom: a socket somebody else left at
/// that path would be handed a user's keystrokes. The kernel records who made each end of a connection,
/// and that cannot be dressed up.
///
/// **Windows answers yes without asking**: the pipe's own descriptor admits this user alone
/// (`security.rs`), so a connection that was made is one to this user's broker.
pub fn peer_is_this_user(stream: &Stream) -> io::Result<bool> {
    #[cfg(not(unix))]
    {
        let _ = stream;
        Ok(true)
    }
    #[cfg(unix)]
    {
        use interprocess::local_socket::traits::StreamCommon as _;

        // SAFETY: `geteuid` takes nothing and cannot fail
        let user = unsafe { libc::geteuid() };
        Ok(stream.peer_creds()?.euid() == Some(user))
    }
}

/// Closes a connection in both directions, whoever else still holds it, so a thread parked in a read or
/// a write of it is let go.
///
/// **A named pipe is hung up by cancelling its pending I/O** (`CancelIoEx` on the handle). A read or a write
/// parked on a synchronous handle ends with `ERROR_OPERATION_ABORTED`, as a read parked on an overlapped one
/// does. Disconnecting the pipe does not do this for a read parked on the server end: a synchronous call on
/// that handle waits behind the pending read, so the disconnect would wait too. Closing the handle does not
/// either. The pipe stays open after this, for the far end to close.
///
/// **The half the far end has not already closed is closed by itself where both cannot be.** A caller
/// that has said all it will has closed its sending side, and on macOS a socket in that state refuses to
/// be shut down in both directions at all (`ENOTCONN`), leaving the direction this process writes in open
/// and whatever is writing to it parked for as long as the far end likes. That direction is the one a
/// hang-up is for.
pub fn hang_up(stream: &Stream) -> bool {
    match stream {
        #[cfg(unix)]
        Stream::UdSocket(socket) => {
            let socket = socket.inner();
            socket.shutdown(std::net::Shutdown::Both).is_ok() || socket.shutdown(std::net::Shutdown::Write).is_ok()
        }
        #[cfg(windows)]
        Stream::NamedPipe(pipe) => pipe::hang_up(pipe),
    }
}
