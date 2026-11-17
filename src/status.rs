//! What a session's programs say they are doing (OSC 7501, the Program Status Protocol), kept by the
//! rules its specification leaves to the terminal: <https://www.superlogical.com/rex/docs/build/program-status>.
//!
//! The core reads a report and checks it against the specification; it keeps nothing. This is what is
//! kept, and the only place the rules for keeping it are written: every client is sent the records as
//! they stand, whole, and merges nothing.
//!
//! **A record is a claim.** Any program in a pty can write a report, and one naming another program is
//! as well formed as an honest one. `Confirmed` says what the broker could check of it — whether the
//! program in front of the pty is the one the record names — and nothing here acts on a record.

use std::time::SystemTime;

/// The most records a session holds. The specification's limit; past it the record changed longest ago
/// is the one let go.
pub const CAP: usize = 256;

/// What a program says it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// At rest, waiting for the person's next instruction
    Idle,
    Working,
    /// Cannot continue until the person does something
    Blocked,
    /// Finished a piece of work the person has not looked at yet
    Done,
    /// Failed and stopped
    Error,
}

/// What a blocked program needs from the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

/// Whether the program in front of the pty is the one a record names, as of the read that took the
/// record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmed {
    /// The foreground process group's leader is the program the record's `app` names
    Matched,
    /// A leader was read, and it is another program
    Mismatched,
    /// Nothing to compare: the record names no program, no leader could be read, or the platform has no
    /// foreground process group
    Unknown,
}

/// One report, as the core hands it over once it has passed every check of the specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `None` for a report that clears
    pub state: Option<State>,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    /// Which record the report is about. Empty for the program itself
    pub id: String,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// One record, as a client is told it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    pub state: State,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    /// The program the record names: its own, or its nearest ancestor's where it gave none
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
    /// Moved by every report that changed a record. Later is larger, within one session
    pub changed: u32,
    /// When the record entered the state it is in. A report that keeps the state keeps this
    pub since: SystemTime,
    pub confirmed: Confirmed,
}

struct Held {
    record: Record,
    /// Taken by a read whose foreground program has not been looked at yet
    fresh: bool,
}

/// A session's records. Held in the order they last changed, oldest first, which is the order the cap
/// lets go of them in.
#[derive(Default)]
pub struct Records {
    held: Vec<Held>,
    counter: u32,
    stirred: bool,
}

impl Records {
    /// Takes one report.
    ///
    /// **A report replaces its record whole**: what it leaves out is gone, and only the time the record
    /// entered its state carries over, when the state is the one it already had. A report that says
    /// exactly what its record says changes nothing, so a program repeating itself is told once.
    pub fn apply(&mut self, report: Report, now: SystemTime) {
        let Some(state) = report.state else {
            return self.clear(&report.id);
        };
        let at = self.held.iter().position(|held| held.record.id == report.id);
        let since = match at.map(|at| &self.held[at].record) {
            Some(was) if was.state == state => was.since,
            _ => now,
        };
        if let Some(was) = at.map(|at| &self.held[at].record)
            && was.kind == report.kind
            && was.progress == report.progress
            && was.app == report.app
            && was.title == report.title
            && was.message == report.message
            && was.state == state
        {
            return;
        }
        if let Some(at) = at {
            self.held.remove(at);
        }
        self.counter = self.counter.wrapping_add(1);
        self.held.push(Held {
            record: Record {
                id: report.id,
                state,
                kind: report.kind,
                progress: report.progress,
                app: report.app,
                title: report.title,
                message: report.message,
                changed: self.counter,
                since,
                confirmed: Confirmed::Unknown,
            },
            fresh: true,
        });
        if self.held.len() > CAP {
            self.held.remove(0);
        }
        self.stirred = true;
    }

    /// Removes the record `id` names and every record beneath it, or every record when it names none.
    fn clear(&mut self, id: &str) {
        let before = self.held.len();
        self.held.retain(|held| !(id.is_empty() || held.record.id == id || beneath(&held.record.id, id)));
        self.stirred |= self.held.len() != before;
    }

    /// The program the records were about is gone: its command ended, or its session did. What it was
    /// in the middle of is over with it; what it finished or failed at is kept until the person has
    /// looked.
    pub fn ended(&mut self) {
        let before = self.held.len();
        self.held.retain(|held| matches!(held.record.state, State::Done | State::Error));
        self.stirred |= self.held.len() != before;
    }

    /// Says, of every record taken since the last time this was called, whether the program in front
    /// is the one it names. `front` is the foreground process group leader's arguments, where one
    /// could be read.
    pub fn confirm(&mut self, front: Option<&[String]>) {
        let fresh: Vec<usize> = (0..self.held.len()).filter(|&at| self.held[at].fresh).collect();
        for at in fresh {
            let app = self.app_of(&self.held[at].record.id);
            let held = &mut self.held[at];
            held.record.confirmed = confirmed(app.as_deref(), front);
            held.fresh = false;
        }
    }

    /// Whether a record has changed since this was last asked. Asking takes it.
    pub fn take_stirred(&mut self) -> bool {
        std::mem::take(&mut self.stirred)
    }

    /// Every record as a client is told it, by id, each with the program it names or inherits.
    pub fn told(&self) -> Vec<Record> {
        let mut told: Vec<Record> =
            self.held.iter().map(|held| Record { app: self.app_of(&held.record.id), ..held.record.clone() }).collect();
        told.sort_by(|a, b| a.id.cmp(&b.id));
        told
    }

    /// What the program itself, rather than anything it reports on, says it is doing.
    pub fn root(&self) -> Option<State> {
        self.held.iter().find(|held| held.record.id.is_empty()).map(|held| held.record.state)
    }

    /// The program a record names: its own, or its nearest ancestor's that names one. The program's own
    /// record, with no id, is every record's ancestor.
    fn app_of(&self, id: &str) -> Option<String> {
        let own =
            |id: &str| self.held.iter().find(|held| held.record.id == id).and_then(|held| held.record.app.clone());
        let mut at = id;
        loop {
            if let Some(app) = own(at) {
                return Some(app);
            }
            if at.is_empty() {
                return None;
            }
            at = at.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }
}

/// Whether the record `id` is beneath the record `of`: `build/test` is beneath `build`.
fn beneath(id: &str, of: &str) -> bool {
    id.strip_prefix(of).is_some_and(|rest| rest.starts_with('/'))
}

/// The names a program is run by, where they differ from the name it reports as.
const ALIASES: &[(&str, &[&str])] = &[("claude-code", &["claude"])];

/// The programs that run another program's script, so that what is in front is the script and not them.
const INTERPRETERS: &[&str] = &["node", "bun", "deno", "python", "python3"];

/// Whether `front`, a foreground process group leader's arguments, is the program `app` names.
pub fn confirmed(app: Option<&str>, front: Option<&[String]>) -> Confirmed {
    let (Some(app), Some(name)) = (app, front.and_then(program)) else {
        return Confirmed::Unknown;
    };
    let named = |candidate: &str| candidate.eq_ignore_ascii_case(&name);
    let aliased = ALIASES
        .iter()
        .any(|(of, names)| of.eq_ignore_ascii_case(app) && names.iter().any(|candidate| named(candidate)));
    if named(app) || aliased { Confirmed::Matched } else { Confirmed::Mismatched }
}

/// The program a process's arguments say it is: what it was run as, or the script it was run with when
/// what it was run as is an interpreter.
fn program(argv: &[String]) -> Option<String> {
    // A login shell is run as `-zsh`
    let run_as = base(argv.first()?.trim_start_matches('-'));
    let run_as = run_as.strip_suffix(".exe").unwrap_or(run_as);
    if run_as.is_empty() {
        return None;
    }
    if INTERPRETERS.iter().any(|interpreter| interpreter.eq_ignore_ascii_case(run_as))
        && let Some(script) = argv.iter().skip(1).find(|argument| !argument.starts_with('-'))
    {
        let script = base(script);
        return Some(script.rsplit_once('.').map_or(script, |(stem, _)| stem).to_owned());
    }
    Some(run_as.to_owned())
}

/// The last part of a path, whichever way its separators lean.
fn base(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
