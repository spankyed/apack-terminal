//! What a broker or a process says, heard on a thread of its own so that **every wait for it is bounded by
//! silence** and a wait that runs out says where it was and what came last.
//!
//! **A window of silence, not a budget.** Everything heard resets it, so a slow machine makes a wait longer
//! and never makes it fail; what spends it is something that has stopped answering, which is the thing worth
//! failing over. And it fails as a case's own panic, naming the line that waited and the last few things
//! heard, well before the run's deadline would end the case with nothing but how long it took — a lost
//! signal and a slow machine read differently, which is the whole of diagnosing one.
//!
//! **The reading is done here.** A case listens through a `Heard` and writes to the stream itself, so the
//! one way to wait for a broker is the bounded one, and a file of cases that needs a connection starts from
//! this rather than from a `read_line` of its own — which is how one file's cases came to block for ever. The
//! bound lives on a thread and a channel rather than on the socket because `interprocess` refuses a receive
//! timeout on a Windows named pipe: one mechanism on both platforms, which cannot quietly not apply on one.
//!
//! Not a test target: cargo builds the files directly under `tests/`, and a subdirectory is only reachable
//! through the `#[path]` the suite's root uses.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt::Debug;
use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel, sync_channel};
use std::time::Duration;

use apack_terminal::view::wire::{Incoming, ToView, read_message};
use serde::de::DeserializeOwned;

/// How long whatever is heard may say **nothing at all** before a wait for it fails.
///
/// **The longest the files it replaced used, and shorter than the runner's deadline for a case**
/// (`.config/nextest.toml`), so a wait that runs out is the case's own failure, naming what it waited for. Ten
/// seconds, which one of them used, let a view wait for a shell's prompt run out under forty busy loops on ten
/// cores, which a slow machine and not a lost signal is.
pub const QUIET: Duration = Duration::from_secs(15);

/// How many of the last things heard a failure shows
const REMEMBERED: usize = 4;
/// How much of each one it shows: enough to know it, not a megabyte of output
const SHOWN: usize = 300;

/// What a broker or a process has said, waiting to be taken.
pub struct Heard<T> {
    incoming: Receiver<T>,
    /// Behind a cell so a case listens through a shared reference, as it would read a channel
    recent: RefCell<VecDeque<T>>,
    /// For one read at the case's pace: what asks the thread to read the next thing
    wants: Option<Sender<()>>,
    /// Whether it has been asked and has not answered yet, so a wait that ran out does not ask twice
    asked: Cell<bool>,
}

impl<T: Clone + Debug + Send + 'static> Heard<T> {
    /// Hears whatever `decode` makes of `source` until it answers `None`, as fast as `source` says it: what
    /// the case has not taken yet waits here, so the far end is never held up by the case's pace.
    pub fn reading<R: Read + Send + 'static>(
        source: R,
        mut decode: impl FnMut(&mut BufReader<R>) -> Option<T> + Send + 'static,
    ) -> Self {
        let (sending, incoming) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(source);
            while let Some(heard) = decode(&mut reader) {
                // A send that fails means the case has finished listening
                if sending.send(heard).is_err() {
                    return;
                }
            }
        });
        Self { incoming, recent: RefCell::default(), wants: None, asked: Cell::new(false) }
    }

    /// The same, read **at the case's pace**: nothing is read until the case asks for the next thing, so the
    /// far end sees exactly the reading the case does. For a case whose subject is what the far end does
    /// while it is not being read — backpressure, a channel filling — which a reader running ahead of it
    /// would quietly empty: a saturating producer read that way is never throttled.
    pub fn paced<R: Read + Send + 'static>(
        source: R,
        mut decode: impl FnMut(&mut BufReader<R>) -> Option<T> + Send + 'static,
    ) -> Self {
        let (wants, wanted) = channel::<()>();
        let (sending, incoming) = sync_channel(1);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(source);
            // An ask that cannot be received means the case has finished listening
            while wanted.recv().is_ok() {
                let Some(heard) = decode(&mut reader) else { return };
                if sending.send(heard).is_err() {
                    return;
                }
            }
        });
        Self { incoming, recent: RefCell::default(), wants: Some(wants), asked: Cell::new(false) }
    }

    /// The next thing heard, failing once nothing has been for `QUIET`.
    #[track_caller]
    pub fn next(&self) -> T {
        match self.within(QUIET) {
            Ok(heard) => heard,
            Err(RecvTimeoutError::Timeout) => panic!("nothing was heard for {QUIET:?}; {}", self.last()),
            Err(RecvTimeoutError::Disconnected) => panic!("the connection ended; {}", self.last()),
        }
    }

    /// The next thing heard, or `None` once nothing has been for `QUIET`: for a case that reads until it
    /// goes quiet. Failing when the connection ends is the same as `next`.
    #[track_caller]
    pub fn until_quiet(&self) -> Option<T> {
        match self.within(QUIET) {
            Ok(heard) => Some(heard),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => panic!("the connection ended; {}", self.last()),
        }
    }

    /// Everything heard so far that the case has not taken, without waiting for more.
    pub fn waiting(&self) -> Vec<T> {
        std::iter::from_fn(|| self.within(Duration::ZERO).ok()).collect()
    }

    /// Everything heard until the far end closes, **failing if it goes quiet for `QUIET` without closing**:
    /// for a case whose claim is that the far end lets the connection go.
    #[track_caller]
    pub fn until_closed(&self) -> Vec<T> {
        let mut heard = Vec::new();
        loop {
            match self.within(QUIET) {
                Ok(more) => heard.push(more),
                Err(RecvTimeoutError::Disconnected) => return heard,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("the far end kept the connection open and said nothing for {QUIET:?}; {}", self.last())
                }
            }
        }
    }

    /// The next thing heard within `step`, for a case whose claim is about a silence or an ending.
    pub fn within(&self, step: Duration) -> Result<T, RecvTimeoutError> {
        if let Some(wants) = &self.wants
            && !self.asked.get()
        {
            // A reader that has gone answers `Disconnected` below, which is what the case is told
            let _ = wants.send(());
            self.asked.set(true);
        }
        let heard = self.incoming.recv_timeout(step)?;
        self.asked.set(false);
        let mut recent = self.recent.borrow_mut();
        if recent.len() == REMEMBERED {
            recent.pop_front();
        }
        recent.push_back(heard.clone());
        Ok(heard)
    }

    /// What was heard last, as a failure shows it
    fn last(&self) -> String {
        let recent = self.recent.borrow();
        if recent.is_empty() {
            return "nothing had been heard at all".to_owned();
        }
        let shown: Vec<String> = recent
            .iter()
            .map(|heard| {
                let whole = format!("{heard:?}");
                match whole.char_indices().nth(SHOWN) {
                    Some((cut, _)) => format!("{}… ({} bytes)", &whole[..cut], whole.len()),
                    None => whole,
                }
            })
            .collect();
        format!("the last heard, oldest first: {shown:#?}")
    }
}

/// `source`'s lines, without their line ending.
pub fn lines<R: Read + Send + 'static>(source: R) -> Heard<String> {
    Heard::reading(source, |reader| {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                let ended = line.trim_end_matches(['\r', '\n']).len();
                line.truncate(ended);
                Some(line)
            }
        }
    })
}

/// `source`'s lines read as `T`, passing over any line that is not one.
pub fn events<T, R>(source: R) -> Heard<T>
where
    T: DeserializeOwned + Clone + Debug + Send + 'static,
    R: Read + Send + 'static,
{
    Heard::reading(source, event)
}

/// The same, read at the case's pace (`Heard::paced`).
pub fn paced_events<T, R>(source: R) -> Heard<T>
where
    T: DeserializeOwned + Clone + Debug + Send + 'static,
    R: Read + Send + 'static,
{
    Heard::paced(source, event)
}

/// The next line of `reader` that is a `T`
fn event<T: DeserializeOwned, R: Read>(reader: &mut BufReader<R>) -> Option<T> {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {
                if let Ok(event) = serde_json::from_str::<T>(&line) {
                    return Some(event);
                }
            }
        }
    }
}

/// A view connection's messages: past the one JSON line that answers its handshake, the binary wire.
pub fn view_messages<R: Read + Send + 'static>(source: R) -> Heard<ToView> {
    Heard::reading(source, view_message())
}

/// The same, read at the case's pace (`Heard::paced`): for a view whose subject is what the broker does
/// while the view is not reading.
pub fn paced_view_messages<R: Read + Send + 'static>(source: R) -> Heard<ToView> {
    Heard::paced(source, view_message())
}

/// What reads a view's next message, the handshake's answer passed over first
fn view_message<R: Read>() -> impl FnMut(&mut BufReader<R>) -> Option<ToView> {
    let mut greeted = false;
    move |reader| {
        if !greeted {
            let mut hello = String::new();
            reader.read_line(&mut hello).ok()?;
            greeted = true;
        }
        match read_message(reader) {
            Incoming::Message(body) => ToView::decode(&body).ok(),
            _ => None,
        }
    }
}
