//! One pty and the process in it: the process layer, with nothing that knows what a screen is.
//!
//! The split the goal asks for — a process layer under a terminal capability — shows up here as what this
//! file does *not* do: it moves bytes and owns a lifetime, and `cols`/`rows` pass through to the pty because
//! that is the pty's own shape, not because this layer models a screen.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::protocol::{ErrorCode, Event, SessionId};
use crate::output::{Credit, Outgoing, coalesce};

/// A running session: the master side of its pty, its child, and the thread reading its output.
struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// Shared with the reader thread, which waits on it at EOF so **nothing polls for an exit**: the pty
    /// closing *is* the process ending, and that is an event this already receives.
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
}

/// Every session this broker owns, and where their output goes.
pub struct Sessions {
    next: u64,
    live: HashMap<SessionId, Session>,
    /// Answers and reports: no credit, so none of them waits behind output
    events: Sender<Outgoing>,
    /// What output spends, which is the channel's bound
    credit: Arc<Credit>,
}

/// A failure a request turns into a `Failed` event rather than a panic.
pub struct Refusal {
    pub code: ErrorCode,
    pub message: String,
}

impl Refusal {
    fn pty(what: &str, error: impl std::fmt::Display) -> Self {
        Self { code: ErrorCode::Pty, message: format!("{what}: {error}") }
    }
}

type Outcome = Result<(), Refusal>;

impl Sessions {
    pub fn new(events: Sender<Outgoing>, credit: Arc<Credit>) -> Self {
        Self { next: 0, live: HashMap::new(), events, credit }
    }

    /// Answers this connection, which a caller holding the lock can do without taking a second one.
    pub fn answer(&self, event: Event) {
        let _ = self.events.send(Outgoing::answer(event));
    }

    /// Starts a shell in a new pty and begins reading it. The id is the broker's to mint.
    pub fn spawn(
        &mut self,
        cols: u16,
        rows: u16,
        shell: Option<String>,
        cwd: Option<String>,
    ) -> Result<(SessionId, u32), Refusal> {
        let pty = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| Refusal::pty("opening a pty", error))?;

        let mut command = match shell {
            Some(program) => CommandBuilder::new(program),
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(directory) = cwd {
            command.cwd(directory);
        }
        let child = pty
            .slave
            .spawn_command(command)
            .map_err(|error| Refusal::pty("spawning a shell", error))?;
        // **The slave is dropped here on purpose**: holding it keeps its fd open, so the master never sees
        // EOF and the reader below would block for ever after the process exits.
        drop(pty.slave);

        let reader = pty
            .master
            .try_clone_reader()
            .map_err(|error| Refusal::pty("reading a pty", error))?;
        let writer = pty
            .master
            .take_writer()
            .map_err(|error| Refusal::pty("writing to a pty", error))?;

        self.next += 1;
        let session = format!("s-{}", self.next);
        let pid = child.process_id().unwrap_or_default();
        let child = Arc::new(Mutex::new(child));

        // One thread per session, which is what the skeleton needs; bounded I/O and backpressure are
        // Phase B's, and the goal says so rather than leaving it to be discovered here.
        // **The reader does not decide what a message is.** It hands what one `read` returned to the
        // coalescer, which flushes on a window or a cap — so N bytes of output no longer means N messages,
        // which is F4. The internal hop is unbounded on purpose: the bound belongs at the coalescer, where
        // credit is spent, and a reader blocked on this queue could not notice EOF.
        let (chunks, incoming) = channel::<Vec<u8>>();
        let events = self.events.clone();
        let reading = session.clone();
        let waiting = Arc::clone(&child);
        std::thread::spawn({
            let session = session.clone();
            let events = events.clone();
            let credit = Arc::clone(&self.credit);
            move || coalesce(session, incoming, events, credit)
        });
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if chunks.send(buffer[..read].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
            // The coalescer ends when this drops, flushing what it holds, so nothing is lost at EOF
            drop(chunks);
            // EOF on the master: the process has ended, so its status is available without waiting for a
            // tick. A `kill` may have taken it already, in which case this reports nothing.
            let code = waiting.lock().ok().and_then(|mut child| child.wait().ok()).map(|status| status.exit_code() as i32);
            let _ = events.send(Outgoing::answer(Event::Exited { session: reading, code }));
        });

        self.live.insert(session.clone(), Session { master: pty.master, writer, child });
        Ok((session, pid))
    }

    pub fn write(&mut self, session: &str, data: &str) -> Outcome {
        let live = self.live.get_mut(session).ok_or_else(|| missing(session))?;
        live.writer
            .write_all(data.as_bytes())
            .and_then(|()| live.writer.flush())
            .map_err(|error| Refusal::pty("writing to a pty", error))
    }

    pub fn resize(&mut self, session: &str, cols: u16, rows: u16) -> Outcome {
        let live = self.live.get_mut(session).ok_or_else(|| missing(session))?;
        live.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| Refusal::pty("resizing a pty", error))
    }

    /// Ends the session. The reader thread sees the pty close and reports `Exited`, so this reports nothing:
    /// one exit, one event, whichever way the process ended.
    pub fn kill(&mut self, session: &str) -> Outcome {
        let live = self.live.remove(session).ok_or_else(|| missing(session))?;
        let mut child = live.child.lock().map_err(|_| Refusal::pty("killing a session", "the child's lock was poisoned"))?;
        child.kill().map_err(|error| Refusal::pty("killing a session", error))
    }

    /// Ends every session this owner holds. **The connection's end is what calls it**: a client that
    /// disconnects must not leave its shells running, and the app cannot do it on the way out — a pack's
    /// `onShutdown` is synchronous and runs in `process.on('exit')`, where no socket write can complete. So
    /// the broker is the backstop for an app that quits, crashes or is killed, and the app's own `killAll`
    /// is only the orderly path.
    ///
    /// Sessions outliving their client is Phase B's, and nothing here is shaped to prevent it: this is where
    /// that policy will be stated instead of assumed.
    pub fn end_all(&mut self) {
        for (_, live) in self.live.drain() {
            if let Ok(mut child) = live.child.lock() {
                let _ = child.kill();
            }
        }
    }
}

fn missing(session: &str) -> Refusal {
    Refusal { code: ErrorCode::NoSuchSession, message: format!("no session {session}") }
}
