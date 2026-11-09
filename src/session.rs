//! One pty and the process in it: the process layer, with nothing that knows what a screen is.
//!
//! The split the goal asks for — a process layer under a terminal capability — shows up here as what this
//! file does *not* do: it moves bytes and owns a lifetime, and `cols`/`rows` pass through to the pty because
//! that is the pty's own shape, not because this layer models a screen.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::output::{Credit, Outgoing, coalesce};
use crate::protocol::{ErrorCode, Event, Foreground, SessionId, Until, WaitOutcome};
use crate::screen::{Facts, HISTORY_LINES, Match, Screen, Snapshot};
use crate::tree::Tree;

/// A running session: the master side of its pty, its child, and the thread reading its output.
struct Session {
    master: Box<dyn MasterPty + Send>,
    /// Shared with the reader thread, which writes the terminal's own replies back through it — see
    /// `Replies` in `screen.rs` for why a session that answers nothing does not start on Windows
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// How a session is ended from here. **The child itself belongs to the thread waiting on it**, which is
    /// what lets that wait block: a killer is a handle of its own, so ending a session never queues behind
    /// the wait that will observe the ending.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The process tree this session owns, on the platform that needs one owned (`tree.rs`). Dropped with
    /// the session, which is what ends the tree on Windows — so a `kill`, a disconnect and a broker that
    /// crashes all reap it by the same mechanism rather than by three paths remembering to.
    _tree: Option<Tree>,
    /// Waits outstanding on this session, shared with the reader thread that settles them. **Checked where
    /// the bytes land**: the broker already knows when something arrived, so a caller waiting for a state
    /// costs a condition check per read rather than a timer asking.
    waiters: Arc<Mutex<Vec<Waiter>>>,
    /// The VT state, shared with the reader thread that feeds it. **It is fed before the bytes leave for the
    /// renderer**, so the authority is never behind the replica — the only direction in which a disagreement
    /// is merely stale rather than wrong.
    screen: Arc<Mutex<Screen>>,
}

/// One outstanding wait.
///
/// **`settled` is what makes the answer exactly one.** Three things race to end a wait — the condition
/// holding, the deadline passing and the session exiting — and a caller that was told `Met` must never also
/// be told `Timeout`. Whoever flips this first owns the answer.
struct Waiter {
    until: Until,
    settled: Arc<AtomicBool>,
}

impl Waiter {
    /// Whether the condition holds now. `Exit` is not asked here: nothing a screen holds says a process
    /// ended, so that one is settled by the thread that waits on the child.
    fn holds(&self, screen: &Screen) -> bool {
        match &self.until {
            Until::Idle => !screen.facts().running,
            // The whole history, because a caller waiting for text means text that has been written rather
            // than text still on screen. **The cost is confined to an outstanding wait**: with no `Text`
            // waiter nothing searches, so a session nobody is waiting on pays nothing per read.
            Until::Text { text } => !screen.search(text, HISTORY_LINES as u32).is_empty(),
            Until::Exit => false,
        }
    }
}

/// Settles the waits the screen now satisfies, answering each once. The count is what to report.
fn settle_met(waiters: &Mutex<Vec<Waiter>>, screen: &Screen) -> usize {
    let Ok(mut waiters) = waiters.lock() else {
        return 0;
    };
    let mut met = 0;
    waiters.retain(|waiter| {
        if waiter.settled.load(Ordering::SeqCst) {
            return false;
        }
        if waiter.holds(screen) {
            waiter.settled.store(true, Ordering::SeqCst);
            met += 1;
            return false;
        }
        true
    });
    met
}

/// The pty's foreground process group, and what that process is called.
///
/// **Two questions with a gap between them**, which is why the name is optional and the pid is not: the
/// answer describes the moment it was taken, and nothing holds the process still while it is taken. A
/// caller that sees a pid and no name has been told the truth.
///
/// **A fresh `System` per call rather than one kept in `Sessions`.** It is refreshed for one pid and asked
/// for nothing else, `Facts` is a request a caller makes when something happened rather than on a timer,
/// and a shared one would add a third lock to the order documented on `Sessions` in exchange for nothing.
fn foreground_of(master: &(dyn MasterPty + Send)) -> Option<Foreground> {
    // Annotated rather than cast: a platform whose `pid_t` is not an `i32` is a compile error here instead
    // of a silent truncation on the wire
    let pid: i32 = master.process_group_leader()?;
    let of = Pid::from_u32(u32::try_from(pid).ok()?);
    // A `System` that cannot be created is one more way the OS declines to say, which is what `None` is for
    let mut system = System::new().ok()?;
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[of]), true, ProcessRefreshKind::nothing());
    let name = system.process(of).map(|process| process.name().to_string_lossy().into_owned());
    Some(Foreground { pid, name })
}

/// Every session this broker owns, and where their output goes.
///
/// **The lock order is `Sessions` then a session's `screen`, and nothing else is ever held with either.**
/// Written down because the only way to get a deadlock out of this design is to invert it, and an inversion
/// typechecks. As it stands: the methods here take a session's `screen` while their caller holds the
/// `Sessions` mutex, so that pair always goes one way; the reader thread per session takes `screen`, the
/// pty's `writer` and its `waiters`, and never takes `Sessions` at all; and `main`'s writer loop releases
/// the `Sessions` guard before it touches the feed, so those two are never held together.
///
/// **What would make this fire is a second lock taken in the other order** — a `Sessions` method reached
/// while a `screen` guard is alive, or the reader thread asking the broker for a session. Nothing checks
/// it, which is why it is here rather than implied: there is no loom harness in this crate, deliberately,
/// because every piece of shared state is behind a coarse mutex rather than a lock-free structure, and the
/// credit semaphore returns exactly one unit per `notify_one` so the lost-wakeup a bulk return invites
/// cannot arise. A harness exploring two mutexes held in one order would be exercising this comment.
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

/// What the broker tells a shell its terminal is.
///
/// **The broker names it because the broker *is* it.** A shell inherits the environment of whatever started
/// the app, and an app launched from the Dock or the Start menu has no `TERM` at all — so `less`, `vim` and
/// `top` read no terminfo and fall back to a dumb terminal or stall looking for one. That is not a theory:
/// the alternate-screen case in this repository's own suite waited ten seconds for a switch that never came
/// the first time it ran on a runner with no `TERM`, and had to declare one to pass.
///
/// `xterm-256color` is what the emulator on the other end answers to. A caller may still override it, since
/// the requested environment is applied after this.
const DEFAULT_TERM: &str = "xterm-256color";

/// The widest and tallest grid a client may ask for.
///
/// **A frame arrives from whatever opened the socket, and the grid it names is allocated.** `cols` and `rows`
/// are `u16`, so a request could ask for 65535 by 65535 — four billion cells, held per session, which is an
/// out-of-memory kill for a process that owns every terminal in the app.
///
/// **The figure is the memory rather than the display.** A session holds its viewport *and*
/// `HISTORY_LINES` of scrollback, and the broker holds up to a session per terminal the app has open, so a
/// cap multiplies three times over: at a thousand it is about two million cells a session, and at the
/// two thousand this first was, four times that — which is gigabytes across a full app for a grid nothing
/// could be drawing. The widest real display at the smallest usable font is a few hundred columns, so a
/// thousand is headroom nobody meets rather than a limit anyone notices.
const MAX_GRID: u16 = 1_000;

/// The grid a request gets, which is the one it asked for unless nothing could draw it.
///
/// **Capped rather than refused**, because a client that asked cannot do anything useful with a refusal and
/// the honest reading of an impossible number is "as big as you can". There is deliberately no floor: a
/// zero was probed unguarded and is benign — a `0x0` spawn, its `screen` and a resize back all answer
/// normally, `portable_pty` and the VT core each tolerating it — so a floor would be a guard with nothing
/// behind it.
fn grid(cols: u16, rows: u16) -> (u16, u16) {
    (cols.min(MAX_GRID), rows.min(MAX_GRID))
}

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
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd: Option<String>,
    ) -> Result<(SessionId, u32), Refusal> {
        let (cols, rows) = grid(cols, rows);
        let pty = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| Refusal::pty("opening a pty", error))?;

        let mut command = match shell {
            Some(program) => CommandBuilder::new(program),
            None => CommandBuilder::new_default_prog(),
        };
        for arg in args {
            command.arg(arg);
        }
        // Before the requested environment, so a caller that names its own `TERM` wins and this is only the
        // answer for the common case of nobody saying
        command.env("TERM", DEFAULT_TERM);
        // Added rather than replacing: the broker's own environment is what a shell inherits, and it has
        // already had the app's identity taken out of it (`brokerEnvironment`, app side)
        for (name, value) in env {
            command.env(name, value);
        }
        if let Some(directory) = cwd {
            command.cwd(directory);
        }
        let mut child = pty.slave.spawn_command(command).map_err(|error| Refusal::pty("spawning a shell", error))?;
        // **The slave is dropped here on purpose**: holding it keeps its fd open, so the master never sees
        // EOF and the reader below would block for ever after the process exits.
        drop(pty.slave);

        let reader = pty.master.try_clone_reader().map_err(|error| Refusal::pty("reading a pty", error))?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pty.master.take_writer().map_err(|error| Refusal::pty("writing to a pty", error))?));

        self.next += 1;
        let session = format!("s-{}", self.next);
        let pid = child.process_id().unwrap_or_default();

        // One thread per session, which is what the skeleton needs; bounded I/O and backpressure are
        // Phase B's, and the goal says so rather than leaving it to be discovered here.
        // **The reader does not decide what a message is.** It hands what one `read` returned to the
        // coalescer, which flushes on a window or a cap — so N bytes of output no longer means N messages,
        // which is F4. The internal hop is unbounded on purpose: the bound belongs at the coalescer, where
        // credit is spent, and a reader blocked on this queue could not notice EOF.
        let (chunks, incoming) = channel::<Vec<u8>>();
        let events = self.events.clone();
        let reading = session.clone();
        let screen = Arc::new(Mutex::new(Screen::new(cols, rows)));
        let parsing = Arc::clone(&screen);
        let waiters: Arc<Mutex<Vec<Waiter>>> = Arc::new(Mutex::new(Vec::new()));
        let waiting = Arc::clone(&waiters);
        let ending = Arc::clone(&waiters);
        let answering = Arc::clone(&writer);
        let killer = child.clone_killer();
        let waited = session.clone();
        let exiting = self.events.clone();

        // One thread per session, which is what the skeleton needs; bounded I/O and backpressure are
        // Phase B's, and the goal says so rather than leaving it to be discovered here.
        // **The reader does not decide what a message is.** It hands what one `read` returned to the
        // coalescer, which flushes on a window or a cap — so N bytes of output no longer means N messages,
        // which is F4. The internal hop is unbounded on purpose: the bound belongs at the coalescer, where
        // credit is spent, and a reader blocked on this queue could not notice EOF.
        std::thread::spawn({
            let session = session.clone();
            let events = events.clone();
            let credit = Arc::clone(&self.credit);
            move || coalesce(session, incoming, events, credit)
        });
        let reader_events = events.clone();
        let reader_session = session.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        // The authority first, then the renderer's copy. The waits are settled inside this
                        // same lock, in one order everywhere — screen, then waiters — so the two threads
                        // that take both cannot deadlock on each other
                        let (replies, met) = match parsing.lock() {
                            Ok(mut screen) => {
                                screen.feed(&buffer[..read]);
                                let replies = screen.take_replies();
                                let met = settle_met(&waiting, &screen);
                                (replies, met)
                            }
                            Err(_) => (Vec::new(), 0),
                        };
                        for _ in 0..met {
                            let _ = reader_events.send(Outgoing::answer(Event::Waited {
                                session: reader_session.clone(),
                                outcome: WaitOutcome::Met,
                            }));
                        }
                        // **What the program asked the terminal, answered by the terminal that is here.**
                        // ConPTY opens with a cursor-position request and waits for the reply before it
                        // initialises, so a session nobody answers never starts at all — which is invisible
                        // on a POSIX pty, where nothing asks. It cannot be the window's job: a session with
                        // no window has to run, which is why the broker owns one.
                        for reply in replies {
                            if let Ok(mut writer) = answering.lock() {
                                let _ = writer.write_all(reply.as_bytes()).and_then(|()| writer.flush());
                            }
                        }
                        if chunks.send(buffer[..read].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
            // The coalescer ends when this drops, flushing what it holds, so nothing is lost at EOF
            drop(chunks);
        });

        // **The exit is waited for, not inferred from the output ending.** A POSIX pty gives EOF on the
        // master when the last slave closes, so the reader could report it; a ConPTY does not, because this
        // process still holds the pseudoconsole — so on Windows the read simply never returned and a shell
        // the user exited was never reported at all. The terminal stayed on screen with nothing behind it,
        // which is what X2 calls an undead terminal. Waiting on the child is the one signal both platforms
        // give, and it still polls nothing.
        std::thread::spawn(move || {
            let code = child.wait().ok().map(|status| status.exit_code() as i32);
            // **A session ending settles every wait on it**, because nothing is coming: an `Exit` wait has
            // reached what it asked for and every other has not and never will. Without this a caller
            // waiting for a prompt from a shell the user just exited waits out its whole deadline.
            if let Ok(mut waiters) = ending.lock() {
                for waiter in waiters.drain(..) {
                    if waiter.settled.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                    let outcome = match waiter.until {
                        Until::Exit => WaitOutcome::Met,
                        _ => WaitOutcome::Exited,
                    };
                    let _ = exiting.send(Outgoing::answer(Event::Waited { session: waited.clone(), outcome }));
                }
            }
            let _ = events.send(Outgoing::answer(Event::Exited { session: reading, code }));
        });

        self.live.insert(
            session.clone(),
            Session { master: pty.master, writer, killer, _tree: Tree::owning(pid), waiters, screen },
        );
        Ok((session, pid))
    }

    pub fn write(&mut self, session: &str, data: &str) -> Outcome {
        let live = self.live.get_mut(session).ok_or_else(|| missing(session))?;
        let mut writer =
            live.writer.lock().map_err(|_| Refusal::pty("writing to a pty", "the writer's lock was poisoned"))?;
        writer
            .write_all(data.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|error| Refusal::pty("writing to a pty", error))
    }

    pub fn resize(&mut self, session: &str, cols: u16, rows: u16) -> Outcome {
        let (cols, rows) = grid(cols, rows);
        let live = self.live.get_mut(session).ok_or_else(|| missing(session))?;
        live.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|error| Refusal::pty("resizing a pty", error))?;
        // The screen is resized too, or the authority would describe a viewport the process no longer has
        if let Ok(mut screen) = live.screen.lock() {
            screen.resize(cols, rows);
        }
        Ok(())
    }

    /// The screen as this session has it, with up to `history` lines above the viewport.
    pub fn snapshot(&self, session: &str, history: u32) -> Result<Snapshot, Refusal> {
        let live = self.live.get(session).ok_or_else(|| missing(session))?;
        let screen =
            live.screen.lock().map_err(|_| Refusal::pty("reading a screen", "the screen's lock was poisoned"))?;
        Ok(screen.snapshot(history))
    }

    /// What is running in a session's pty now, measured rather than claimed.
    pub fn foreground(&self, session: &str) -> Result<Option<Foreground>, Refusal> {
        let live = self.live.get(session).ok_or_else(|| missing(session))?;
        Ok(foreground_of(live.master.as_ref()))
    }

    /// What the shell has reported about itself.
    pub fn facts(&self, session: &str) -> Result<Facts, Refusal> {
        let live = self.live.get(session).ok_or_else(|| missing(session))?;
        let screen = live
            .screen
            .lock()
            .map_err(|_| Refusal::pty("reading a session's facts", "the screen's lock was poisoned"))?;
        Ok(screen.facts().clone())
    }

    /// Where `text` appears in the screen and the `history` lines above it.
    pub fn search(&self, session: &str, text: &str, history: u32) -> Result<Vec<Match>, Refusal> {
        let live = self.live.get(session).ok_or_else(|| missing(session))?;
        let screen =
            live.screen.lock().map_err(|_| Refusal::pty("searching a screen", "the screen's lock was poisoned"))?;
        Ok(screen.search(text, history))
    }

    /// Waits for a session to reach a state, answering once.
    ///
    /// **A condition that already holds is answered here**, without a waiter: a caller asking whether a
    /// session is idle when it is gets its answer in the same turn, and nothing is registered to be settled
    /// later. Otherwise the waiter goes on the session, where the reader settles it, and a deadline thread
    /// answers `Timeout` if it gets there first.
    pub fn wait(&mut self, session: &str, until: Until, timeout_ms: u32) -> Outcome {
        let live = self.live.get(session).ok_or_else(|| missing(session))?;
        let settled = Arc::new(AtomicBool::new(false));
        let waiter = Waiter { until, settled: Arc::clone(&settled) };

        let holds = {
            let screen = live
                .screen
                .lock()
                .map_err(|_| Refusal::pty("waiting on a session", "the screen's lock was poisoned"))?;
            waiter.holds(&screen)
        };
        if holds {
            settled.store(true, Ordering::SeqCst);
            self.answer(Event::Waited { session: session.to_string(), outcome: WaitOutcome::Met });
            return Ok(());
        }

        live.waiters
            .lock()
            .map_err(|_| Refusal::pty("waiting on a session", "the waiters' lock was poisoned"))?
            .push(waiter);

        let events = self.events.clone();
        let deadline = session.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(timeout_ms as u64));
            if settled.swap(true, Ordering::SeqCst) {
                return;
            }
            let _ = events.send(Outgoing::answer(Event::Waited { session: deadline, outcome: WaitOutcome::Timeout }));
        });
        Ok(())
    }

    /// Ends the session. The reader thread sees the pty close and reports `Exited`, so this reports nothing:
    /// one exit, one event, whichever way the process ended.
    pub fn kill(&mut self, session: &str) -> Outcome {
        let mut live = self.live.remove(session).ok_or_else(|| missing(session))?;
        live.killer.kill().map_err(|error| Refusal::pty("killing a session", error))
    }

    /// Drops the record of a session whose process has ended, which the writer thread does when it sees the
    /// `Exited` it is about to send. Nothing is killed here: the process is already gone.
    pub fn forget(&mut self, session: &str) {
        self.live.remove(session);
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
        for (_, mut live) in self.live.drain() {
            let _ = live.killer.kill();
        }
    }
}

fn missing(session: &str) -> Refusal {
    Refusal { code: ErrorCode::NoSuchSession, message: format!("no session {session}") }
}
