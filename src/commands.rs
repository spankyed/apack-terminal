//! The commands a shell has run in a session: what each was, where, when, how it ended, and which rows
//! of the terminal are its.
//!
//! **A record, because a fact about the last command is not enough.** `Facts` says what the shell is doing
//! now; a person scrolling back wants the command that printed what they are looking at, and a caller that
//! ran something wants *its* output rather than whatever is on the screen. So each command the shell
//! reports is kept, with a mark on the row its prompt, its output and its end were at.
//!
//! **A mark is taken at the moment the shell said so.** The shell's markers are sequences in the stream,
//! and the terminal stops its parse at each one (`Screen::feed`), so the cursor is exactly where it was
//! when the marker was written — not wherever the rest of that read left it.
//!
//! **How a command ended is one value, and it says who said so** (`Outcome`). A status a keyed mark
//! carried is an exit code; a status anything could have printed is a claim of one, and no reader of a
//! record is handed the second where it asked for the first.
//!
//! This holds the bookkeeping and nothing of the terminal: `M` is whatever marks a row, which is the
//! core's own tracked reference in a session and a plain number in this module's cases.

use std::collections::VecDeque;
use std::time::SystemTime;

use crate::markers::{ExitCode, Said, Vouched};

/// How many commands a session remembers. The oldest goes when a new one arrives past this.
pub const COMMANDS_KEPT: usize = 256;

/// A point in a command's life that has a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// The shell began drawing the prompt the command was typed at
    Prompt,
    /// The command began: what follows is its output
    Output,
    /// The command ended
    End,
}

/// How a command ended, or that it has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It has not ended.
    Running,
    /// It exited with this status, said by a mark that carried the session's key.
    Exited(ExitCode),
    /// It is over, and nothing that could be believed said how.
    Unknown(Unknown),
    /// Something printed that it ended, with this status where it gave one. What a session spawned
    /// without a key has for every command that ended: a claim, which is not a status.
    Claimed(Option<i32>),
}

/// Why a keyed session's command is over without an exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknown {
    /// The shell reported its end and had no status to give.
    NoStatus,
    /// A later command began while it was still running: its end was never reported.
    Superseded,
    /// The session ended while it was running.
    SessionEnded,
}

/// One command, as far as the shell has reported it.
pub struct Command<M> {
    /// Counts from one, and is never reused in a session
    pub id: u64,
    /// The line the shell read, where it said
    pub command: Option<String>,
    /// Where the shell was: when the command began, as a session without a key hears it, and when it
    /// ended, which is what a keyed finish carries
    pub cwd: Option<String>,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    pub outcome: Outcome,
    pub prompt: Option<M>,
    pub output: Option<M>,
    pub end: Option<M>,
}

impl<M> Command<M> {
    fn new(id: u64) -> Self {
        Self {
            id,
            command: None,
            cwd: None,
            started: None,
            finished: None,
            outcome: Outcome::Running,
            prompt: None,
            output: None,
            end: None,
        }
    }

    /// The command began and has not ended.
    pub fn running(&self) -> bool {
        self.started.is_some() && self.finished.is_none()
    }
}

/// What a keyed session's marks have established: nothing is running, or the command of this number is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyed {
    Idle,
    Running(u64),
}

/// What a keyed mark that was accepted did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A command began, with its line. `superseded` says one was still running, which is over without
    /// its end ever having been reported.
    Started { line: String, superseded: bool },
    /// The running command ended this way, with the shell where it said.
    Finished { outcome: Outcome, cwd: Option<String> },
}

/// A keyed session's commands as its marks number them: which is running, and the last that began.
///
/// **Only a `Vouched` moves it**, and a `Vouched` exists only for a mark that carried the session's key
/// (`markers::read`). **Numbers only go up.** The key is in the terminal's own output — every mark
/// carries it — so a log of the session shown again, or its recording printed, is a stream of marks with
/// the right key; what makes them the past is that their numbers are not past the last one accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    state: Keyed,
    /// The number of the last start accepted. Zero before any, which no shell numbers a command.
    last: u64,
}

impl Default for Machine {
    fn default() -> Self {
        Self::after(0)
    }
}

impl Machine {
    /// A machine with nothing running, whose last accepted start was `last`.
    pub fn after(last: u64) -> Self {
        Self { state: Keyed::Idle, last }
    }

    /// The number of the last start accepted.
    pub fn last(&self) -> u64 {
        self.last
    }

    /// Takes one keyed mark, answering what it did, or nothing for a mark that is refused.
    ///
    /// A start is accepted when its number is past the last accepted, whatever is running: a command
    /// still running then is over, its end unreported. A finish is accepted for the command that is
    /// running and for no other. Everything else is refused and leaves this as it was.
    pub fn take(&mut self, mark: Vouched) -> Option<Step> {
        let number = mark.number();
        match (self.state, mark.into_said()) {
            (state, Said::Start { line }) if number > self.last => {
                self.last = number;
                self.state = Keyed::Running(number);
                Some(Step::Started { line, superseded: state != Keyed::Idle })
            }
            (Keyed::Running(running), Said::Finish { status, cwd }) if running == number => {
                self.state = Keyed::Idle;
                let outcome = status.map_or(Outcome::Unknown(Unknown::NoStatus), Outcome::Exited);
                Some(Step::Finished { outcome, cwd })
            }
            (Keyed::Idle | Keyed::Running(_), Said::Start { .. } | Said::Finish { .. }) => None,
        }
    }

    /// The session is over. Answers whether a command was running, which now is not.
    pub fn end(&mut self) -> bool {
        std::mem::replace(&mut self.state, Keyed::Idle) != Keyed::Idle
    }
}

/// A session's commands, oldest first.
pub struct Commands<M> {
    next: u64,
    kept: VecDeque<Command<M>>,
    /// Moves whenever anything here does, which is how something watching knows to look again
    generation: u32,
    /// The moments reported since rows were last given to them, in order
    pending: Vec<(u64, Moment)>,
}

impl<M> Default for Commands<M> {
    fn default() -> Self {
        Self { next: 1, kept: VecDeque::new(), generation: 0, pending: Vec::new() }
    }
}

impl<M> Commands<M> {
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn iter(&self) -> impl Iterator<Item = &Command<M>> {
        self.kept.iter()
    }

    pub fn get(&self, id: u64) -> Option<&Command<M>> {
        self.kept.iter().find(|command| command.id == id)
    }

    fn changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// Forgets every command and counts them from one again, for a terminal about to be rebuilt from a
    /// recording that will report them afresh.
    pub fn clear(&mut self) {
        self.kept.clear();
        self.pending.clear();
        self.next = 1;
        self.changed();
    }

    /// Gives the running command the line the shell read, for a shell that says so after the command
    /// has started rather than before. With none running there is nothing to name.
    pub fn name_running(&mut self, line: String) {
        let Some(current) = self.kept.back_mut().filter(|last| last.running()) else { return };
        current.command = Some(line);
        self.changed();
    }

    fn begin(&mut self) -> &mut Command<M> {
        let id = self.next;
        self.next += 1;
        if self.kept.len() == COMMANDS_KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(Command::new(id));
        self.kept.back_mut().expect("a command was just added")
    }

    /// The shell is drawing a prompt.
    ///
    /// Whatever was running has ended, whether or not the shell said so. A prompt drawn again with nothing
    /// run since — an empty line entered, a redraw — is the same command still waiting to be typed, and
    /// moves to where the prompt now is.
    pub fn prompt_started(&mut self, now: SystemTime) {
        self.ended(Outcome::Claimed(None), None, now);
        let waiting = self.kept.back().is_some_and(|last| last.started.is_none());
        let id = if waiting { self.kept.back().map_or(0, |last| last.id) } else { self.begin().id };
        self.pending.push((id, Moment::Prompt));
        self.changed();
    }

    /// A command began, with the line the shell read where it said and where the shell is.
    pub fn output_started(&mut self, command: Option<String>, cwd: Option<String>, now: SystemTime) {
        // A shell that marks no prompts still marks its commands: the record begins here
        let waiting = self.kept.back().is_some_and(|last| last.started.is_none());
        if !waiting {
            self.begin();
        }
        let Some(current) = self.kept.back_mut() else { return };
        current.command = command;
        current.cwd = cwd;
        current.started = Some(now);
        let id = current.id;
        self.pending.push((id, Moment::Output));
        self.changed();
    }

    /// A command began with this line, as a keyed start says: a record of its own, whatever came before.
    ///
    /// A record still waiting at a prompt was never a command — a session without a key left it, as one
    /// replayed from a recording does — and goes rather than being taken for this one's.
    pub fn began(&mut self, line: String, now: SystemTime) {
        if self.kept.back().is_some_and(|last| last.started.is_none()) {
            self.kept.pop_back();
        }
        let current = self.begin();
        current.command = Some(line);
        current.started = Some(now);
        let id = current.id;
        self.pending.push((id, Moment::Output));
        self.changed();
    }

    /// The command that was running ended this way, and where the shell was if whatever ended it said.
    /// With none running there is nothing to end.
    pub fn ended(&mut self, outcome: Outcome, cwd: Option<String>, now: SystemTime) {
        let Some(current) = self.kept.back_mut().filter(|last| last.running()) else { return };
        current.finished = Some(now);
        current.outcome = outcome;
        if cwd.is_some() {
            current.cwd = cwd;
        }
        let id = current.id;
        self.pending.push((id, Moment::End));
        self.changed();
    }

    /// Whether a moment has been reported that has not been given its row yet.
    pub fn moments_waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Gives every moment reported since the last call the mark `mark` makes for it, in the order they
    /// happened.
    ///
    /// Called where the terminal is exactly as it was when the shell wrote the marker, so each mark is
    /// made where the cursor was at that moment.
    pub fn place(&mut self, mut mark: impl FnMut(Moment) -> Option<M>) {
        for (id, moment) in std::mem::take(&mut self.pending) {
            let Some(command) = self.kept.iter_mut().find(|command| command.id == id) else { continue };
            let slot = match moment {
                Moment::Prompt => &mut command.prompt,
                Moment::Output => &mut command.output,
                Moment::End => &mut command.end,
            };
            *slot = mark(moment);
        }
    }
}
