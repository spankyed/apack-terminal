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
//! This holds the bookkeeping and nothing of the terminal: `M` is whatever marks a row, which is the
//! core's own tracked reference in a session and a plain number in this module's cases.

use std::collections::VecDeque;
use std::time::SystemTime;

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

/// One command, as far as the shell has reported it.
pub struct Command<M> {
    /// Counts from one, and is never reused in a session
    pub id: u64,
    /// The line the shell read, where it said
    pub command: Option<String>,
    /// Where the shell was when it began
    pub cwd: Option<String>,
    pub started: Option<SystemTime>,
    pub finished: Option<SystemTime>,
    /// Absent while it runs, and when the shell reported none: a shell that says nothing about how a
    /// command ended is not saying it succeeded
    pub exit_status: Option<i32>,
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
            exit_status: None,
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
        self.ended(None, now);
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

    /// The command that was running ended. With none running there is nothing to end.
    pub fn ended(&mut self, exit_status: Option<i32>, now: SystemTime) {
        let Some(current) = self.kept.back_mut().filter(|last| last.running()) else { return };
        current.finished = Some(now);
        current.exit_status = exit_status;
        let id = current.id;
        self.pending.push((id, Moment::End));
        self.changed();
    }

    /// Whether a moment has been reported that has not been given its row yet.
    pub fn moments_waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Gives every moment reported since the last call the row `mark` makes, in the order they happened.
    ///
    /// Called where the terminal is exactly as it was when the shell wrote the marker, so each mark is
    /// made at the cursor's row at that moment.
    pub fn place(&mut self, mut mark: impl FnMut() -> Option<M>) {
        for (id, moment) in std::mem::take(&mut self.pending) {
            let Some(command) = self.kept.iter_mut().find(|command| command.id == id) else { continue };
            let slot = match moment {
                Moment::Prompt => &mut command.prompt,
                Moment::Output => &mut command.output,
                Moment::End => &mut command.end,
            };
            *slot = mark();
        }
    }
}
