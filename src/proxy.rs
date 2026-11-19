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
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use crate::frames::MAX_FRAME_BYTES;
use crate::listening::{self, HELD_EXIT};
use crate::protocol::Request;

/// How many times a proxy looks for a broker before giving up on there being one.
///
/// Each look that finds none starts a broker, so this is reached only when brokers keep starting and
/// none ends up listening — which is something wrong with the machine, not a race to be waited out.
pub const LOOKS: usize = 50;

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

/// A connection to the broker on `socket` that is this user's own, starting a broker when none is there.
fn reach_own(socket: &str) -> std::io::Result<Stream> {
    let stream = reach(socket)?;
    // Before a byte is carried either way: what is about to be handed over is somebody's terminals
    if !listening::peer_is_this_user(&stream)? {
        return Err(std::io::Error::other(format!(
            "what is listening on {socket} is another user's, not a broker of this one's"
        )));
    }
    Ok(stream)
}

/// The most of what the far end says before the broker has answered any of it that is kept to be said
/// again (`Carrying`): one frame's ceiling. A greeting and whatever a caller sends behind it before hearing
/// back is a few frames; one that says more than this without waiting for a word is told why it cannot be
/// carried on, should the broker let it go unanswered.
pub const KEPT_BYTES: usize = MAX_FRAME_BYTES;

/// What the far end has said, and the connection it is carried on.
///
/// **A broker answers everything a connection says, beginning with its greeting**, so a connection it lets
/// go of without a word is one it never served. That is a broker that was leaving as this proxy reached it
/// — its last caller had gone, or nothing had reached it within its start grace — and took the connection
/// into its queue as it went. So what is said is kept until the broker has answered any of it, and said
/// again on the next connection. Nothing a broker acts on comes before its answer to a greeting, so nothing
/// said again has been acted on.
struct Carrying {
    /// Where what the far end says goes now
    stream: Arc<Stream>,
    kept: Kept,
    /// The far end has said something of its own, which the `done` said for it is not
    spoke: bool,
    /// The far end's input has ended, and `done` has been said for it
    ended: bool,
}

enum Kept {
    /// Said since the broker last answered, which it has not yet on this connection
    Unanswered(Vec<u8>),
    /// The broker has answered: the connection is the conversation, and its ending is the end
    Answered,
    /// More was said before an answer than `KEPT_BYTES`, so it cannot be said again
    TooLong,
}

/// Who is saying what is carried.
enum Saying {
    /// The far end, through this process's stdin
    FarEnd,
    /// This process, saying `done` for a far end whose input has ended
    Done,
}

fn carrying_of(carrying: &Mutex<Carrying>) -> MutexGuard<'_, Carrying> {
    carrying.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Carries `bytes` to the broker, keeping them while it has answered nothing.
///
/// **Written with the lock let go of**, to the connection there was when they were kept. Should that
/// connection be let go of without a word, everything kept is said again on the next before anything said
/// after it, so a write that fails here has lost nothing.
fn carry(carrying: &Mutex<Carrying>, bytes: &[u8], saying: Saying) {
    let stream = {
        let mut carrying = carrying_of(carrying);
        match saying {
            Saying::FarEnd => carrying.spoke = true,
            Saying::Done => carrying.ended = true,
        }
        if let Kept::Unanswered(kept) = &mut carrying.kept {
            if kept.len() + bytes.len() > KEPT_BYTES {
                carrying.kept = Kept::TooLong;
            } else {
                kept.extend_from_slice(bytes);
            }
        }
        Arc::clone(&carrying.stream)
    };
    let _ = (&*stream).write_all(bytes).and_then(|()| (&*stream).flush());
}

/// Copies what the broker says on `stream` to `to` until it lets the connection go or `to` refuses it,
/// answering whether the broker said anything at all.
fn heard(stream: &Stream, mut to: impl Write, carrying: &Mutex<Carrying>) -> bool {
    let mut buffer = [0u8; 64 * 1024];
    let mut answered = false;
    loop {
        match (&*stream).read(&mut buffer) {
            Ok(0) | Err(_) => return answered,
            Ok(read) => {
                if !answered {
                    answered = true;
                    carrying_of(carrying).kept = Kept::Answered;
                }
                if to.write_all(&buffer[..read]).and_then(|()| to.flush()).is_err() {
                    return answered;
                }
            }
        }
    }
}

/// Runs as the proxy: until the broker hangs up, which it does once this process's stdin has ended and
/// the broker has said everything it had to say.
pub fn run(socket: &str) -> std::io::Result<()> {
    listening::private_directory(socket).map_err(|refused| std::io::Error::other(refused.to_string()))?;
    let mut stream = Arc::new(reach_own(socket)?);
    let carrying = Arc::new(Mutex::new(Carrying {
        stream: Arc::clone(&stream),
        kept: Kept::Unanswered(Vec::new()),
        spoke: false,
        ended: false,
    }));
    // **Stdin ending is the far end having said all it will, not the end of the conversation.** The
    // broker may still be answering what it was last asked, so the caller says so with `Done` and goes on
    // reading until the broker lets the connection go; ending the process here would cut off the answer
    // to the request just carried. A named pipe cannot be closed one way, so this is the same on every
    // platform, rather than a half-close where there is one and an exit where there is not
    std::thread::spawn({
        let carrying = Arc::clone(&carrying);
        move || {
            let mut input = std::io::stdin().lock();
            let mut buffer = [0u8; 64 * 1024];
            while let Ok(read @ 1..) = input.read(&mut buffer) {
                carry(&carrying, &buffer[..read], Saying::FarEnd);
            }
            let done = serde_json::to_string(&Request::Done).expect("a request serializes");
            carry(&carrying, format!("{done}\n").as_bytes(), Saying::Done);
        }
    });
    // The broker hanging up leaves nothing to read, and a thread still parked on stdin is no reason to stay.
    // A connection let go of without a word was never served (`Carrying`), and is made again, as many times
    // as a broker is looked for
    let mut stdout = std::io::stdout().lock();
    let mut made = 1;
    while !heard(&stream, &mut stdout, &carrying) {
        // Held while the next is reached, so nothing the far end says goes anywhere until what it said
        // before has been said again
        let mut carrying = carrying_of(&carrying);
        let kept = match &carrying.kept {
            // A far end that said nothing and has finished is answered by the connection being let go
            _ if carrying.ended && !carrying.spoke => return Ok(()),
            Kept::Unanswered(kept) => kept.clone(),
            Kept::Answered | Kept::TooLong => {
                return Err(std::io::Error::other(format!(
                    "the broker on {socket} let the connection go without answering, and more had been said \
                     on it than the {KEPT_BYTES} bytes that are said again"
                )));
            }
        };
        if made == LOOKS {
            return Err(std::io::Error::other(format!(
                "the broker on {socket} let {LOOKS} connections go without answering"
            )));
        }
        stream = Arc::new(reach_own(socket)?);
        made += 1;
        let _ = (&*stream).write_all(&kept).and_then(|()| (&*stream).flush());
        carrying.stream = Arc::clone(&stream);
    }
    Ok(())
}
