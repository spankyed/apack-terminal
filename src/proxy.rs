//! `apack-terminal proxy <socket>`: this process's stdin and stdout, as a connection to the broker on
//! that socket — started first, if there is none.
//!
//! **This is what puts a broker anywhere a command can be run.** A broker listens on a socket, which only
//! something on the same machine can reach. Run under `ssh host`, `docker exec -i` or anything else that
//! carries a process's input and output, this turns that into a connection: whatever is written to its
//! stdin the broker reads, and what the broker says comes back on its stdout, byte for byte. So the app
//! speaks to a broker on another machine exactly as it speaks to its own, in whichever role it greets
//! with, and nothing about a remote terminal is a second protocol.
//!
//! **One broker per socket, however many proxies arrive.** The first finds nothing listening and starts
//! one; the rest connect to it. A broker started here is a process of its own — it has to outlive this
//! one, whose life is one connection — and ends by the broker's own rule, when nothing is connected to it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

/// How long a broker another proxy is starting is waited for before this one gives up on it.
const START_WAIT: Duration = Duration::from_secs(10);

fn connect(socket: &str) -> std::io::Result<Stream> {
    Stream::connect(socket.to_fs_name::<GenericFilePath>()?)
}

/// Where the claim to be the one starting a broker for `socket` is made.
fn starting_file(socket: &str) -> PathBuf {
    PathBuf::from(format!("{socket}.starting"))
}

/// Starts a broker on `socket` as a process of its own, answering once it has said it is listening.
fn start_broker(socket: &str) -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg(socket).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    // **A session of its own**, so the broker is not ended with whatever carried this proxy: an `ssh`
    // connection closing sends its hang-up to the session it started, and the broker's life is its own
    // connections', not this one's
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: runs in the child between fork and exec, and `setsid` touches nothing this process owns
        unsafe {
            command.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) })
        };
    }
    let mut broker = command.spawn()?;
    let mut ready = String::new();
    let said = broker.stdout.take().ok_or_else(|| std::io::Error::other("the broker has no stdout to read"))?;
    BufReader::new(said).read_line(&mut ready)?;
    if ready.starts_with("listening ") {
        return Ok(());
    }
    Err(std::io::Error::other(format!("the broker did not say it was listening: {ready:?}")))
}

/// A connection to the broker on `socket`, starting one when nothing is listening there.
///
/// **Two proxies arriving together must not both start one**: the broker takes its socket whether or not
/// another holds it, so the second would leave the first one's sessions unreachable. The one that creates
/// the claim file starts the broker; any other waits for it to be listening.
///
/// **Holding the claim is not yet a reason to start one.** A proxy can find nothing listening, and take the
/// claim only after another has started a broker and let it go — so the claim's holder asks once more, and
/// a broker is started only by a holder that still finds none. A broker is listening before its starter
/// releases the claim, which is what makes that second look decisive.
/// The window that second look closes is a few microseconds wide and no test reaches it; what the suite
/// holds is the claim itself, without which proxies arriving together fail every time.
fn reach(socket: &str) -> std::io::Result<Stream> {
    let claim = starting_file(socket);
    let mut waiting_since = Instant::now();
    let mut took_over = false;
    loop {
        if let Ok(stream) = connect(socket) {
            return Ok(stream);
        }
        if std::fs::OpenOptions::new().write(true).create_new(true).open(&claim).is_ok() {
            let reached = connect(socket).or_else(|_| start_broker(socket).and_then(|()| connect(socket)));
            let _ = std::fs::remove_file(&claim);
            return reached;
        }
        // Another proxy is starting it. A claim outlives a proxy that died holding it, so one that has had
        // its time is removed and contended for again — once, since a second that goes nowhere is not stale
        if waiting_since.elapsed() > START_WAIT {
            if took_over {
                return Err(std::io::Error::other(format!(
                    "nothing started listening on {socket}, and {} is held by something that is not starting it",
                    claim.display()
                )));
            }
            let _ = std::fs::remove_file(&claim);
            took_over = true;
            waiting_since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Copies everything from `from` to `to`, a read at a time, until either ends.
fn pump(mut from: impl Read, mut to: impl Write) {
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match from.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if to.write_all(&buffer[..read]).and_then(|()| to.flush()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Runs as the proxy: until this process's stdin ends or the broker hangs up.
pub fn run(socket: &str) -> std::io::Result<()> {
    if let Some(directory) = Path::new(socket).parent().filter(|directory| !directory.as_os_str().is_empty()) {
        std::fs::create_dir_all(directory)?;
    }
    let stream = Arc::new(reach(socket)?);
    // **Whichever side ends first ends the process**, which is what closes the other: the broker hanging
    // up leaves nothing to read, and stdin ending is the far end having gone. A thread parked on the other
    // read is no reason to stay
    std::thread::spawn({
        let stream = Arc::clone(&stream);
        move || {
            pump(std::io::stdin().lock(), &*stream);
            std::process::exit(0);
        }
    });
    pump(&*stream, std::io::stdout().lock());
    Ok(())
}
