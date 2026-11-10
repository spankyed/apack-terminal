//! A session's recording: what its program wrote, as a file a later session or a rewind reads.
//!
//! **A log of what arrived, with the screen written out whole every so often.** Each entry is one thing
//! that happened to the terminal — bytes from the pty, a resize — stamped with when. Replaying them all
//! from an empty terminal reproduces the session exactly, and costs everything it ever printed; so every
//! `KEYFRAME_EVERY` bytes the terminal is written out as the sequences that redraw it (a *keyframe*), and a
//! replay starts from the last one before the moment it wants. That bounds a replay's work by the distance
//! between keyframes rather than by the session's age.
//!
//! **Bounded on disk, by dropping its oldest part at a keyframe.** A recording past its budget is rewritten
//! from the first keyframe in its newer half, so what remains still starts with a whole screen. How far
//! back a rewind reaches is therefore the budget's, not the session's.
//!
//! **What is recorded is everything the program printed**, including what it printed and then erased. It
//! is opt-in per session for that reason, and the file is created readable by its owner alone.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many bytes a program writes between two keyframes: what a replay feeds at most, after the keyframe.
pub const KEYFRAME_EVERY: u64 = 1 << 20;

/// How large a recording grows before its oldest part is dropped.
pub const BUDGET: u64 = 32 << 20;

const KEYFRAME: u8 = 1;
const OUTPUT: u8 = 2;
const RESIZE: u8 = 3;

/// Kind, time and payload length.
const HEADER: usize = 1 + 8 + 4;

/// One thing that happened to a recorded terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// The whole terminal at this moment, as the sequences that redraw it in one of this size.
    Keyframe {
        cols: u16,
        rows: u16,
        redraw: Vec<u8>,
    },
    /// Bytes the program wrote.
    Output(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
}

/// An entry and when it happened, in milliseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timed {
    pub at: u64,
    pub entry: Entry,
}

/// Now, as a recording stamps it.
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64)
}

fn encode(at: u64, entry: &Entry) -> Vec<u8> {
    let (kind, payload): (u8, Vec<u8>) = match entry {
        Entry::Keyframe { cols, rows, redraw } => {
            let mut payload = Vec::with_capacity(4 + redraw.len());
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&rows.to_le_bytes());
            payload.extend_from_slice(redraw);
            (KEYFRAME, payload)
        }
        Entry::Output(bytes) => (OUTPUT, bytes.clone()),
        Entry::Resize { cols, rows } => {
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&cols.to_le_bytes());
            payload.extend_from_slice(&rows.to_le_bytes());
            (RESIZE, payload)
        }
    };
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.push(kind);
    bytes.extend_from_slice(&at.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes
}

fn size(payload: &[u8]) -> Option<(u16, u16)> {
    let cols = u16::from_le_bytes(payload.get(0..2)?.try_into().ok()?);
    let rows = u16::from_le_bytes(payload.get(2..4)?.try_into().ok()?);
    Some((cols, rows))
}

/// Every whole entry in `bytes`, in order.
///
/// **A recording is read while it is written, and after a process that was writing it died**, so its last
/// entry may be part of one. Reading stops at the first entry that is not whole or not understood: what
/// came before it is what was recorded.
pub fn decode(bytes: &[u8]) -> Vec<Timed> {
    let mut entries = Vec::new();
    let mut rest = bytes;
    while rest.len() >= HEADER {
        let kind = rest[0];
        let at = u64::from_le_bytes(rest[1..9].try_into().unwrap_or_default());
        let length = u32::from_le_bytes(rest[9..13].try_into().unwrap_or_default()) as usize;
        let Some(payload) = rest.get(HEADER..HEADER + length) else { break };
        let entry = match kind {
            KEYFRAME => match size(payload) {
                Some((cols, rows)) => Entry::Keyframe { cols, rows, redraw: payload[4..].to_vec() },
                None => break,
            },
            OUTPUT => Entry::Output(payload.to_vec()),
            RESIZE => match size(payload) {
                Some((cols, rows)) => Entry::Resize { cols, rows },
                None => break,
            },
            _ => break,
        };
        entries.push(Timed { at, entry });
        rest = &rest[HEADER + length..];
    }
    entries
}

/// A recording's entries, or none for one that is not there.
pub fn read(path: &Path) -> Vec<Timed> {
    let mut bytes = Vec::new();
    match File::open(path).and_then(|mut file| file.read_to_end(&mut bytes)) {
        Ok(_) => decode(&bytes),
        Err(_) => Vec::new(),
    }
}

/// Where a replay to `until` starts and ends: the last keyframe at or before it, and one past the last
/// entry at or before it. `None` when the recording holds no keyframe that early.
pub fn span(entries: &[Timed], until: u64) -> Option<(usize, usize)> {
    let end = entries.partition_point(|timed| timed.at <= until);
    let start = entries[..end].iter().rposition(|timed| matches!(timed.entry, Entry::Keyframe { .. }))?;
    Some((start, end))
}

/// Writes a session's recording.
pub struct Recorder {
    path: PathBuf,
    file: File,
    /// The file's length.
    written: u64,
    /// Bytes of output since the last keyframe.
    since_keyframe: u64,
    /// Where each keyframe in the file starts.
    keyframes: Vec<u64>,
    budget: u64,
    every: u64,
}

fn create(path: &Path) -> std::io::Result<File> {
    if let Some(directory) = path.parent().filter(|directory| !directory.as_os_str().is_empty()) {
        std::fs::create_dir_all(directory)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // What a program printed can be a secret it was shown once
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Recorder {
    /// Starts a recording at `path`, replacing whatever was there.
    pub fn start(path: &Path) -> std::io::Result<Self> {
        Self::start_with(path, BUDGET, KEYFRAME_EVERY)
    }

    /// The same with a budget and a keyframe distance of the caller's, for a case about either that would
    /// otherwise take megabytes of output to reach.
    pub fn start_with(path: &Path, budget: u64, every: u64) -> std::io::Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            file: create(path)?,
            written: 0,
            since_keyframe: 0,
            keyframes: Vec::new(),
            budget,
            every,
        })
    }

    /// Carries on the recording at `path`, answering what it already holds. A recording that is not there,
    /// or holds no whole entry, is started.
    ///
    /// **An entry its last writer did not finish is cut off first**, so what is appended follows a whole
    /// entry and a reader does not stop at the tear with everything after it unread.
    pub fn resume(path: &Path) -> std::io::Result<(Self, Vec<Timed>)> {
        let mut bytes = Vec::new();
        if let Ok(mut file) = File::open(path) {
            file.read_to_end(&mut bytes)?;
        }
        let entries = decode(&bytes);
        if entries.is_empty() {
            return Ok((Self::start(path)?, entries));
        }
        let mut keyframes = Vec::new();
        let mut since_keyframe = 0;
        let mut written = 0u64;
        for timed in &entries {
            let length = encode(timed.at, &timed.entry).len() as u64;
            match &timed.entry {
                Entry::Keyframe { .. } => {
                    keyframes.push(written);
                    since_keyframe = 0;
                }
                Entry::Output(output) => since_keyframe += output.len() as u64,
                Entry::Resize { .. } => {}
            }
            written += length;
        }
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_len(written)?;
        let mut recorder = Self {
            path: path.to_owned(),
            file,
            written,
            since_keyframe,
            keyframes,
            budget: BUDGET,
            every: KEYFRAME_EVERY,
        };
        recorder.file.seek(SeekFrom::End(0))?;
        Ok((recorder, entries))
    }

    fn append(&mut self, entry: &Entry) -> std::io::Result<()> {
        let bytes = encode(now(), entry);
        self.file.write_all(&bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Records bytes the program wrote, answering whether a keyframe is due.
    pub fn output(&mut self, bytes: &[u8]) -> std::io::Result<bool> {
        self.append(&Entry::Output(bytes.to_vec()))?;
        self.since_keyframe += bytes.len() as u64;
        Ok(self.keyframe_due())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.append(&Entry::Resize { cols, rows })
    }

    /// Whether enough has been written since the last keyframe for another, or none has been written yet.
    pub fn keyframe_due(&self) -> bool {
        self.keyframes.is_empty() || self.since_keyframe >= self.every
    }

    /// Whether a keyframe can no longer be put off: the recording is past its budget and only a keyframe
    /// lets its oldest part go.
    pub fn keyframe_overdue(&self) -> bool {
        self.written > self.budget && self.since_keyframe > 0
    }

    /// Records the whole terminal, and drops the recording's oldest part if it is past its budget.
    pub fn keyframe(&mut self, cols: u16, rows: u16, redraw: Vec<u8>) -> std::io::Result<()> {
        let at = self.written;
        self.append(&Entry::Keyframe { cols, rows, redraw })?;
        self.keyframes.push(at);
        self.since_keyframe = 0;
        if self.written > self.budget {
            self.drop_oldest()?;
        }
        Ok(())
    }

    /// Rewrites the file from the first keyframe in its newer half — or from its last, when that is the
    /// only one there.
    fn drop_oldest(&mut self) -> std::io::Result<()> {
        let half = self.written.saturating_sub(self.budget / 2);
        let from = self
            .keyframes
            .iter()
            .copied()
            .find(|offset| *offset >= half)
            .or(self.keyframes.last().copied())
            .unwrap_or(0);
        if from == 0 {
            return Ok(());
        }
        let mut kept = Vec::new();
        let mut reading = File::open(&self.path)?;
        reading.seek(SeekFrom::Start(from))?;
        reading.read_to_end(&mut kept)?;
        // Written beside it and renamed over it, so a reader never finds a recording that starts mid-entry
        let beside = self.path.with_extension("rewriting");
        let mut file = create(&beside)?;
        file.write_all(&kept)?;
        std::fs::rename(&beside, &self.path)?;
        self.file = file;
        self.written = kept.len() as u64;
        self.keyframes = self.keyframes.iter().filter(|offset| **offset >= from).map(|offset| offset - from).collect();
        Ok(())
    }
}
