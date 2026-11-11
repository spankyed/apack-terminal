//! Compressing the history of sessions that have gone quiet.
//!
//! **A long history is affordable because of what it costs when nobody is writing to it.** A session keeps
//! a hundred thousand lines (`screen::VIEW_HISTORY_LINES`), which is tens of megabytes while a program
//! fills them and about one once the core has compressed them. Compressing is the core's; deciding *when*
//! is the broker's, and the answer is: once a session has been written to and then left alone.
//!
//! **One thread, woken by a session being written to, and asleep when nothing is waiting.** A session's
//! reader says so the first time it writes after its history was last compressed; from then this waits out
//! `QUIET`, and if the terminal did nothing in that time compresses it a step at a time. With no session
//! waiting there is no timer at all — an idle broker does not wake to find nothing to do.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::screen::Screen;

/// How long a session is left alone before its history is compressed.
///
/// A program that pauses for a moment is about to write again, and compressing a page it is about to
/// scroll past is work thrown away; a session nobody has written to for this long is one a person has
/// stopped watching scroll.
pub const QUIET: Duration = Duration::from_secs(2);

/// A session's say in when it is compressed: told when it is written to.
#[derive(Clone)]
pub struct Quiet {
    notes: Sender<Note>,
    /// Whether this session has already said it was written to since it was last compressed, so a
    /// program writing flat out says so once rather than once a read
    noted: Arc<AtomicBool>,
    screen: Weak<Mutex<Screen>>,
    session: Arc<str>,
}

struct Note {
    session: Arc<str>,
    screen: Weak<Mutex<Screen>>,
    noted: Arc<AtomicBool>,
}

impl Quiet {
    /// The session was written to. Cheap enough to call after every read.
    pub fn wrote(&self) {
        if self.noted.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.notes.send(Note {
            session: Arc::clone(&self.session),
            screen: Weak::clone(&self.screen),
            noted: Arc::clone(&self.noted),
        });
    }
}

/// The thread that compresses, and the way a session is given its say in it.
#[derive(Clone)]
pub struct Housekeeping {
    notes: Sender<Note>,
}

impl Housekeeping {
    /// Starts the thread. It ends when every handle to it has gone.
    pub fn start() -> Self {
        Self::start_with(QUIET, None)
    }

    /// The same, with how long a session is left alone given, and somewhere to say which session was just
    /// compressed. For a case, which cannot wait out the real delay or see compression happen otherwise.
    pub fn start_with(quiet: Duration, compressed: Option<Sender<String>>) -> Self {
        let (notes, noted) = channel();
        std::thread::spawn(move || keep(&noted, quiet, compressed.as_ref()));
        Self { notes }
    }

    /// What a session's reader holds to say when it wrote.
    pub fn for_session(&self, session: &str, screen: &Arc<Mutex<Screen>>) -> Quiet {
        Quiet {
            notes: self.notes.clone(),
            noted: Arc::new(AtomicBool::new(false)),
            screen: Arc::downgrade(screen),
            session: Arc::from(session),
        }
    }
}

struct Waiting {
    screen: Weak<Mutex<Screen>>,
    noted: Arc<AtomicBool>,
    /// When to look at it next
    due: Instant,
    /// What the terminal's activity was when it was last looked at
    activity: Option<u64>,
}

fn keep(notes: &Receiver<Note>, quiet: Duration, compressed: Option<&Sender<String>>) {
    let mut waiting: HashMap<Arc<str>, Waiting> = HashMap::new();
    loop {
        let next = waiting.values().map(|one| one.due).min();
        let note = match next {
            // Nothing is waiting: asleep until a session is written to
            None => match notes.recv() {
                Ok(note) => Some(note),
                Err(_) => return,
            },
            Some(due) => match notes.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(note) => Some(note),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        if let Some(note) = note {
            waiting.insert(
                note.session,
                Waiting { screen: note.screen, noted: note.noted, due: Instant::now() + quiet, activity: None },
            );
            continue;
        }

        let now = Instant::now();
        waiting.retain(|session, one| {
            if one.due > now {
                return true;
            }
            // A session that has ended has nothing left to compress
            let Some(screen) = one.screen.upgrade() else { return false };
            let Ok(mut screen) = screen.lock() else { return false };
            let activity = screen.compression_activity();
            if one.activity != Some(activity) {
                // It did something since it was last looked at, so it is not quiet yet
                one.activity = Some(activity);
                one.due = now + quiet;
                return true;
            }
            // **One step, then the lock is let go.** A step is bounded, so a program that starts writing
            // again waits for one of them at most; more to do comes round again at once
            if screen.compress_step() {
                one.due = now;
                return true;
            }
            // Done. The next write says so again, and the wait starts over
            one.noted.store(false, Ordering::Release);
            if let Some(compressed) = compressed {
                let _ = compressed.send(session.to_string());
            }
            false
        });
    }
}
