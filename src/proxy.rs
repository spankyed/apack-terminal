//! `apack-terminal proxy [<socket>]`: this process's stdin and stdout, as a connection to the broker on
//! that socket — started first, if there is none. With no socket named, the user's own
//! (`listening::default_socket`).
//!
//! **This is what puts a broker anywhere a command can be run.** A broker listens on a socket, which only
//! something on the same machine can reach. Run under `ssh host`, `docker exec -i` or anything else that
//! carries a process's input and output, this turns that into a connection: whatever is written to its
//! stdin the broker reads, and what the broker says comes back on its stdout, byte for byte. So the app
//! speaks to a broker on another machine exactly as it speaks to its own, in whichever role it greets
//! with, and nothing about a remote terminal is a second protocol.
//!
//! **One broker per socket, however many proxies arrive, and that is the broker's doing.** A broker holds
//! a lock on its socket for its life and one that cannot take it exits saying so (`listening`). So a proxy
//! that finds nothing listening simply starts a broker: if another proxy's broker got there first, this
//! one's exits at once, and the proxy connects to the one that is there. A broker started here is a
//! process of its own — it has to outlive this one, whose life is one connection — and ends by the
//! broker's own rules: when nothing is connected to it, or when nothing ever connects.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use crate::listening::{self, HELD_EXIT};

/// How many times a proxy looks for a broker before giving up on there being one.
///
/// Each look that finds none starts a broker, so this is reached only when brokers keep starting and
/// none ends up listening — which is something wrong with the machine, not a race to be waited out.
const LOOKS: usize = 50;

/// How long a proxy leaves a broker that beat its own to the socket to finish starting to listen.
///
/// **A pause, because there is nothing to wait on.** The broker that holds the socket was started by
/// some other process and says it is listening on a pipe this one does not have; from here the only
/// sign of it is that a connection is accepted. It took the lock a moment before it listens, so the
/// pause is that moment several times over, and `LOOKS` of them is the bound.
const BETWEEN_LOOKS: Duration = Duration::from_millis(20);

fn connect(socket: &str) -> std::io::Result<Stream> {
    Stream::connect(socket.to_fs_name::<GenericFilePath>()?)
}

/// What became of a broker this proxy started.
enum Started {
    /// It is listening
    Listening,
    /// Another broker holds the socket, and this one has exited
    Beaten,
}

/// Starts a broker on `socket` as a process of its own, answering once it has said what became of it.
///
/// **The broker says so on its stdout, in one line**: `listening <socket>`, or `failed <why>` before it
/// exits. Its stderr goes nowhere: a broker outlives the proxy that started it, and a pipe to a process
/// that has gone is one more thing a long-lived broker could be killed by writing to.
fn start_broker(socket: &str) -> std::io::Result<Started> {
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
    let mut said = String::new();
    let stdout = broker.stdout.take().ok_or_else(|| std::io::Error::other("the broker has no stdout to read"))?;
    BufReader::new(stdout).read_line(&mut said)?;
    if said.starts_with("listening ") {
        return Ok(Started::Listening);
    }
    // It said why it is not listening, or nothing at all, and either way it is on its way out
    let status = broker.wait()?;
    if status.code() == Some(HELD_EXIT) {
        return Ok(Started::Beaten);
    }
    let why = said.trim().strip_prefix("failed ").unwrap_or(said.trim());
    Err(std::io::Error::other(if why.is_empty() {
        format!("the broker started for {socket} exited without listening ({status})")
    } else {
        format!("the broker started for {socket} could not listen: {why}")
    }))
}

/// A connection to the broker on `socket`, starting one when nothing is listening there.
fn reach(socket: &str) -> std::io::Result<Stream> {
    for _ in 0..LOOKS {
        if let Ok(stream) = connect(socket) {
            return Ok(stream);
        }
        match start_broker(socket)? {
            Started::Listening => {}
            Started::Beaten => std::thread::sleep(BETWEEN_LOOKS),
        }
    }
    Err(std::io::Error::other(format!("nothing started listening on {socket}")))
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

/// Says nothing more will be sent on `stream`, leaving what the broker still has to say to be read.
/// `false` where a connection cannot be closed one way, which is a named pipe.
fn finish_sending(stream: &Stream) -> bool {
    match stream {
        #[cfg(unix)]
        Stream::UdSocket(socket) => socket.inner().shutdown(std::net::Shutdown::Write).is_ok(),
        #[cfg(windows)]
        Stream::NamedPipe(_) => false,
    }
}

/// Runs as the proxy: until the broker hangs up, which it does once this process's stdin has ended and
/// the broker has said everything it had to say.
pub fn run(socket: &str) -> std::io::Result<()> {
    listening::private_directory(socket).map_err(|refused| std::io::Error::other(refused.to_string()))?;
    let stream = Arc::new(reach(socket)?);
    // Before a byte is carried either way: what is about to be handed over is somebody's terminals
    if !listening::peer_is_this_user(&stream)? {
        return Err(std::io::Error::other(format!(
            "what is listening on {socket} is another user's, not a broker of this one's"
        )));
    }
    // **Stdin ending is the far end having said all it will, not the end of the conversation.** The
    // broker may still be answering what it was last asked, so the connection is closed in that one
    // direction and read until the broker closes the other; ending the process here would cut off the
    // answer to the request just carried. Where a connection has only one end to close, that is what
    // this still does
    std::thread::spawn({
        let stream = Arc::clone(&stream);
        move || {
            pump(std::io::stdin().lock(), &*stream);
            if !finish_sending(&stream) {
                std::process::exit(0);
            }
        }
    });
    // The broker hanging up leaves nothing to read, and a thread still parked on stdin is no reason to stay
    pump(&*stream, std::io::stdout().lock());
    Ok(())
}
