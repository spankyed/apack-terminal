//! One pty and the process in it: the process layer, with nothing that knows what a screen is.
//!
//! The split the goal asks for — a process layer under a terminal capability — shows up here as what this
//! file does *not* do: it moves bytes and owns a lifetime, and `cols`/`rows` pass through to the pty because
//! that is the pty's own shape, not because this layer models a screen.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::deadlines::Deadlines;
use crate::holds::Holds;
use crate::housekeeping::Housekeeping;
use crate::markers::MarksKey;
use crate::output::{Credit, Flow, Outgoing, READ_AHEAD, coalesce};
use crate::protocol::{
    ErrorCode, Event, Foreground, ProgressState, Scope, SessionId, SessionSummary, Signal, Until, WaitOutcome,
};
use crate::recording;
use crate::screen::{
    Colors, CommandAt, Facts, Fetched, HISTORY_LINES, ImagesAt, Match, Screen, Snapshot, Unanswered, Vouching,
};
use crate::tree::Tree;
use crate::view::publish::{Outlet, ViewId};
use crate::view::wire::{Position, Span};
use crate::vt::images::ImageRead;
use crate::vt::input::{Key, Pasted, Pointer};
use crate::vt::{Effect, Progress};
use crate::writer::{INPUT_BYTES, Input, Refused};

/// How long a session's exit waits for its output to finish being read.
///
/// **The exit and the last of the output are two events on two threads**, and a view told the session is
/// over is sent the screen as it ended — so the ending has to come second. On a POSIX pty the reader sees
/// the end of the stream as soon as the process is gone and this wait is over in microseconds. A ConPTY
/// reports no end while its console is open, so the console is closed once the shell has gone
/// (`Process::pty`), which flushes what it holds and ends the stream the same way. It is bounded for the
/// case where the end never comes: a background job the shell left behind still holds a POSIX pty open, a
/// reader parked on a pause is reading nothing, and a session whose shell has exited must not wait on
/// either. Not on the clock for any other reason.
const EXIT_DRAIN: Duration = Duration::from_millis(250);

/// How long a session that was ended is given to go before it is made to.
///
/// **Ending a session asks first.** What its program is sent is a hangup, which is what a terminal closing
/// is and what a shell tidies up on — its history written, its jobs told. A program may ignore one, and a
/// session whose program did would be out of the record with its process running and its exit never
/// reported. So an ending that has not happened by this long after it was asked for is made to happen
/// (`end_for_certain`). On the clock because it is a grace: the thing waited for is a program choosing
/// to stop, which nothing but its exit reports.
pub const KILL_GRACE: Duration = Duration::from_secs(2);

/// A running session: its process, and the screen its output is parsed into.
struct Session {
    process: Arc<Process>,
    /// How a session is ended from here. **The child itself belongs to the thread waiting on it**, which is
    /// what lets that wait block: a killer is a handle of its own, so ending a session never queues behind
    /// the wait that will observe the ending.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The process tree this session owns, on the platform that needs one owned (`tree.rs`). Dropped with
    /// the session, which is what ends the tree on Windows — so a `kill`, a disconnect and a broker that
    /// crashes all reap it by the same mechanism rather than by three paths remembering to.
    _tree: Option<Tree>,
    /// The VT state, shared with the reader thread that feeds it. **It is fed before the bytes leave for the
    /// renderer**, so the authority is never behind the replica — the only direction in which a disagreement
    /// is merely stale rather than wrong.
    screen: Arc<Mutex<Screen>>,
    /// Whether this session outlives whoever holds the broker (`Request::Keep`).
    kept: bool,
    /// Closed by the thread that waits on the process, once it has been waited for: how an ending that
    /// was asked for learns it has happened (`end_for_certain`).
    reaped: Receiver<()>,
}

/// Makes sure a process that was asked to end does: one still there `KILL_GRACE` after is killed, with
/// everything in its process group.
///
/// **By the signal a caller could have sent itself** (`deliver`), so there is one piece of code in this
/// process that signals anything and one guard on what it may be aimed at. **Only while the process is
/// unreaped**: `reaped` closes when it has been waited for, and until then its pid is its own — a pid is
/// signalled here because nothing has collected it, never because of what it was a moment ago. On
/// Windows there is nothing to send and nothing needed: the session's tree ends with its record (`Tree`).
fn end_for_certain(pid: u32, reaped: Receiver<()>) {
    std::thread::spawn(move || {
        // A pattern rather than a comparison, so there is no operator here to turn round: the one way
        // to the signal is the grace running out with the process unreaped
        if let Err(RecvTimeoutError::Timeout) = reaped.recv_timeout(KILL_GRACE) {
            // Whatever it started while it was ignoring its own hangup
            hang_up_the_rest(&[pid]);
            let _ = deliver(pid, Signal::Sigkill, Scope::Group);
        }
    });
}

/// Hangs up whatever else is in the sessions these shells lead, before the shells themselves are.
///
/// **Nothing but the shell can be relied on to tell a shell's jobs that their terminal has gone, and the
/// shell cannot be relied on.** The kernel hangs up only the foreground group when a session's leader
/// goes, so a job the shell put in the background — a group of its own under job control — hears of it
/// only if the shell passes its own hangup on. One that is killed passes on nothing, and one that ends on
/// the hangup may not either: measured here with the login shell under load, where it was gone two
/// milliseconds after the signal and its `sleep 300 &` was left running under nothing
/// (`killing_a_session_takes_the_whole_tree_with_it` is that case). So the broker tells them itself.
///
/// **While the leader is alive, which is why it is done first.** What is in a session's tree is read from
/// each process's parent (`inside_tree`), and a job whose shell has gone has no parent to be read from.
/// Ending a session calls this before its shell is signalled, and the grace running out calls it before
/// the shell is killed.
///
/// **A hangup, as the shell would have sent, and through `deliver`.** A job that was told to outlive its
/// terminal (`nohup`) ignores it and does, which is what it asked for; and each one is signalled by the
/// one function here that signals anything, which sends only to a process still inside that session's
/// tree. One listing of the machine's processes however many sessions are ending.
fn hang_up_the_rest(leaders: &[u32]) {
    #[cfg(not(unix))]
    {
        let _ = leaders;
    }
    #[cfg(unix)]
    {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

        if leaders.is_empty() {
            return;
        }
        let Ok(mut system) = System::new() else {
            return;
        };
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let others: Vec<(u32, u32)> = system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                let session = process.session_id()?.as_u32();
                (leaders.contains(&session) && pid.as_u32() != session).then_some((session, pid.as_u32()))
            })
            .collect();
        for (leader, pid) in others {
            let _ = deliver(leader, Signal::Sighup, Scope::Pid { pid });
        }
    }
}

/// What reaches a session's process. The record of the session owns it, and a `Handle` reaches it for
/// as long as the session is in the record.
struct Process {
    /// What is written to the program, by the one thread that writes to its pty (`writer::Input`).
    input: Arc<Input>,
    /// The master side of its pty, which is what is resized and what knows the foreground process.
    ///
    /// **`None` once its shell has exited**, which is when it is let go of: a ConPTY's output ends only
    /// when its console is closed, and the last of what the shell printed is read before the session is
    /// said to be over (`EXIT_DRAIN`). The reader holds its own handle on the output, so letting go of
    /// this one loses nothing still to be read on either platform.
    pty: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    /// Waits outstanding on this session, shared with the reader thread that settles them. **Checked where
    /// the bytes land**: the broker already knows when something arrived, so a caller waiting for a state
    /// costs a condition check per read rather than a timer asking.
    waiters: Arc<Mutex<Vec<Waiter>>>,
    /// Whether this session's pty is being read, shared with the reader thread it parks (`output::Flow`).
    /// Closed when the session goes, so a reader parked on a pause nobody is left to lift still ends.
    flow: Arc<Flow>,
    /// The process the broker started, kept because a `Signal` names it — the leader, its group, or a pid
    /// this is the root of.
    pid: u32,
}

/// A recording opened to be looked at: a screen with no process behind it.
struct Rewound {
    screen: Arc<Mutex<Screen>>,
    look: Arc<Mutex<Look>>,
    /// The connection that opened it, which is whose it is (`Sessions::end_rewound_by`).
    opener: u64,
}

impl Rewound {
    /// Tells its views it is over.
    fn end(&self) {
        if let Ok(mut screen) = self.screen.lock() {
            screen.end_views();
        }
    }
}

/// Where a rewound session is in its recording.
struct Look {
    index: recording::Index,
    /// The size its viewer has, which a seek puts back after replaying at the sizes the recording had.
    size: (u16, u16),
}

/// One session, to act on without the record of sessions locked (`Sessions::handle`).
///
/// **It keeps nothing alive.** The screen, the pty and a recording's open file are the record's; a
/// handle a view connection keeps for as long as it is attached answers `no session` once the session is
/// over, rather than holding a hundred thousand lines, a pty or a file of a session nothing else
/// remembers.
#[derive(Clone)]
pub struct Handle {
    session: SessionId,
    screen: Weak<Mutex<Screen>>,
    behind: Behind,
}

/// What is behind a session's screen.
#[derive(Clone)]
enum Behind {
    /// A program in a pty
    Process(Weak<Process>),
    /// A recording being looked at, which has no program to type at
    Recording(Weak<Mutex<Look>>),
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
    ///
    /// **`recent` is how far above the viewport a `Text` wait looks**, and it is what keeps a wait from
    /// costing the whole history on every read. A caller waiting for text means text that has been
    /// written rather than text still on screen, so the first ask covers everything held; after that a
    /// read can only have changed the viewport and the rows it scrolled off, and the reader says how many
    /// those could be. With no `Text` waiter nothing searches at all.
    fn holds(&self, screen: &Screen, recent: u32) -> bool {
        match &self.until {
            Until::Idle => !screen.facts().running,
            Until::Text { text } => screen.shows_recently(text, recent),
            Until::Exit => false,
        }
    }
}

/// Settles the waits the screen now satisfies, answering each once. The count is what to report.
fn settle_met(waiters: &Mutex<Vec<Waiter>>, screen: &Screen, recent: u32) -> usize {
    let Ok(mut waiters) = waiters.lock() else {
        return 0;
    };
    let mut met = 0;
    waiters.retain(|waiter| {
        if waiter.settled.load(Ordering::SeqCst) {
            return false;
        }
        if waiter.holds(screen, recent) {
            waiter.settled.store(true, Ordering::SeqCst);
            met += 1;
            return false;
        }
        true
    });
    met
}

/// The most rows a run of `bytes` can have scrolled off a grid `cols` wide.
///
/// A row leaves the viewport when a line feed moves past its last row or a character wraps there, so the
/// bound is every line feed plus every wrap the bytes could have caused if each were a character — and
/// one more, for the row the cursor was already on. It is a ceiling: most bytes in a real stream are
/// escape sequences and continuation bytes that add no row.
pub fn rows_added(bytes: &[u8], cols: u16) -> u32 {
    let feeds = bytes.iter().filter(|byte| **byte == b'\n').count();
    let wraps = bytes.len() / usize::from(cols.max(1));
    u32::try_from(feeds + wraps + 1).unwrap_or(u32::MAX)
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
///
/// **Windows answers `None`, and the branch is inside this function rather than a second definition of
/// it.** There is nothing to ask there: `portable-pty`'s `process_group_leader` is `#[cfg(unix)]` in the
/// crate, and a ConPTY has no foreground process group — its equivalent is the job object `tree.rs` already
/// holds per session, whose `JobObjectBasicProcessIdList` lists what is in it, unwritten because a platform
/// claim is made on that platform. Two `#[cfg]` definitions put the mutation gate in an impossible
/// position: off Windows the second one is not compiled, so every mutant of it survives a suite that cannot
/// reach it, and a run on any developer's machine reports a survivor nothing can kill. One function has one
/// set of mutants, and they land in code the suite runs.
fn foreground_of(master: &(dyn MasterPty + Send)) -> Option<Foreground> {
    #[cfg(not(unix))]
    {
        let _ = master;
        None
    }
    #[cfg(unix)]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

        // Annotated rather than cast: a platform whose `pid_t` is not an `i32` is a compile error here
        // instead of a silent truncation on the wire
        let pid: i32 = master.process_group_leader()?;
        let of = Pid::from_u32(u32::try_from(pid).ok()?);
        // A `System` that cannot be created is one more way the OS declines to say, which `None` carries
        let mut system = System::new().ok()?;
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[of]), true, ProcessRefreshKind::nothing());
        let name = system.process(of).map(|process| process.name().to_string_lossy().into_owned());
        Some(Foreground { pid, name })
    }
}

/// The name of the signal the system described as `described`, or that description when it is not one of
/// these.
///
/// **The description is what `portable-pty` hands back and a name is what a contract can carry.** It calls
/// `strsignal`, which answers prose written for a person — *Terminated* — and translated with the system's
/// locale, so a consumer cannot switch on it. The inversion is exact rather than a guess: the same
/// `strsignal`, in this same process, is asked what it calls each signal this broker knows the name of, and
/// the answers are compared. Anything else is carried through as worded, which is the honest answer for a
/// signal with no name here.
///
/// **Built once.** `strsignal` is not required to be thread-safe, and a child exiting is the only caller —
/// so the table is derived behind a `OnceLock`, and every lookup after the first compares strings.
fn signal_named(described: &str) -> String {
    #[cfg(unix)]
    {
        if let Some(name) = descriptions().iter().find(|(_, prose)| prose == described).map(|(name, _)| *name) {
            return name.to_owned();
        }
    }
    described.to_owned()
}

/// The signals this broker names, which are the ones a `Signal` may send and the ones that kill a program.
#[cfg(unix)]
const NAMED_SIGNALS: &[(&str, i32)] = &[
    ("SIGHUP", libc::SIGHUP),
    ("SIGINT", libc::SIGINT),
    ("SIGQUIT", libc::SIGQUIT),
    ("SIGILL", libc::SIGILL),
    ("SIGABRT", libc::SIGABRT),
    ("SIGFPE", libc::SIGFPE),
    ("SIGKILL", libc::SIGKILL),
    ("SIGBUS", libc::SIGBUS),
    ("SIGSEGV", libc::SIGSEGV),
    ("SIGPIPE", libc::SIGPIPE),
    ("SIGALRM", libc::SIGALRM),
    ("SIGTERM", libc::SIGTERM),
];

#[cfg(unix)]
fn descriptions() -> &'static [(&'static str, String)] {
    static NAMED: std::sync::OnceLock<Vec<(&'static str, String)>> = std::sync::OnceLock::new();
    NAMED.get_or_init(|| {
        NAMED_SIGNALS.iter().filter_map(|(name, number)| described(*number).map(|prose| (*name, prose))).collect()
    })
}

/// What the system calls this signal, or nothing when it declines to say.
#[cfg(unix)]
fn described(number: i32) -> Option<String> {
    // SAFETY: `strsignal` takes a signal number and answers a pointer to a string it owns; the result is
    // copied before anything else can ask, and a null is the documented "no description".
    let prose = unsafe { libc::strsignal(number) };
    if prose.is_null() {
        return None;
    }
    // SAFETY: non-null, and `strsignal`'s answer is a NUL-terminated C string.
    Some(unsafe { std::ffi::CStr::from_ptr(prose) }.to_string_lossy().into_owned())
}

/// A number `kill(2)` may be given: one process, or one process group, and never every process.
///
/// **A private field and no `Default`, both deliberately.** The two numbers that must never reach `kill` are
/// `0` — every process in *this* process's group — and `-1`, every process the user owns. A mutation tester
/// builds its replacements from `Default::default()` and small literals, so a return type it cannot
/// construct is a mutant that does not compile and therefore never *runs*; for a function whose wrong
/// answer is "signal everything", not running is the only safe outcome. This exists because it happened: a
/// run of `cargo xtask pre-push` on a developer's machine sent `kill(-1, SIGKILL)` and killed every
/// application the user had open.
///
/// **The guard is in the constructor**, so there is no state in which an unsafe target exists — rather than
/// in the caller, where each new call site is another chance to forget it. `.cargo/mutants.toml` excludes
/// it, because a mutant of a guard is the guard gone.
#[derive(Debug, Clone, Copy)]
#[cfg(unix)]
pub struct SignalTarget(i32);

#[cfg(unix)]
impl SignalTarget {
    /// Refuses the two that broadcast; everything else is one process or one group.
    pub fn at(target: i32) -> Result<Self, Refusal> {
        if target == 0 {
            return Err(Self::refused(0, "every process in this broker's own group"));
        }
        if target == -1 {
            return Err(Self::refused(1, "every process the user owns"));
        }
        Ok(Self(target))
    }

    /// What `kill` is given. Read inside this module without it, so a mutant of this cannot aim a signal.
    pub fn number(self) -> i32 {
        self.0
    }

    fn refused(pid: u32, what: &str) -> Refusal {
        Refusal { code: ErrorCode::Pty, message: format!("{pid} is not a process this broker can signal ({what})") }
    }
}

/// The pid `kill(2)` is given for a scope, or a refusal.
///
/// **A pid below 2 is refused, and that is the whole reason this is a function of its own.** `kill(0, …)`
/// signals every process in the *caller's* own process group — the broker's — and `kill(-0, …)` is the same
/// call written the other way. A session records `child.process_id().unwrap_or_default()`, so a spawn that
/// could not report a pid records 0, and one stop request would then take the broker and every session it
/// owns. Pid 1 is refused for the neighbouring reason: `-1` is every process the user may signal.
///
/// **Nothing produces that today**, every platform `portable-pty` spawns on answering a pid; the edit that
/// would make this fire is one where `process_id()` answers `None`, which is why the arithmetic is here
/// rather than inline, where it cannot be tested without signalling something.
#[cfg(unix)]
pub fn signal_target(leader: u32, scope: Scope) -> Result<SignalTarget, Refusal> {
    let real = |pid: u32| -> Result<i32, Refusal> {
        if pid < 2 {
            return Err(SignalTarget::refused(pid, "a session whose process id is unknown, or init"));
        }
        i32::try_from(pid).map_err(|_| SignalTarget::refused(pid, "the pid does not fit a signal"))
    };
    match scope {
        Scope::Leader => SignalTarget::at(real(leader)?),
        // A negative pid is its process group, which `portable-pty` made this process the leader of
        Scope::Group => SignalTarget::at(-real(leader)?),
        Scope::Pid { pid } => SignalTarget::at(real(pid)?),
    }
}

/// Delivers a signal to a session's process, its group, or one process inside its tree.
///
/// **The pid scope is checked against the tree**, because a pid is a number any caller can name: without
/// the check this request is `kill(2)` over a socket, which would hand whatever can reach the broker the
/// ability to signal any process the user owns. The walk is bounded, so a parent chain that lies cannot
/// hang the broker.
///
/// **Windows has no signals**, and the job object `tree.rs` holds cannot express what this asks for, so
/// there it is a refusal rather than an approximation.
///
/// **One function with the branch inside it rather than two definitions**, for the reason `foreground_of`
/// gives above: off a platform the second definition is not compiled, so every mutant of it survives a
/// suite that cannot reach it and a run on any developer's machine reports a survivor nothing can kill.
/// Measured here — the two-definition version left `replace deliver with Default::default()` missed.
fn deliver(leader: u32, signal: Signal, scope: Scope) -> Outcome {
    #[cfg(not(unix))]
    {
        let _ = (leader, signal, scope);
        Err(Refusal {
            code: ErrorCode::Unsupported,
            message: "this platform has no signals; end the session with kill".to_owned(),
        })
    }
    #[cfg(unix)]
    {
        if let Scope::Pid { pid } = scope
            && !inside_tree(leader, pid)
        {
            return Err(Refusal {
                code: ErrorCode::OutsideSession,
                message: format!("{pid} is not in the process tree of the session led by {leader}"),
            });
        }
        let target = signal_target(leader, scope)?;
        // SAFETY: `kill` takes two integers and answers an error through errno, which is read below.
        // `target.0` rather than `number()`: an accessor between the guard and the syscall is one more
        // expression a mutation could replace with a broadcast
        if unsafe { libc::kill(target.0, signal.number()) } == 0 {
            return Ok(());
        }
        Err(Refusal::pty("signalling a session", std::io::Error::last_os_error()))
    }
}

/// Whether `pid` is `leader` or something it started, as an answer rather than a signal.
///
/// **Windows has no answer here yet, and saying so is the honest one.** The route there is the job object
/// `tree.rs` already holds per session — `JobObjectBasicProcessIdList` lists what is in it — and a platform
/// claim is made on that platform, so it is unwritten rather than guessed. A refusal a caller can see beats
/// the alternative a consumer had before this: a parent walk with `/bin/ps`, which on Windows finds no `ps`,
/// answers *not owned* for everything, and refuses every claim without saying why.
///
/// **One function with the branch inside it**, for the reason `deliver` and `foreground_of` both give: two
/// `#[cfg]` definitions leave every mutant of the uncompiled one surviving a suite that cannot reach it.
fn owned_by(leader: u32, pid: u32) -> Result<bool, Refusal> {
    #[cfg(not(unix))]
    {
        let _ = (leader, pid);
        Err(Refusal {
            code: ErrorCode::Unsupported,
            message: "this platform cannot be asked what is in a session's tree".to_owned(),
        })
    }
    #[cfg(unix)]
    {
        Ok(inside_tree(leader, pid))
    }
}

/// Whether `pid` is `leader` or something it started, however deep.
///
/// **Bounded at a depth no real tree reaches**, because the parent chain is read from the OS a process at a
/// time and a process that has been reparented while this walks could otherwise be followed in a circle.
#[cfg(unix)]
fn inside_tree(leader: u32, pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    // Neither is a process a session could own: 0 is what an unknown pid records as, and 1 is init
    if pid < 2 || leader < 2 {
        return false;
    }
    if pid == leader {
        return true;
    }
    let Ok(mut system) = System::new() else {
        return false;
    };
    let mut at = Pid::from_u32(pid);
    for _ in 0..64 {
        system.refresh_processes_specifics(ProcessesToUpdate::Some(&[at]), true, ProcessRefreshKind::nothing());
        let Some(parent) = system.process(at).and_then(sysinfo::Process::parent) else {
            return false;
        };
        if parent.as_u32() == leader {
            return true;
        }
        at = parent;
    }
    false
}

/// Every session this broker owns, and where their output goes.
///
/// **The record of the sessions is locked to look one up, add one or take one out, and for nothing
/// else.** What is done *to* a session is done through a `Handle` taken under that lock and used after it
/// is let go, so a request that takes time — a search of a long history, a recording replayed, a resize
/// that reflows a hundred thousand lines — holds up that session and no other, and a session that is
/// stuck cannot stop another being listed or killed.
///
/// **The lock order, which is what a deadlock would have to invert.** There are three kinds of lock:
/// this record; a session's `screen`; and the small ones beside a screen — its waits, its pty, a rewound
/// session's place in its recording (`Look`). Nothing is ever locked while the record is. Under a screen
/// only its waits are locked (the reader thread and `wait`, both in that order), and a queue's own lock
/// for the moment it takes to add to it (`writer::Input`, a view's sink). A `Look` is locked before the
/// screen it is shown on (`Handle::seek`), and nothing takes them the other way. The reader thread per
/// session takes that session's screen and never this record at all.
///
/// **What would make this fire is a lock taken in the other order** — a `Sessions` method reached while
/// a screen is locked, or a screen locked while the record is. Nothing checks it, which is why it is
/// here rather than implied: there is no loom harness in this crate, deliberately, because every piece of
/// shared state is behind a coarse mutex rather than a lock-free structure, and the credit semaphore
/// returns exactly one unit per `notify_one` so the lost-wakeup a bulk return invites cannot arise.
pub struct Sessions {
    record: Mutex<Record>,
    /// Answers and reports: no credit, so none of them waits behind output
    events: Sender<Outgoing>,
    /// What output spends, which is the channel's bound
    credit: Arc<Credit>,
    /// When each wait nothing has met is given up on
    deadlines: Deadlines,
    /// Compresses the history of a session that has gone quiet
    housekeeping: Housekeeping,
    /// Ends a hold on a screen that its program does not
    holds: Holds,
}

/// Which sessions there are.
#[derive(Default)]
struct Record {
    next: u64,
    live: HashMap<SessionId, Session>,
    /// Recordings opened as sessions (`Request::Rewind`), by the id each was given.
    rewound: HashMap<SessionId, Rewound>,
    /// The file each recording is written to, and the last session to write it.
    ///
    /// **One writer a file.** A recording is a log its writer knows the layout of — where each keyframe
    /// is, how long it has grown — and a second session appending to the same file would leave each
    /// rewriting the other's entries out from under it.
    ///
    /// **An entry outlives its session, and is none the worse for it.** What reads this asks the named
    /// session what it has still to write (`rewind`), and a session that is gone has nothing: it reads
    /// as a file nobody is writing. The next session to record there puts its own name in.
    recording: HashMap<PathBuf, SessionId>,
}

/// What a spawn is asked for: `Request::Spawn`'s own fields, as one value.
///
/// The wire shape is the request's and unchanged — this exists because the method took eight arguments
/// once `unset` joined them, and a list of seven positional values is a call site nobody can read.
pub struct Spawning {
    pub cols: u16,
    pub rows: u16,
    pub shell: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub unset: Vec<String>,
    pub cwd: Option<String>,
    /// What the session is drawn in, when the caller knows.
    pub colors: Option<Colors>,
    /// Whether its bytes are sent as `Output` (`Request::Spawn`)
    pub output: bool,
    /// Where to record it, if anywhere, and whether to begin with what is recorded there.
    pub record: Option<String>,
    pub restore: bool,
    /// Whether its programs may send images (`Request::Spawn`)
    pub images: bool,
    /// What vouches for its shell's marks, if anything does (`Request::Spawn`)
    pub marks_key: Option<String>,
}

/// A failure a request turns into a `Failed` event rather than a panic.
#[derive(Debug)]
pub struct Refusal {
    pub code: ErrorCode,
    pub message: String,
}

impl Refusal {
    fn pty(what: &str, error: impl std::fmt::Display) -> Self {
        Self { code: ErrorCode::Pty, message: format!("{what}: {error}") }
    }
}

impl From<Unanswered> for Refusal {
    fn from(unanswered: Unanswered) -> Self {
        // A session that is over is, to whoever asks after it, a session that is not there
        let code = if unanswered == Unanswered::Ended { ErrorCode::NoSuchSession } else { ErrorCode::Pty };
        Self { code, message: unanswered.to_string() }
    }
}

impl From<crate::vt::Failed> for Refusal {
    fn from(failed: crate::vt::Failed) -> Self {
        Self { code: ErrorCode::Pty, message: failed.to_string() }
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

fn poisoned(what: &str) -> Refusal {
    Refusal::pty(what, "a lock was poisoned")
}

impl Handle {
    /// The session's screen, or the refusal for a session that has ended since this was taken.
    fn screen(&self) -> Result<Arc<Mutex<Screen>>, Refusal> {
        self.screen.upgrade().ok_or_else(|| missing(&self.session))
    }

    /// Runs `ask` on the session's screen, locked for as long as it takes and no longer.
    fn on_screen<T>(&self, ask: impl FnOnce(&mut Screen) -> T) -> Result<T, Refusal> {
        let screen = self.screen()?;
        let mut screen = screen.lock().map_err(|_| poisoned("reading a screen"))?;
        Ok(ask(&mut screen))
    }

    /// Bytes for the program, which is where a view's input ends up once it is encoded.
    ///
    /// **Queued, never written from here** (`writer::Input`): whoever called this is answered whether or
    /// not the program is reading.
    fn send(&self, bytes: Vec<u8>) -> Outcome {
        // A recording has no program to be typed at: what a view sends it goes nowhere
        let Behind::Process(process) = &self.behind else { return Ok(()) };
        let process = process.upgrade().ok_or_else(|| missing(&self.session))?;
        process.input.send(bytes).map_err(|refused| match refused {
            Refused::Closed => missing(&self.session),
            // The session is still there, whatever became of its pty: saying otherwise would have a
            // view forget a session it is still drawing
            Refused::Broken => Refusal {
                code: ErrorCode::Pty,
                message: format!("{}'s pty refused what was written to it, and takes no more", self.session),
            },
            Refused::Full => Refusal {
                code: ErrorCode::Pty,
                message: format!(
                    "{} has more than {INPUT_BYTES} bytes waiting for its program to read, and takes no more",
                    self.session
                ),
            },
        })
    }

    /// What a client writes to the program.
    pub fn write(&self, text: &str) -> Outcome {
        self.process()?;
        self.send(text.as_bytes().to_vec())
    }

    /// The session's process, or the refusal for a session that has none or is over.
    fn process(&self) -> Result<Arc<Process>, Refusal> {
        match &self.behind {
            Behind::Process(process) => process.upgrade().ok_or_else(|| missing(&self.session)),
            Behind::Recording(_) => Err(missing(&self.session)),
        }
    }

    /// `cell_width` and `cell_height` are one cell in pixels as the window measured it, or zero from a
    /// caller that measures nothing.
    ///
    /// Its views are sent the new grid by the screen (`Screen::resize`).
    pub fn resize(&self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) -> Outcome {
        let (cols, rows) = grid(cols, rows);
        match &self.behind {
            // A recording has no pty; its screen is the whole of what a resize changes
            Behind::Recording(look) => {
                let look = look.upgrade().ok_or_else(|| missing(&self.session))?;
                look.lock().map_err(|_| poisoned("resizing a screen"))?.size = (cols, rows);
            }
            Behind::Process(_) => {
                let process = self.process()?;
                let pty = process.pty.lock().map_err(|_| poisoned("resizing a pty"))?;
                // A session whose shell has exited has no pty left to tell, and its screen is still
                // one a view is drawing
                if let Some(pty) = pty.as_ref() {
                    pty.resize(PtySize {
                        rows,
                        cols,
                        // The pty carries the same fact for a program that asks the kernel rather than the terminal
                        pixel_width: cols.saturating_mul(cell_width),
                        pixel_height: rows.saturating_mul(cell_height),
                    })
                    .map_err(|error| Refusal::pty("resizing a pty", error))?;
                }
            }
        }
        // The screen is resized too, or the authority would describe a viewport the process no longer has
        self.on_screen(|screen| screen.resize(cols, rows, cell_width, cell_height))
    }

    /// The screen as this session has it, with up to `history` lines above the viewport.
    pub fn snapshot(&self, history: u32) -> Result<Snapshot, Refusal> {
        self.on_screen(|screen| screen.snapshot(history))
    }

    /// What is running in the session's pty now, measured rather than claimed. `None` as well for a
    /// recording, which has nothing running in it.
    pub fn foreground(&self) -> Option<Foreground> {
        let process = self.process().ok()?;
        let pty = process.pty.lock().ok()?;
        foreground_of(pty.as_deref()?)
    }

    /// What the shell has reported about itself, and what vouches for it: both as of one moment.
    pub fn facts(&self) -> Result<(Facts, Vouching), Refusal> {
        self.on_screen(|screen| (screen.facts().clone(), screen.vouching()))
    }

    /// Where `text` appears in the screen and the `history` lines above it.
    pub fn search(&self, text: &str, history: u32) -> Result<Vec<Match>, Refusal> {
        self.on_screen(|screen| screen.search(text, history))
    }

    /// Starts sending a view this session's frames through `outlet`.
    pub fn attach_view(&self, id: ViewId, outlet: impl Outlet + 'static) -> Outcome {
        Ok(self.on_screen(|screen| screen.attach_view(id, outlet))??)
    }

    pub fn detach_view(&self, id: ViewId) {
        let _ = self.on_screen(|screen| screen.detach_view(id));
    }

    /// A view drew the frame with this sequence.
    pub fn acknowledge(&self, id: ViewId, sequence: u64) -> Outcome {
        Ok(self.on_screen(|screen| screen.acknowledge(id, sequence))??)
    }

    /// Lines by number, in the numbering `epoch` names.
    pub fn fetch(&self, epoch: u32, first: u64, count: u32) -> Result<Fetched, Refusal> {
        Ok(self.on_screen(|screen| screen.fetch(epoch, first, count))??)
    }

    /// A key from a view, sent as whatever the terminal's modes make of it.
    pub fn key(&self, key: &Key<'_>) -> Outcome {
        let bytes = self.on_screen(|screen| screen.key(key))??;
        self.send(bytes)
    }

    /// Text that did not come from a key — what an input method composed — sent as it is.
    pub fn text(&self, text: &str) -> Outcome {
        self.send(text.as_bytes().to_vec())
    }

    pub fn pointer(&self, view: ViewId, pointer: &Pointer) -> Outcome {
        let bytes = self.on_screen(|screen| screen.pointer(view, pointer))??;
        self.send(bytes)
    }

    /// Pastes, answering whether it went: `false` is text that could run a command and was not allowed to.
    ///
    /// **Encoded under the screen's lock and sent after it is let go.** What a paste becomes depends on
    /// the terminal's modes, which is the screen's to say; where it goes is the program, which reads when
    /// it likes — and the thread that would let it read is the one waiting for this screen.
    pub fn paste(&self, text: &str, allow_unsafe: bool) -> Result<bool, Refusal> {
        match self.on_screen(|screen| screen.paste(text, allow_unsafe))?? {
            Pasted::Encoded(bytes) => self.send(bytes).map(|()| true),
            Pasted::Unsafe => Ok(false),
        }
    }

    /// The view gained or lost focus. **One that lost it holds no button down any more**: wherever the
    /// button is let go, it is not here, and the release this view would have reported will not come.
    pub fn focus(&self, view: ViewId, gained: bool) -> Outcome {
        let bytes = self.on_screen(|screen| {
            if !gained {
                screen.release_pointer(view);
            }
            screen.focus(gained)
        })??;
        self.send(bytes)
    }

    pub fn word_at(&self, epoch: u32, at: Position) -> Result<Option<Span>, Refusal> {
        Ok(self.on_screen(|screen| screen.word_at(epoch, at))??)
    }

    pub fn line_at(&self, epoch: u32, at: Position) -> Result<Option<Span>, Refusal> {
        Ok(self.on_screen(|screen| screen.line_at(epoch, at))??)
    }

    pub fn text_of(&self, epoch: u32, span: Span) -> Result<String, Refusal> {
        Ok(self.on_screen(|screen| screen.text_of(epoch, span))??)
    }

    pub fn find(&self, needle: &str) -> Result<(u32, Vec<Span>), Refusal> {
        Ok(self.on_screen(|screen| screen.find(needle))??)
    }

    /// Every image in the session, with the epoch their lines are in and one cell in pixels.
    pub fn images(&self) -> Result<ImagesAt, Refusal> {
        Ok(self.on_screen(|screen| screen.images())??)
    }

    /// Part of an image's pixels, or `None` for an image that is not there.
    pub fn image(&self, image: u32, offset: u32) -> Result<Option<ImageRead>, Refusal> {
        self.on_screen(|screen| screen.image(image, offset))
    }

    /// The commands the session's shell has reported, with the epoch their lines are in.
    pub fn commands(&self) -> Result<(u32, Vec<CommandAt>), Refusal> {
        Ok(self.on_screen(|screen| screen.commands())??)
    }

    /// What one command printed, or `None` for one that is not kept or whose output has left the history.
    pub fn command_output(&self, command: u64) -> Result<Option<String>, Refusal> {
        Ok(self.on_screen(|screen| screen.command_output(command))??)
    }

    /// Shows a rewound session's recording as it was at `at`, answering the moment shown: `at`, held to
    /// what the recording covers.
    ///
    /// **The recording is read for the moment asked for and no more**: the entries from the keyframe
    /// before it, which is at most a keyframe's distance of them (`recording::Index`).
    pub fn seek(&self, at: u64) -> Result<u64, Refusal> {
        let Behind::Recording(look) = &self.behind else { return Err(missing(&self.session)) };
        let look = look.upgrade().ok_or_else(|| missing(&self.session))?;
        // Held across the replay, so two seeks of one session show their moments one after the other
        let mut look = look.lock().map_err(|_| poisoned("reading a recording"))?;
        let at = at.max(look.index.first());
        let entries = look.index.moment(at).map_err(|error| Refusal::pty("reading a recording", error))?;
        let size = look.size;
        // Its views are sent the moment now showing by the resize that ends this, as they would be
        // anything else that changed
        self.on_screen(|screen| {
            screen.replay(&entries, at);
            screen.resize(size.0, size.1, 0, 0);
        })?;
        Ok(at)
    }
}

impl Sessions {
    pub fn new(events: Sender<Outgoing>, credit: Arc<Credit>) -> Self {
        Self {
            record: Mutex::new(Record::default()),
            deadlines: Deadlines::keeping(events.clone()),
            events,
            credit,
            housekeeping: Housekeeping::start(),
            holds: Holds::start(),
        }
    }

    /// The record, locked. A thread that panicked holding it left it as whole as it found it — every
    /// change to it is one insert or one removal — so what it holds is still true.
    fn record(&self) -> MutexGuard<'_, Record> {
        self.record.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Answers this connection.
    pub fn answer(&self, event: Event) {
        let _ = self.events.send(Outgoing::answer(event));
    }

    /// Waits until every answer and report queued so far has been written to whoever is attached.
    pub fn settled(&self) {
        let (reached, done) = channel();
        if self.events.send(Outgoing::reached(reached)).is_ok() {
            let _ = done.recv();
        }
    }

    /// A session to act on, or the refusal for one that is not there. **Taken under the record's lock
    /// and used after it**, which is what keeps one session's work from being another's wait.
    pub fn handle(&self, session: &str) -> Result<Handle, Refusal> {
        let record = self.record();
        match (record.live.get(session), record.rewound.get(session)) {
            (Some(live), _) => Ok(Handle {
                session: session.to_owned(),
                screen: Arc::downgrade(&live.screen),
                behind: Behind::Process(Arc::downgrade(&live.process)),
            }),
            (None, Some(rewound)) => Ok(Handle {
                session: session.to_owned(),
                screen: Arc::downgrade(&rewound.screen),
                behind: Behind::Recording(Arc::downgrade(&rewound.look)),
            }),
            (None, None) => Err(missing(session)),
        }
    }

    /// Starts a shell in a new pty and begins reading it. The id is the broker's to mint.
    pub fn spawn(&self, asked: Spawning) -> Outcome {
        let Spawning { cols, rows, shell, args, env, unset, cwd, colors, output, record, restore, images, marks_key } =
            asked;
        // **Before anything is started.** A key that is not one is a request this cannot carry out as
        // asked, and a session spawned without it would report as vouched what nothing vouches for
        let marks_key = match marks_key {
            Some(key) => Some(MarksKey::new(&key).ok_or_else(|| Refusal {
                code: ErrorCode::Malformed,
                message: format!("a marks key is {} lowercase hexadecimal characters", MarksKey::LENGTH),
            })?),
            None => None,
        };
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
        // **After the overrides, so asking for a name to be absent is what a caller gets.** A session
        // inherits this process's environment, which a caller cannot subtract from by adding entries — and
        // what it is usually subtracting is a credential the app carries and its child must not: a CLI that
        // found one would authenticate as the app rather than as whoever the user signed in as.
        for name in unset {
            command.env_remove(name);
        }
        if let Some(directory) = cwd {
            command.cwd(directory);
        }
        // **Before the program is started**, which is the last moment a wait costs nothing but the wait: a
        // session that replaces one just ended may find the last of that one's recording still on its way
        // to the file. A file that is not whole within `RECORDER_PATIENCE` is one this session does not
        // record to — it starts unrecorded and its answer says so, rather than starting late or over a
        // recording that is still being written
        let record = record.filter(|path| recording::settled(Path::new(path)));
        let mut child = pty.slave.spawn_command(command).map_err(|error| Refusal::pty("spawning a shell", error))?;
        // **The slave is dropped here on purpose**: holding it keeps its fd open, so the master never sees
        // EOF and the reader below would block for ever after the process exits.
        drop(pty.slave);

        let reader = pty.master.try_clone_reader().map_err(|error| Refusal::pty("reading a pty", error))?;
        let writer = pty.master.take_writer().map_err(|error| Refusal::pty("writing to a pty", error))?;

        let session = {
            let mut record = self.record();
            record.next += 1;
            format!("s-{}", record.next)
        };
        let pid = child.process_id().unwrap_or_default();

        // **Built before the session is anybody's to ask after**, with no lock held: a screen restored
        // from a recording replays all of it, which for a long one is the work of everything it printed.
        let mut screen = match marks_key {
            Some(key) => Screen::keyed(cols, rows, key),
            None => Screen::new(cols, rows),
        };
        if let Some(colors) = &colors {
            screen.set_colors(colors);
        }
        if images {
            let _ = screen.allow_images();
        }
        // **Before the process is read**, so what it prints lands after what the last session left and
        // is recorded from its first byte. A recording that cannot be opened — or that another session is
        // writing (`recording::claim`) — leaves the session without one rather than without a shell, and
        // the spawn's answer says which it is. With no lock held: opening it reads a disk
        let (mut recording, mut restored) = (None, false);
        if let Some(path) = record.as_deref().map(Path::new) {
            let opened =
                if restore { screen.resume_recording(path) } else { screen.start_recording(path).map(|()| false) };
            if let Ok(rebuilt) = opened {
                restored = rebuilt;
                // Named once it is open, which is when its directory is certain to be there to resolve
                recording = Some(recording::canonical(path));
            }
        }
        let recorded = recording.is_some();

        let screen = Arc::new(Mutex::new(screen));
        if let Ok(mut held) = screen.lock() {
            held.limit_holds(self.holds.for_screen(Arc::downgrade(&screen)));
        }
        let flow = Flow::reading();
        let waiters: Arc<Mutex<Vec<Waiter>>> = Arc::new(Mutex::new(Vec::new()));
        let process = Arc::new(Process {
            input: Input::start(writer),
            pty: Arc::new(Mutex::new(Some(pty.master))),
            waiters: Arc::clone(&waiters),
            flow: Arc::clone(&flow),
            pid,
        });
        let answering = Arc::clone(&process.input);
        let ending_input = Arc::clone(&process.input);
        let ending_pty = Arc::clone(&process.pty);
        let (reaping, reaped) = channel::<()>();

        // **In the record before anything can report its end.** The thread that waits on the child tells
        // the broker the session is over, and the broker forgets it then (`forget`): a child that exited
        // before its session was recorded would be forgotten first and recorded after, for ever.
        {
            let mut record = self.record();
            let held = &mut *record;
            // Only sessions still running are named: the list of recordings keeps what a rewind must wait
            // for, and a session that has ended has nothing left to write. Without this, every file a
            // broker ever recorded to stayed in it for the life of the broker
            held.recording.retain(|_, recorder| held.live.contains_key(recorder));
            if let Some(path) = &recording {
                record.recording.insert(path.clone(), session.clone());
            }
            record.live.insert(
                session.clone(),
                Session {
                    process,
                    killer: child.clone_killer(),
                    _tree: Tree::owning(pid),
                    screen: Arc::clone(&screen),
                    kept: false,
                    reaped,
                },
            );
        }

        // **Said here, before any thread of the session's exists.** Each of them reports to the same
        // queue this goes into — a title its program sets, its output, its exit — and a client told of
        // any of those first would be reading about a session it has not been given
        let _ =
            self.events.send(Outgoing::answer(Event::Spawned { session: session.clone(), pid, recorded, restored }));

        // **The reader does not decide what a message is.** It hands what one `read` returned to the
        // coalescer, which flushes on a window or a cap — so N bytes of output is not N messages.
        //
        // **The hop is bounded, and that is what makes a slow consumer the program's problem rather than
        // this process's.** When the coalescer is out of credit it stops taking chunks, this queue fills,
        // the reader blocks on it and stops reading the pty, and the program blocks on its own write. An
        // unbounded queue here grew by everything a program wrote while its consumer was stalled. A reader
        // blocked on it still ends: the exit is waited for on the child, and the coalescer drains what is
        // queued whenever credit returns.
        //
        // **A session nobody reads the bytes of has no such hop at all.** Its screen is still fed, which
        // is what its views are drawn from, and nothing stands between the program and the parser.
        let (chunks, incoming) = sync_channel::<Vec<u8>>(READ_AHEAD);
        let chunks = output.then_some(chunks);
        let events = self.events.clone();
        let reading = session.clone();
        let parsing = Arc::clone(&screen);
        let quiet = self.housekeeping.for_session(&session, &screen);
        let waiting = Arc::clone(&waiters);
        let ending = Arc::clone(&waiters);
        let ended = Arc::clone(&screen);
        let waited = session.clone();
        let exiting = self.events.clone();

        if output {
            std::thread::spawn({
                let session = session.clone();
                let events = events.clone();
                let credit = Arc::clone(&self.credit);
                move || coalesce(session, incoming, events, credit)
            });
        }
        // Told when the reader has read the last of the output, which is what the exit waits for
        let (drained, draining) = channel::<()>();
        let reader_events = events.clone();
        let reader_session = session.clone();
        let reading_flow = Arc::clone(&flow);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 8 * 1024];
            loop {
                // **Before the read, which is what makes a pause backpressure.** Parked here the pty is not
                // read, its buffer fills and the process blocks on its own write; nothing is held in the
                // broker. A session whose record is gone closes the gate, so this is never parked for ever
                reading_flow.wait_while_paused();
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        // The authority first, then the renderer's copy. The waits are settled inside this
                        // same lock, in one order everywhere — screen, then waiters — so the two threads
                        // that take both cannot deadlock on each other
                        let (replies, met, effects) = match parsing.lock() {
                            Ok(mut screen) => {
                                screen.feed(&buffer[..read]);
                                let replies = screen.take_replies();
                                let effects = screen.take_effects();
                                let recent = rows_added(&buffer[..read], screen.cols());
                                let met = settle_met(&waiting, &screen, recent);
                                // Each view that has drawn its last frame is sent what changed, here
                                // where the change landed
                                screen.publish();
                                (replies, met, effects)
                            }
                            Err(_) => (Vec::new(), 0, Vec::new()),
                        };
                        // Written to, so not quiet: its history is compressed once it has been (`housekeeping`)
                        quiet.wrote();
                        for effect in effects {
                            let _ = reader_events.send(Outgoing::answer(event_of(&reader_session, effect)));
                        }
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
                        //
                        // **Queued like everything else for the program** (`writer::Input`), so this thread
                        // never waits on a write: a program that asks and does not read the answer would
                        // otherwise stop the reading that lets it carry on. An answer the queue has no room
                        // for is one to a program that has read none of the megabytes before it
                        for reply in replies {
                            let _ = answering.send(reply.into_bytes());
                        }
                        if let Some(chunks) = &chunks
                            && chunks.send(buffer[..read].to_vec()).is_err()
                        {
                            break;
                        }
                    }
                }
            }
            // The coalescer ends when this drops, flushing what it holds, so nothing is lost at EOF
            drop(chunks);
            let _ = drained.send(());
        });

        // **The exit is waited for, not inferred from the output ending.** A POSIX pty gives EOF on the
        // master when the last slave closes, so the reader could report it; a ConPTY does not, because this
        // process still holds the pseudoconsole — so on Windows the read simply never returned and a shell
        // the user exited was never reported at all. The terminal stayed on screen with nothing behind it,
        // which is what X2 calls an undead terminal. Waiting on the child is the one signal both platforms
        // give, and it still polls nothing.
        std::thread::spawn(move || {
            // **A signal and an exit code are different answers, and never both.** `portable-pty` reports a
            // killed child as code 1 with the system's description of the signal, which read on its own says
            // a program exited with a failure — so a consumer could not tell the shutdown it asked for from
            // a crash. What is reported here is one or the other, and the signal by name
            let (code, signal) = match child.wait().ok() {
                Some(status) => match status.signal() {
                    Some(described) => (None, Some(signal_named(described))),
                    None => (Some(status.exit_code() as i32), None),
                },
                None => (None, None),
            };
            // Waited for: whatever was making sure of this ending has nothing left to do
            drop(reaping);
            // Nothing is left to read what is queued for it, and the thread writing it has nothing to wait for
            ending_input.close();
            // Only now: letting go of a pty's writer types into the pty, and until here there was a
            // program that could have read it
            ending_input.release();
            // **Let go of on a thread of its own**: closing a console waits, on the Windows releases that
            // wait at all, for its output to be read to the end, and a reader parked on a pause reads
            // nothing — so the exit waits for the reader below, by its own bound, and never for the close
            let master = ending_pty.lock().ok().and_then(|mut held| held.take());
            std::thread::spawn(move || drop(master));
            let _ = draining.recv_timeout(EXIT_DRAIN);
            // **A command the shell left running is over, and is told before the exit is**: a shell that
            // is told to exit begins a command and never ends it. After the last of the output, so a
            // finish the shell did print is the one that closes it
            let left = ended.lock().map_or_else(
                |_| Vec::new(),
                |mut screen| {
                    screen.session_ended();
                    screen.take_effects()
                },
            );
            for effect in left {
                let _ = exiting.send(Outgoing::answer(event_of(&waited, effect)));
            }
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
            let _ = events.send(Outgoing::answer(Event::Exited { session: reading, code, signal }));
        });

        Ok(())
    }

    /// Stops reading this session's pty, so its process feels the reader stop (`output::Flow`).
    pub fn pause(&self, session: &str) -> Outcome {
        self.record().live.get(session).ok_or_else(|| missing(session))?.process.flow.pause();
        Ok(())
    }

    /// Reads it again, and sends what arrived while it was not being read.
    pub fn resume(&self, session: &str) -> Outcome {
        self.record().live.get(session).ok_or_else(|| missing(session))?.process.flow.resume();
        Ok(())
    }

    /// Lets every session be read again. **What a client leaving calls**: a pause belongs to the client that
    /// asked for one, and the next client to attach never did — so inheriting a session nobody is reading,
    /// with no way to know it, is the one outcome this must not have.
    pub fn resume_all(&self) {
        for live in self.record().live.values() {
            live.process.flow.resume();
        }
    }

    /// The process a session was started as, or the refusal for a session with none.
    fn pid(&self, session: &str) -> Result<u32, Refusal> {
        Ok(self.record().live.get(session).ok_or_else(|| missing(session))?.process.pid)
    }

    /// Whether `pid` is this session's own process or something it started.
    ///
    /// **The same walk the `Pid` scope refuses with**, so the answer a caller reads and the refusal it would
    /// have got cannot disagree — which is the whole reason this is a request rather than something a
    /// caller infers from trying to signal.
    pub fn owns(&self, session: &str, pid: u32) -> Result<bool, Refusal> {
        owned_by(self.pid(session)?, pid)
    }

    /// Sends a signal to a session's process, its group, or one process inside its tree.
    pub fn signal(&self, session: &str, signal: Signal, scope: Scope) -> Outcome {
        deliver(self.pid(session)?, signal, scope)
    }

    /// Waits for a session to reach a state, answering once.
    ///
    /// **A condition that already holds is answered here**, without a waiter: a caller asking whether a
    /// session is idle when it is gets its answer in the same turn, and nothing is registered to be settled
    /// later. Otherwise the waiter goes on the session, where the reader settles it, and a deadline thread
    /// answers `Timeout` if it gets there first.
    ///
    /// **Asked and registered under the screen's lock, in the reader's order** — screen, then waits — so
    /// what the reader parses next either was seen here or finds the waiter there: there is no read
    /// between the two that neither looked at.
    pub fn wait(&self, session: &str, until: Until, timeout_ms: u32) -> Outcome {
        let handle = self.handle(session)?;
        let process = handle.process()?;
        let settled = Arc::new(AtomicBool::new(false));
        let waiter = Waiter { until, settled: Arc::clone(&settled) };

        let holds = handle.on_screen(|screen| -> Result<bool, Refusal> {
            if waiter.holds(screen, HISTORY_LINES as u32) {
                return Ok(true);
            }
            process.waiters.lock().map_err(|_| poisoned("waiting on a session"))?.push(waiter);
            Ok(false)
        })??;
        if holds {
            settled.store(true, Ordering::SeqCst);
            self.answer(Event::Waited { session: session.to_string(), outcome: WaitOutcome::Met });
            return Ok(());
        }

        self.deadlines.set(Duration::from_millis(timeout_ms as u64), session.to_string(), settled);
        Ok(())
    }

    /// Everything a session's ending owes, whichever way it ended: its reader and its input let go, its
    /// views told, and its recording stopped.
    ///
    /// **With the session already out of the record and no lock held**: the views are sent the screen as
    /// it ended, which is not a thing every other session should stand still for. **And nothing here
    /// waits for a disk**: the recording is stopped, and what is still on its way to the file is the
    /// recorder's thread's to finish — whatever records there next waits for it (`recording::claim`).
    ///
    /// **The pty's writer is not let go of**, only stopped (`Input::close`): the thread that waits on the
    /// process lets go of it once the process is gone.
    fn end(&self, live: &Session) {
        // A reader parked on a pause has nobody left to lift it, and its thread would never reach the EOF
        // that ends it
        live.process.flow.close();
        live.process.input.close();
        if let Ok(mut screen) = live.screen.lock() {
            // Before its views are sent the screen as it ended, which says how its commands stand
            screen.session_ended();
            screen.end_views();
            // Its reader goes on until the pty closes, and the session that replaces this one may pick its
            // recording up before then
            screen.stop_recording();
        }
    }

    /// Ends the session. The thread waiting on its process reports `Exited` when it has gone, so this
    /// reports nothing: one exit, one event, whichever way the process ended. **And it does go**: a
    /// program that ignores the hangup this sends is killed `KILL_GRACE` later (`end_for_certain`).
    pub fn kill(&self, session: &str) -> Outcome {
        let (live, rewound) = {
            let mut record = self.record();
            (record.live.remove(session), record.rewound.remove(session))
        };
        if let Some(rewound) = rewound {
            rewound.end();
            return Ok(());
        }
        let mut live = live.ok_or_else(|| missing(session))?;
        // Its jobs first, while the shell that could say whose they are is still there
        hang_up_the_rest(&[live.process.pid]);
        // **Killed first, and only then tidied up after.** Between the two its program can still read,
        // and nothing that ending a session does may reach a program that is being ended
        let killed = live.killer.kill().map_err(|error| Refusal::pty("killing a session", error));
        self.end(&live);
        // What was sent is a hangup, which a program may ignore
        end_for_certain(live.process.pid, live.reaped);
        killed
    }

    /// Drops the record of a session whose process has ended, which the writer thread does when it sees the
    /// `Exited` it is about to send. Nothing is killed here: the process is already gone.
    pub fn forget(&self, session: &str) {
        let live = self.record().live.remove(session);
        if let Some(live) = live {
            self.end(&live);
        }
    }

    /// Ends every session this owner holds. **The connection's end is what calls it**: a client that
    /// disconnects must not leave its shells running, and the app cannot do it on the way out — a pack's
    /// `onShutdown` is synchronous and runs in `process.on('exit')`, where no socket write can complete. So
    /// the broker is the backstop for an app that quits, crashes or is killed, and the app's own `killAll`
    /// is only the orderly path.
    ///
    /// Whether a client leaving ends its sessions is `Role`'s question (`main.rs`); this is what it calls
    /// when the answer is yes.
    ///
    /// **A session that was asked to be kept is not ended** (`Request::Keep`): it is what its name says.
    pub fn end_unkept(&self) {
        let (ending, rewound) = {
            let mut record = self.record();
            let unkept: Vec<SessionId> =
                record.live.iter().filter(|(_, live)| !live.kept).map(|(session, _)| session.clone()).collect();
            let ending: Vec<Session> = unkept.into_iter().filter_map(|session| record.live.remove(&session)).collect();
            // A recording being looked at is looked at by somebody, and they have gone
            let rewound: Vec<Rewound> = record.rewound.drain().map(|(_, rewound)| rewound).collect();
            (ending, rewound)
        };
        let leaders: Vec<u32> = ending.iter().map(|live| live.process.pid).collect();
        hang_up_the_rest(&leaders);
        for mut live in ending {
            let _ = live.killer.kill();
            self.end(&live);
            end_for_certain(live.process.pid, live.reaped);
        }
        for rewound in rewound {
            rewound.end();
        }
    }

    /// Ends every recording a connection opened to look back through, when the connection goes.
    ///
    /// **A rewound session is its opener's.** It holds a screen and a file and is no process, so nothing
    /// ends it of its own accord; whoever opened it is the only one who knows its id, and a client that
    /// reconnects opens what it wants again.
    pub fn end_rewound_by(&self, connection: u64) {
        let ending: Vec<Rewound> = {
            let mut record = self.record();
            let opened: Vec<SessionId> = record
                .rewound
                .iter()
                .filter(|(_, rewound)| rewound.opener == connection)
                .map(|(session, _)| session.clone())
                .collect();
            opened.iter().filter_map(|session| record.rewound.remove(session)).collect()
        };
        for rewound in ending {
            rewound.end();
        }
    }

    /// Whether no session with a process is left, which is when a broker nobody is connected to has
    /// nothing to stay for.
    pub fn none_live(&self) -> bool {
        self.record().live.is_empty()
    }

    /// Says whether a session outlives the broker's holder.
    pub fn keep(&self, session: &str, kept: bool) -> Outcome {
        self.record().live.get_mut(session).ok_or_else(|| missing(session))?.kept = kept;
        Ok(())
    }

    /// Every session with a process, oldest first.
    pub fn summaries(&self) -> Vec<SessionSummary> {
        // Who there is, under the record's lock; what each is doing, from its own screen after
        let sessions: Vec<(SessionId, u32, bool, Arc<Mutex<Screen>>)> = self
            .record()
            .live
            .iter()
            .map(|(session, live)| (session.clone(), live.process.pid, live.kept, Arc::clone(&live.screen)))
            .collect();
        let mut all: Vec<SessionSummary> = sessions
            .into_iter()
            .filter_map(|(session, pid, kept, screen)| {
                let screen = screen.lock().ok()?;
                let facts = screen.facts();
                Some(SessionSummary {
                    session,
                    pid,
                    cols: screen.cols(),
                    rows: screen.rows(),
                    kept,
                    recorded: screen.recorded(),
                    title: screen.title().map(str::to_owned),
                    cwd: facts.cwd.clone(),
                    command: facts.command.clone(),
                    running: facts.running,
                })
            })
            .collect();
        // `s-<n>`, in the order they were started
        all.sort_by_key(|summary| summary.session.trim_start_matches("s-").parse::<u64>().unwrap_or(u64::MAX));
        all
    }

    /// How many recordings have a session named against them, which is how many files are held open for
    /// writing or waited on by a rewind. Bounded by the sessions that are running, not by every recording
    /// the broker has ever made.
    pub fn recorders(&self) -> usize {
        self.record().recording.len()
    }

    /// Opens a recording as a session showing its end, answering the id and the moments it runs between.
    /// `opener` is the connection that asked, which is whose it is (`end_rewound_by`).
    ///
    /// **Read and replayed before the session is anybody's to ask after**, with no lock held.
    pub fn rewind(&self, opener: u64, recording: &str, cols: u16, rows: u16) -> Result<(SessionId, u64, u64), Refusal> {
        let path = recording::canonical(Path::new(recording));
        // A session still recording there has entries on their way to the file: what is looked back
        // through is everything it has recorded, not everything that has reached the disk. Waited for
        // with that session's screen let go, and one that has just stopped recording there likewise
        let recorder = self.record().recording.get(&path).cloned();
        let reached = recorder
            .and_then(|session| self.handle(&session).ok())
            .and_then(|recorder| recorder.on_screen(Screen::recording_reached).ok().flatten());
        // **For so long and no longer** (`RECORDER_PATIENCE`): what is waited for is a disk, and what
        // waits is a connection with other requests behind this one. A recording that is not whole by
        // then is not looked back through, and whoever asked is told why
        let unwritten = || Refusal {
            code: ErrorCode::Pty,
            message: format!("{recording} is still being written, and was not looked back through; ask again"),
        };
        if let Some(reached) = reached
            && reached.recv_timeout(recording::RECORDER_PATIENCE) == Err(RecvTimeoutError::Timeout)
        {
            return Err(unwritten());
        }
        if !recording::settled(&path) {
            return Err(unwritten());
        }
        let nothing = || Refusal {
            code: ErrorCode::NoSuchSession,
            message: format!("{recording} holds no recording to look back through"),
        };
        recording::check(&path).map_err(|why| Refusal {
            code: ErrorCode::NoSuchSession,
            message: format!("{recording} is not a recording this broker reads: {why}"),
        })?;
        let mut index = recording::Index::open(&path).ok_or_else(nothing)?;
        let (first, last) = (index.first(), index.last());
        let entries = index.moment(last).map_err(|error| Refusal::pty("reading a recording", error))?;
        let (cols, rows) = grid(cols, rows);
        let mut screen = Screen::new(cols, rows);
        screen.replay(&entries, last);
        screen.resize(cols, rows, 0, 0);
        let screen = Arc::new(Mutex::new(screen));
        if let Ok(mut held) = screen.lock() {
            held.limit_holds(self.holds.for_screen(Arc::downgrade(&screen)));
        }
        let mut record = self.record();
        record.next += 1;
        let session = format!("r-{}", record.next);
        record.rewound.insert(
            session.clone(),
            Rewound { screen, look: Arc::new(Mutex::new(Look { index, size: (cols, rows) })), opener },
        );
        Ok((session, first, last))
    }

    /// Every view a connection had, in every session, when the connection goes.
    pub fn detach_connection(&self, connection: u64) {
        let screens: Vec<Arc<Mutex<Screen>>> = {
            let record = self.record();
            record
                .live
                .values()
                .map(|live| &live.screen)
                .chain(record.rewound.values().map(|rewound| &rewound.screen))
                .map(Arc::clone)
                .collect()
        };
        for screen in screens {
            if let Ok(mut screen) = screen.lock() {
                screen.detach_connection(connection);
            }
        }
    }
}

/// Something a program did that is not on its screen, as the event a client is told it with.
fn event_of(session: &str, effect: Effect) -> Event {
    let session = session.to_owned();
    match effect {
        Effect::Title(title) => Event::Title { session, title },
        Effect::Bell => Event::Bell { session },
        Effect::Clipboard(text) => Event::Clipboard { session, text },
        Effect::Notification { title, body } => Event::Notification { session, title, body },
        Effect::Progress { state, percent } => Event::Progress {
            session,
            state: match state {
                Progress::None => ProgressState::None,
                Progress::Running => ProgressState::Running,
                Progress::Error => ProgressState::Error,
                Progress::Indeterminate => ProgressState::Indeterminate,
                Progress::Paused => ProgressState::Paused,
            },
            percent,
        },
        Effect::CommandStarted { command } => Event::CommandStarted { session, command },
        Effect::CommandFinished { outcome } => Event::CommandFinished { session, outcome: outcome.into() },
        Effect::CwdChanged(cwd) => Event::CwdChanged { session, cwd },
    }
}

fn missing(session: &str) -> Refusal {
    Refusal { code: ErrorCode::NoSuchSession, message: format!("no session {session}") }
}
