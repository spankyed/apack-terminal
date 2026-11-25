//! A session's recording: what its program wrote, as a file a later session or a rewind reads.
//!
//! **A log of what arrived, with the screen written out whole every so often.** Each entry is one thing
//! that happened to the terminal — bytes from the pty, a resize, a change of what vouches for the shell's
//! marks — stamped with when. Replaying them all
//! from an empty terminal reproduces the session exactly, and costs everything it ever printed; so every
//! `KEYFRAME_EVERY` bytes the terminal is written out whole (a *keyframe*), and a replay starts from the
//! last one before the moment it wants. That bounds a replay's work by the distance between keyframes
//! rather than by the session's age.
//!
//! **A keyframe is the terminal's core's own account of it**, held here and not read: all of the
//! terminal's state, so a replay from it goes on exactly as the session did (`vt::keyframe`). It is read
//! by the core that wrote it. The core's format moves with the core, so a recording outlives a change of
//! core as a log and not as something to pick up: one whose keyframes this build's core does not read is
//! begun again from the first it does, or not at all.
//!
//! **Bounded on disk, by dropping its oldest part at a keyframe.** A recording past its budget is rewritten
//! from the first keyframe in its newer half, so what remains still starts with a whole screen. How far
//! back a rewind reaches is therefore the budget's, not the session's.
//!
//! **What is recorded is everything the program printed**, including what it printed and then erased. It
//! is opt-in per session for that reason, and the file is readable by its owner alone.
//!
//! **And the key its shell's marks were vouched with** (`Entry::Marks`), for a session that had one. The
//! stream holds the marks as they were printed, and a replay rebuilds the session's commands from them:
//! read with the key and the numbering they were written under, what was refused then is refused again
//! and what was recorded is recorded as it was. The key is in every mark the stream holds already, so
//! the file keeps no secret the stream did not.
//!
//! **The file is written by a thread of its own** (`Recorder`), so a disk that is slow holds up no
//! session: what the terminal's reader does is hand an entry over, and it waits for that thread at no
//! point — not to stop recording, and not when the session ends. What that buys and costs in what
//! survives is this. An entry is in the operating system's hands moments after it happened, and what the
//! operating system has been handed survives this process dying, however it dies; an entry still waiting
//! for the thread does not. **Nothing is synced to the disk, the rename that drops a recording's oldest
//! part included**, so the machine losing power can lose the recording's last moments, leave its last
//! entry part written — which a reader stops at, and the next session to pick the recording up cuts
//! off — or, around that rename, lose the recording.
//!
//! **One recorder a file, in this process** (`claim`). A recording is a log its writer knows the layout
//! of, so a second recorder on the same file is refused while the first is recording; and one that
//! follows a recorder that has stopped waits for that recorder's thread to have written everything it
//! was handed, so what it picks up is whole.
//!
//! **Times never go back.** An entry is stamped with the clock or with the stamp before it, whichever is
//! later, so a clock set back while a session runs does not leave a recording out of order: finding the
//! entries up to a moment is a search, and a search needs them sorted.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::markers::MarksKey;

/// How many bytes a program writes between two keyframes: what a replay feeds at most, after the keyframe —
/// or as many as that keyframe held, where it is larger (`Recorder::keyframe_due`).
pub const KEYFRAME_EVERY: u64 = 1 << 20;

/// How large a recording grows before its oldest part is dropped.
pub const BUDGET: u64 = 32 << 20;

/// The most bytes a recording reader takes into memory from disk at once: four times `BUDGET`,
/// covering a recording that ran past twice its budget before stopping while refusing a hostile or
/// corrupted multi-gigabyte file.
pub const MAX_RECORDING_BYTES: u64 = BUDGET * 4;

/// How much of a recording may be waiting to be written before it is given up on.
///
/// **A disk that cannot keep up costs the recording, never the session.** What waits is held in memory,
/// and a program can print faster than a stalled disk takes it for as long as it likes; past this the
/// recording stops and says so (`Recorder::failed`), and the session carries on unrecorded.
pub const BACKLOG: usize = 16 << 20;

const KEYFRAME: u8 = 1;
const OUTPUT: u8 = 2;
const RESIZE: u8 = 3;
const MARKS: u8 = 4;

/// Kind, time and payload length.
const HEADER: usize = 1 + 8 + 4;

/// Maximum payload length of a `MARKS` entry: 8-byte counter plus `MarksKey::LENGTH`.
const MARKS_MAX_PAYLOAD: usize = 8 + MarksKey::LENGTH;

/// The format a recording file is written in. A file records its format in its first bytes, so a reader can
/// refuse a file it does not understand rather than decode it wrongly.
pub const FORMAT: u16 = 1;

/// The file's first bytes: the magic string, then the format as two little-endian bytes. No entry begins
/// with `A` (its kind is one of `KEYFRAME` to `MARKS`), so a file that begins with this is not mistaken for
/// a run of entries, and a file that does not is refused rather than read.
const FILE_HEADER: [u8; 8] = {
    let format = FORMAT.to_le_bytes();
    [b'A', b'P', b'K', b'R', b'E', b'C', format[0], format[1]]
};

/// Why a recording's first bytes are not a recording this build reads, or `None` when they are, or when
/// there are none yet. Given the start of a file, which is all that is needed to answer.
pub fn refusal(head: &[u8]) -> Option<String> {
    if head.is_empty() || head.starts_with(&FILE_HEADER) {
        return None;
    }
    if head.starts_with(b"APKREC") {
        let version = head.get(6..8).map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
        return Some(format!("it is format {version}, and this broker reads format {FORMAT}"));
    }
    Some(format!("it does not begin with a recording's header, and only format {FORMAT} recordings are read"))
}

/// Whether the file at `path` is a recording this build reads: `Ok` for one that is, and for one that is
/// not there or is empty, which have nothing to refuse. The reason for a refusal is said for a person.
pub fn check(path: &Path) -> Result<(), String> {
    let Ok(mut file) = File::open(path) else { return Ok(()) };
    let mut head = [0u8; FILE_HEADER.len()];
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(more) => read += more,
            Err(error) => return Err(error.to_string()),
        }
    }
    refusal(&head[..read]).map_or(Ok(()), Err)
}

/// One thing that happened to a recorded terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// The whole terminal at this moment, of this size: its state as its core wrote it out, which this
    /// module holds and does not read (`vt::keyframe`).
    Keyframe {
        cols: u16,
        rows: u16,
        state: Vec<u8>,
    },
    /// Bytes the program wrote.
    Output(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// What vouches for the shell's marks in the output that follows: the key they must carry and the
    /// number of the last start accepted under it, or no key, where every marker was believed. Absent
    /// from a recording's beginning, no key vouches.
    Marks {
        key: Option<MarksKey>,
        last: u64,
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

/// An entry as the file holds it: its header, then its payload given as the pieces it is made of.
fn encode(kind: u8, at: u64, payload: &[&[u8]]) -> Vec<u8> {
    let length: usize = payload.iter().map(|piece| piece.len()).sum();
    let mut bytes = Vec::with_capacity(HEADER + length);
    bytes.push(kind);
    bytes.extend_from_slice(&at.to_le_bytes());
    bytes.extend_from_slice(&(length as u32).to_le_bytes());
    for piece in payload {
        bytes.extend_from_slice(piece);
    }
    bytes
}

/// What a marks entry's payload says: the last number, then the key or nothing. `None` for a payload
/// that is neither.
fn marks(payload: &[u8]) -> Option<Entry> {
    let last = u64::from_le_bytes(payload.get(0..8)?.try_into().ok()?);
    let key = match &payload[8..] {
        [] => None,
        key => Some(MarksKey::new(std::str::from_utf8(key).ok()?)?),
    };
    Some(Entry::Marks { key, last })
}

fn size(payload: &[u8]) -> Option<(u16, u16)> {
    let cols = u16::from_le_bytes(payload.get(0..2)?.try_into().ok()?);
    let rows = u16::from_le_bytes(payload.get(2..4)?.try_into().ok()?);
    Some((cols, rows))
}

/// How long an entry is in the file.
fn length(entry: &Entry) -> u64 {
    let payload = match entry {
        Entry::Keyframe { state, .. } => 4 + state.len(),
        Entry::Output(bytes) => bytes.len(),
        Entry::Resize { .. } => 4,
        Entry::Marks { key, .. } => 8 + key.as_ref().map_or(0, |key| key.as_str().len()),
    };
    (HEADER + payload) as u64
}

/// Decodes one entry from its kind byte and payload, or `None` when the kind or payload is invalid.
fn decode_entry(kind: u8, payload: &[u8]) -> Option<Entry> {
    match kind {
        KEYFRAME => size(payload).map(|(cols, rows)| Entry::Keyframe { cols, rows, state: payload[4..].to_vec() }),
        OUTPUT => Some(Entry::Output(payload.to_vec())),
        RESIZE => {
            (payload.len() == 4).then(|| size(payload)).flatten().map(|(cols, rows)| Entry::Resize { cols, rows })
        }
        MARKS => marks(payload),
        _ => None,
    }
}

/// Every whole entry in `bytes`, in order.
///
/// **A recording is read while it is written, and after a process that was writing it died**, so its last
/// entry may be part of one. Reading stops at the first entry that is not whole or not understood: what
/// came before it is what was recorded.
pub fn decode(bytes: &[u8]) -> Vec<Timed> {
    let mut entries = Vec::new();
    // A whole file begins with its header, which is not an entry; a stretch of one never does
    let mut rest = bytes.strip_prefix(&FILE_HEADER).unwrap_or(bytes);
    while rest.len() >= HEADER {
        let kind = rest[0];
        let at = u64::from_le_bytes(rest[1..9].try_into().unwrap_or_default());
        let length = u32::from_le_bytes(rest[9..13].try_into().unwrap_or_default()) as usize;
        let Some(end) = HEADER.checked_add(length) else { break };
        let Some(payload) = rest.get(HEADER..end) else { break };
        let Some(entry) = decode_entry(kind, payload) else { break };
        entries.push(Timed { at, entry });
        rest = &rest[end..];
    }
    entries
}

/// Reads a recording stream up to `limit` bytes, returning `InvalidData` if more bytes follow.
fn read_stream_bounded(reader: impl Read, limit: u64, path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is larger than a recording may be ({limit} bytes)", path.display()),
        ));
    }
    Ok(bytes)
}

/// Reads a recording file into memory, refusing any file larger than `limit`.
fn read_bounded_to(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is larger than a recording may be ({limit} bytes)", path.display()),
        ));
    }
    read_stream_bounded(file, limit, path)
}

/// Reads a recording file into memory, refusing any file larger than `MAX_RECORDING_BYTES`.
fn read_bounded(path: &Path) -> std::io::Result<Vec<u8>> {
    read_bounded_to(path, MAX_RECORDING_BYTES)
}

/// A recording's entries, or none for one that is not there or exceeds `MAX_RECORDING_BYTES`.
pub fn read(path: &Path) -> Vec<Timed> {
    match read_bounded(path) {
        Ok(bytes) if refusal(&bytes).is_none() => decode(&bytes),
        _ => Vec::new(),
    }
}

/// Whether the last stretch of a recording's stream was vouched with a key.
pub fn ends_vouched(entries: &[Timed]) -> bool {
    entries
        .iter()
        .rev()
        .find_map(|timed| match &timed.entry {
            Entry::Marks { key, .. } => Some(key.is_some()),
            _ => None,
        })
        .unwrap_or(false)
}

/// Where a replay to `until` starts and ends: the last keyframe at or before it, and one past the last
/// entry at or before it. `None` when the recording holds no keyframe that early.
pub fn span(entries: &[Timed], until: u64) -> Option<(usize, usize)> {
    // Treat timestamps monotonically, matching `Index::open` and `Index::moment`, so a recording with
    // out-of-order timestamps never misleads a binary search.
    let mut latest = 0u64;
    let mut end = 0;
    for timed in entries {
        latest = latest.max(timed.at);
        if latest > until {
            break;
        }
        end += 1;
    }
    let start = entries[..end].iter().rposition(|timed| matches!(timed.entry, Entry::Keyframe { .. }))?;
    Some((start, end))
}

/// A recording opened to be looked back through: where each of its keyframes is, and the file they are in.
///
/// **Where the keyframes are, rather than what the recording holds.** A rewind shows one moment at a
/// time, which is a keyframe and the little that follows it; the tens of megabytes between are read when
/// a moment among them is asked for and dropped again after.
///
/// **The file stays open**, so the offsets stay true: a session still recording here drops the oldest
/// part by writing a new file over this one's name, and an open file is the one that was opened.
pub struct Index {
    file: File,
    /// When each keyframe was taken and where its entry begins, oldest first
    keyframes: Vec<(u64, u64)>,
    /// Where the last whole entry ends
    end: u64,
    /// When the last entry happened
    last: u64,
}

impl Index {
    /// Reads where a recording's keyframes are. `None` for a recording that is not there, exceeds
    /// `MAX_RECORDING_BYTES`, or holds no keyframe: there is nothing in it to look back through.
    pub fn open(path: &Path) -> Option<Self> {
        Self::open_bounded(path, MAX_RECORDING_BYTES)
    }

    fn open_bounded(path: &Path, limit: u64) -> Option<Self> {
        let file = File::open(path).ok()?;
        let total = file.metadata().ok()?.len();
        if total > limit {
            return None;
        }
        let mut reader = BufReader::new(file);
        let mut prefix = [0u8; FILE_HEADER.len()];
        if reader.read_exact(&mut prefix).is_err() || prefix != FILE_HEADER {
            return None;
        }
        let mut keyframes = Vec::new();
        let (mut at, mut offset) = (0u64, FILE_HEADER.len() as u64);
        let mut header = [0u8; HEADER];
        // An entry's header, and then past its payload without reading it. A header that is not all
        // there, or a payload the file ends inside, is a recording's torn end
        while reader.read_exact(&mut header).is_ok() {
            let length = u64::from(u32::from_le_bytes(header[9..13].try_into().unwrap_or_default()));
            let whole = match header[0] {
                KEYFRAME => length >= 4,
                RESIZE => length == 4,
                MARKS => length == 8 || length == MARKS_MAX_PAYLOAD as u64,
                OUTPUT => true,
                _ => false,
            };
            let Some(next) = offset.checked_add(HEADER as u64).and_then(|at| at.checked_add(length)) else {
                break;
            };
            if !whole || next > total {
                break;
            }
            if header[0] == MARKS {
                let mut payload = [0u8; MARKS_MAX_PAYLOAD];
                let payload = &mut payload[..length as usize];
                if reader.read_exact(payload).is_err() || marks(payload).is_none() {
                    break;
                }
            } else if reader.seek_relative(i64::try_from(length).ok()?).is_err() {
                break;
            }
            // Never earlier than the entry before it, whatever the clock that stamped it said
            at = at.max(u64::from_le_bytes(header[1..9].try_into().unwrap_or_default()));
            if header[0] == KEYFRAME {
                keyframes.push((at, offset));
            }
            offset = next;
        }
        (!keyframes.is_empty()).then(|| Self { file: reader.into_inner(), keyframes, end: offset, last: at })
    }

    /// When the earliest moment that can be shown is: the first keyframe.
    pub fn first(&self) -> u64 {
        self.keyframes.first().map_or(0, |(at, _)| *at)
    }

    /// When the recording's last entry happened.
    pub fn last(&self) -> u64 {
        self.last
    }

    /// The entries that rebuild the terminal as it was at `until`: the last keyframe at or before then,
    /// and what followed it up to then. Empty when there is no keyframe that early.
    pub fn moment(&mut self, until: u64) -> std::io::Result<Vec<Timed>> {
        self.moment_bounded(until, MAX_RECORDING_BYTES)
    }

    fn moment_bounded(&mut self, until: u64, limit: u64) -> std::io::Result<Vec<Timed>> {
        let after = self.keyframes.partition_point(|(at, _)| *at <= until);
        let Some(&(first_at, from)) = after.checked_sub(1).and_then(|at| self.keyframes.get(at)) else {
            return Ok(Vec::new());
        };
        // Up to the keyframe after it, which is past `until` or it would have been the one chosen
        let to = self.keyframes.get(after).map_or(self.end, |(_, offset)| *offset);
        self.file.seek(SeekFrom::Start(from))?;
        let mut reader = BufReader::new(&mut self.file);
        let mut entries = Vec::new();
        let (mut pos, mut latest, mut read_bytes) = (from, first_at, 0u64);
        let mut header = [0u8; HEADER];
        while pos.saturating_add(HEADER as u64) <= to {
            if reader.read_exact(&mut header).is_err() {
                break;
            }
            let kind = header[0];
            let at = u64::from_le_bytes(header[1..9].try_into().unwrap_or_default());
            let length = u64::from(u32::from_le_bytes(header[9..13].try_into().unwrap_or_default()));
            latest = latest.max(at);
            if latest > until {
                break;
            }
            let Some(next) = pos.checked_add(HEADER as u64).and_then(|at| at.checked_add(length)) else {
                break;
            };
            if next > to {
                break;
            }
            read_bytes = read_bytes.saturating_add(HEADER as u64).saturating_add(length);
            if read_bytes > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("recording segment exceeds maximum readable size ({limit} bytes)"),
                ));
            }
            let mut payload = vec![0u8; usize::try_from(length).map_err(std::io::Error::other)?];
            reader.read_exact(&mut payload)?;
            let Some(entry) = decode_entry(kind, &payload) else { break };
            entries.push(Timed { at, entry });
            pos = next;
        }
        Ok(entries)
    }
}

/// What the thread writing a recording's file is asked to do, in order.
enum Job {
    /// These bytes, at the end of the file
    Append(Vec<u8>),
    /// Everything before this offset dropped, and what was after it now the whole file
    DropBefore(u64),
    /// Say when everything asked before this has been done
    Reached(Sender<()>),
}

/// What a recorder and the thread writing for it both see.
struct Shared {
    /// Bytes handed over and not yet written
    waiting: AtomicUsize,
    /// The file could not be written, or the disk fell too far behind: nothing more is recorded
    failed: AtomicBool,
}

/// Writes a session's recording.
///
/// **Two halves.** This one is the terminal's reader's: it decides what the file holds — which entry,
/// stamped when, where each keyframe will be, when the oldest part goes — and never touches the disk.
/// The other is a thread that does what it is told in the order it is told (`Job`), so the file is what
/// this half worked out, a little later.
pub struct Recorder {
    jobs: Option<Sender<Job>>,
    /// This recorder's place among the recorders of this process, which says it has stopped when it has
    writing: Arc<Writing>,
    shared: Arc<Shared>,
    /// The file's length, once everything handed over is written.
    written: u64,
    /// Bytes of output since the last keyframe.
    since_keyframe: u64,
    /// How many bytes the last keyframe held: as much output again comes before the next
    /// (`keyframe_due`).
    last_keyframe: u64,
    /// Where each keyframe in the file starts.
    keyframes: Vec<u64>,
    budget: u64,
    every: u64,
    /// The latest time an entry was stamped with.
    stamped: u64,
}

/// Opens a fresh private recording file at `path` directly and writes `FILE_HEADER`, cleaning up
/// `path` if setting permissions or writing the header fails.
fn open_new(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(error) = private(&file).and_then(|()| file.write_all(&FILE_HEADER)) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

/// Renames a freshly written sibling `beside` over `path` and returns the handle positioned at its
/// end, removing `beside` if the rename fails.
///
/// On Unix, the open handle survives the rename and any reader with the old inode open continues
/// reading it undisturbed. On Windows, the open handle must be closed before `rename` and reopened
/// at the end of `path`.
fn replace_with(beside: &Path, path: &Path, file: File) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        if let Err(error) = std::fs::rename(beside, path) {
            drop(file);
            let _ = std::fs::remove_file(beside);
            return Err(error);
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        drop(file);
        if let Err(error) = std::fs::rename(beside, path) {
            let _ = std::fs::remove_file(beside);
            return Err(error);
        }
        let mut reopened = std::fs::OpenOptions::new().write(true).open(path)?;
        reopened.seek(SeekFrom::End(0))?;
        Ok(reopened)
    }
}

/// A file for a recording at `path`: new, or the one that is there, with nothing in it either way.
///
/// **A new file under the name, never the old one emptied.** Something may be reading the file that is
/// there — a recording being looked back through keeps it open — and emptying it would take what it is
/// reading from under it. Written beside and renamed over, the old file is whoever has it open's until
/// they close it.
fn create(path: &Path) -> std::io::Result<File> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let beside = beside(path);
    let file = open_new(&beside)?;
    replace_with(&beside, path, file)
}

/// A recording's path as the one name its file has: its directory with every `..` and link in it
/// resolved, and the file's own name. Two spellings of one file are then one name, which is what "one
/// recorder a file" is decided by.
///
/// **Nothing is made by asking.** A directory that is not there is not resolved through, and the path
/// is answered as it was spelled, made absolute: a recording that does not exist has no recorder to be
/// confused with, and looking for one must not leave directories behind. Whatever is about to *write*
/// a recording makes its directory first (`in_its_directory`).
pub fn canonical(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let (Some(directory), Some(name)) = (absolute.parent(), absolute.file_name()) else { return absolute };
    directory.canonicalize().map_or(absolute.clone(), |directory| directory.join(name))
}

/// The one name of a recording about to be written, with the directory it goes in made.
fn in_its_directory(path: &Path) -> PathBuf {
    if let Some(directory) = std::path::absolute(path).ok().as_deref().and_then(Path::parent) {
        let _ = std::fs::create_dir_all(directory);
    }
    canonical(path)
}

/// How long anything waits for a recorder that has stopped to finish writing its file.
///
/// **A bound, because what is waited for is a disk.** A recorder's thread is writing the last of what it
/// was handed, which is moments of work on a disk that is working and has no end on one that is not; and
/// what waits on it is a request somebody is waiting on in turn. Past this the wait is given up and
/// whoever was waiting is told: a session starts unrecorded, a recording is not looked back through, a
/// broker that is leaving leaves. It is the clock's because nothing else says a disk has stopped.
pub const RECORDER_PATIENCE: Duration = Duration::from_secs(5);

/// How far along the recorder of one file is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// It may still be handed entries
    Recording,
    /// It has stopped, and its thread is writing what it was handed
    Finishing,
    /// Everything it was handed is written, or never will be
    Done,
}

/// One recorder's place among this process's recorders.
struct Writing {
    stage: Mutex<Stage>,
    moved: Condvar,
}

impl Writing {
    fn set(&self, stage: Stage) {
        if let Ok(mut held) = self.stage.lock() {
            *held = stage;
        }
        self.moved.notify_all();
    }

    fn stage(&self) -> Stage {
        self.stage.lock().map_or(Stage::Done, |held| *held)
    }

    /// Waits until the recorder's thread has finished with its file, or `until`. Answers whether it has.
    fn finished_by(&self, until: Instant) -> bool {
        let Ok(mut held) = self.stage.lock() else { return true };
        while *held != Stage::Done {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let Ok((woken, _)) = self.moved.wait_timeout(held, left) else { return true };
            held = woken;
        }
        true
    }
}

/// The recorder of each file being recorded to in this process, by the file's one name (`canonical`).
fn writers() -> &'static Mutex<HashMap<PathBuf, Arc<Writing>>> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, Arc<Writing>>>> = OnceLock::new();
    WRITERS.get_or_init(Mutex::default)
}

/// A recorder's hold on its file, from before the file is opened until its thread has finished with it.
struct Claim {
    path: PathBuf,
    writing: Arc<Writing>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut writers) = writers().lock()
            && writers.get(&self.path).is_some_and(|held| Arc::ptr_eq(held, &self.writing))
        {
            writers.remove(&self.path);
        }
        self.writing.set(Stage::Done);
    }
}

/// Takes the right to record to the file `path` names, which must be its one name.
///
/// Refused while another recorder is recording there. One that has stopped is waited for, for as long
/// as `RECORDER_PATIENCE`: its thread is writing the last of what it was handed, and what follows it
/// must find all of that in the file — or, when the disk is not taking it, be refused the file.
fn claim(path: &Path) -> std::io::Result<Claim> {
    let until = Instant::now() + RECORDER_PATIENCE;
    loop {
        let previous = {
            let mut writers =
                writers().lock().map_err(|_| std::io::Error::other("the recorders' lock was poisoned"))?;
            match writers.get(path).map(|held| (Arc::clone(held), held.stage())) {
                Some((_, Stage::Recording)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!("{} is being recorded to by another session", path.display()),
                    ));
                }
                Some((previous, _)) => previous,
                None => {
                    let writing = Arc::new(Writing { stage: Mutex::new(Stage::Recording), moved: Condvar::new() });
                    writers.insert(path.to_owned(), Arc::clone(&writing));
                    return Ok(Claim { path: path.to_owned(), writing });
                }
            }
        };
        // With the lock let go: this is the wait for a disk
        if !previous.finished_by(until) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} is still being written by the session that recorded there last", path.display()),
            ));
        }
    }
}

/// Waits until whatever recorder of `path` has stopped has written everything it was handed, for as
/// long as `RECORDER_PATIENCE`. Answers whether the file is whole: `false` is a recorder still writing
/// when the wait was given up. For whoever is about to read the file of a recording that was just
/// stopped, or to start a program that will be recorded there.
pub fn settled(path: &Path) -> bool {
    let path = canonical(path);
    // Looked up and read in one hold of the lock, so the recorder found is known to have stopped
    let stopped = writers()
        .lock()
        .ok()
        .and_then(|writers| writers.get(&path).filter(|writing| writing.stage() != Stage::Recording).cloned());
    stopped.is_none_or(|stopped| stopped.finished_by(Instant::now() + RECORDER_PATIENCE))
}

/// Waits until every recorder in this process that has stopped has written everything it was handed,
/// for as long as `RECORDER_PATIENCE` in all. **For a process about to exit**: a recorder's thread ends
/// with the process, and what it had not written by then is the end of a recording nobody will see.
pub fn settle_all() {
    let until = Instant::now() + RECORDER_PATIENCE;
    let stopped: Vec<Arc<Writing>> = writers().lock().map_or_else(
        |_| Vec::new(),
        |writers| writers.values().filter(|writing| writing.stage() != Stage::Recording).cloned().collect(),
    );
    for stopped in stopped {
        if !stopped.finished_by(until) {
            return;
        }
    }
}

/// Makes a recording's file readable by its owner alone: what a program printed can be a secret it was
/// shown once.
///
/// **Said of the file rather than only asked for when it is made**, because the mode a file is opened
/// with is used only if the open creates it, and a recording is often picked up from a file already there.
fn private(file: &File) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

/// The name a recording's replacement is written under before it takes the recording's own.
///
/// **The whole name with a suffix**, not the name with its extension changed: two recordings told apart
/// only by extension — `a.1` and `a.2` — would otherwise both be rewritten through one file.
fn beside(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".rewriting");
    PathBuf::from(name)
}

/// The thread's half: does each job in turn, and stops at the first the disk refuses.
fn write_file(path: &Path, file: File, jobs: &Receiver<Job>, shared: &Shared) {
    let mut file = Some(file);
    for job in jobs {
        // Given up on by the half that hands jobs over (`BACKLOG`): what is still queued is not written
        if shared.failed.load(Ordering::Acquire) {
            return;
        }
        let done = match job {
            Job::Append(bytes) => {
                let done = match &mut file {
                    Some(file) => file.write_all(&bytes),
                    None => Err(std::io::Error::other("the recording file is closed")),
                };
                shared.waiting.fetch_sub(bytes.len(), Ordering::AcqRel);
                done
            }
            Job::DropBefore(from) => {
                // Close our own handle before renaming over `path` so Windows does not reject the
                // rename with a sharing violation.
                drop(file.take());
                drop_before(path, from).map(|kept| file = Some(kept))
            }
            Job::Reached(reached) => {
                let _ = reached.send(());
                Ok(())
            }
        };
        if done.is_err() {
            // Nothing after a write that failed can be put in the right place. Whoever is recording
            // finds out at its next entry; whatever is still queued is dropped with the channel
            shared.failed.store(true, Ordering::Release);
            return;
        }
    }
}

/// Rewrites the recording at `path` as what follows `from`, answering the new file open at its end.
///
/// Written beside it and renamed over it, so a reader never finds a recording that starts mid-entry.
fn drop_before(path: &Path, from: u64) -> std::io::Result<File> {
    let beside = beside(path);
    let copied = (|| -> std::io::Result<File> {
        let mut reading = File::open(path)?;
        reading.seek(SeekFrom::Start(from))?;
        let mut file = open_new(&beside)?;
        std::io::copy(&mut reading, &mut file)?;
        drop(reading);
        Ok(file)
    })();
    let file = match copied {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_file(&beside);
            return Err(error);
        }
    };
    replace_with(&beside, path, file)
}

impl Recorder {
    /// Starts a recording at `path`, replacing whatever was there.
    pub fn start(path: &Path) -> std::io::Result<Self> {
        Self::start_with(path, BUDGET, KEYFRAME_EVERY)
    }

    /// The same with a budget and a keyframe distance of the caller's, for a case about either that would
    /// otherwise take megabytes of output to reach.
    pub fn start_with(path: &Path, budget: u64, every: u64) -> std::io::Result<Self> {
        let path = in_its_directory(path);
        let claim = claim(&path)?;
        Self::over(claim, create(&path)?, budget, every)
    }

    /// A recorder writing to `file`, which is empty or is positioned at its end, under its claim on the
    /// file's name. The claim goes to the thread, which lets it go when it has written all it was handed.
    fn over(claim: Claim, file: File, budget: u64, every: u64) -> std::io::Result<Self> {
        let (jobs, asked) = channel();
        let shared = Arc::new(Shared { waiting: AtomicUsize::new(0), failed: AtomicBool::new(false) });
        let writing = Arc::clone(&claim.writing);
        std::thread::Builder::new().name("recording".into()).spawn({
            let shared = Arc::clone(&shared);
            move || write_file(&claim.path, file, &asked, &shared)
        })?;
        Ok(Self {
            jobs: Some(jobs),
            writing,
            shared,
            written: FILE_HEADER.len() as u64,
            since_keyframe: 0,
            last_keyframe: 0,
            keyframes: Vec::new(),
            budget,
            every,
            stamped: 0,
        })
    }

    /// Carries on the recording at `path`, answering what it already holds. A recording that is not
    /// there, or is an empty file, is started.
    ///
    /// **A file that is there and is not a recording is refused, and left exactly as it is.** Starting a
    /// recording empties the file it is given, and "could not be read" covers a recording on a disk that
    /// is failing as well as a path that was never one: neither is this function's to throw away.
    ///
    /// **An entry its last writer did not finish is cut off first**, so what is appended follows a whole
    /// entry and a reader does not stop at the tear with everything after it unread.
    pub fn resume(path: &Path) -> std::io::Result<(Self, Vec<Timed>)> {
        let path = &in_its_directory(path);
        // Before the file is read: a recorder that has just stopped there may still be writing it
        let claim = claim(path)?;
        let bytes = match read_bounded(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        if bytes.is_empty() || bytes == FILE_HEADER {
            return Ok((Self::over(claim, create(path)?, BUDGET, KEYFRAME_EVERY)?, Vec::new()));
        }
        if let Some(why) = refusal(&bytes) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a recording this broker reads, and is left as it is: {why}", path.display()),
            ));
        }
        let entries = decode(&bytes);
        if entries.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} does not begin with a recording's entry, and is left as it is", path.display()),
            ));
        }
        let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
        private(&file)?;
        let written: u64 = FILE_HEADER.len() as u64 + entries.iter().map(|timed| length(&timed.entry)).sum::<u64>();
        file.set_len(written)?;
        file.seek(SeekFrom::End(0))?;
        let mut recorder = Self::over(claim, file, BUDGET, KEYFRAME_EVERY)?;
        let mut offset = FILE_HEADER.len() as u64;
        for timed in &entries {
            match &timed.entry {
                Entry::Keyframe { state, .. } => {
                    recorder.keyframes.push(offset);
                    recorder.since_keyframe = 0;
                    recorder.last_keyframe = state.len() as u64;
                }
                Entry::Output(output) => recorder.since_keyframe += output.len() as u64,
                Entry::Resize { .. } | Entry::Marks { .. } => {}
            }
            offset += length(&timed.entry);
            // What is recorded from here on happened after everything already there
            recorder.stamped = recorder.stamped.max(timed.at);
        }
        recorder.written = written;
        Ok((recorder, entries))
    }

    /// Whether the recording has stopped: its file could not be written, or the disk fell further behind
    /// than `BACKLOG`.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    fn ask(&mut self, job: Job) -> std::io::Result<()> {
        let stopped = || std::io::Error::other("the recording has stopped");
        if self.failed() {
            return Err(stopped());
        }
        self.jobs.as_ref().ok_or_else(stopped)?.send(job).map_err(|_| stopped())
    }

    fn append(&mut self, kind: u8, payload: &[&[u8]]) -> std::io::Result<()> {
        // The clock, or the entry before when the clock has gone back past it
        self.stamped = self.stamped.max(now());
        let bytes = encode(kind, self.stamped, payload);
        let length = bytes.len();
        if self.shared.waiting.fetch_add(length, Ordering::AcqRel) + length > BACKLOG {
            self.shared.failed.store(true, Ordering::Release);
            return Err(std::io::Error::other("the disk fell too far behind the recording"));
        }
        self.ask(Job::Append(bytes))?;
        self.written += length as u64;
        Ok(())
    }

    /// Records bytes the program wrote.
    pub fn output(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.append(OUTPUT, &[bytes])?;
        self.since_keyframe += bytes.len() as u64;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.append(RESIZE, &[&cols.to_le_bytes(), &rows.to_le_bytes()])
    }

    /// Records what vouches for the shell's marks in the output recorded after this (`Entry::Marks`).
    pub fn marks(&mut self, key: Option<&MarksKey>, last: u64) -> std::io::Result<()> {
        self.append(MARKS, &[&last.to_le_bytes(), key.map_or("", MarksKey::as_str).as_bytes()])
    }

    /// Whether enough has been written since the last keyframe for another, or none has been written yet:
    /// `KEYFRAME_EVERY` bytes of output, or as many as the last keyframe held where that is more.
    ///
    /// **Keyframes are never more of a recording than its output.** A keyframe that is more than the
    /// distance between two is something growing in the terminal, and the core does not say what: an
    /// image a program is part way through sending is written out with the screen, and nothing the core
    /// can be asked says a transmission is open until its last piece arrives. Taken every
    /// `KEYFRAME_EVERY` regardless, each would hold all of the image so far, and an image of megabytes
    /// would be recorded half as many times again as it was sent — keyframes growing with the output in
    /// front of them, which is quadratic in the image. Spaced by their own size, the keyframes between
    /// two moments are at most the output between them and one keyframe more, whatever is growing.
    pub fn keyframe_due(&self) -> bool {
        self.keyframes.is_empty() || self.since_keyframe >= self.every.max(self.last_keyframe)
    }

    /// Puts the keyframe that is due off until as much again has been written as lies between two: for a
    /// terminal that could not be written out now.
    pub fn put_off_keyframe(&mut self) {
        self.since_keyframe = 0;
    }

    /// Whether the recording is past its budget, where only a keyframe lets its oldest part go.
    pub fn over_budget(&self) -> bool {
        self.written > self.budget
    }

    /// Whether the disk is behind: more than half of what may wait to be written (`BACKLOG`) is waiting.
    ///
    /// **What a keyframe waits for.** A keyframe repeats what the recording already holds, so one put off
    /// loses nothing, where output it is queued beside cannot be put off at all. Taken while the disk is
    /// behind, each is one more copy of the screen in what is waiting, up to as much again as the output
    /// it follows (`keyframe_due`), which a writer the machine is slow to run would have to catch up on
    /// with nothing wrong with the disk.
    pub fn behind(&self) -> bool {
        self.shared.waiting.load(Ordering::Acquire) > BACKLOG / 2
    }

    /// Whether a keyframe that is due may be put off for the disk: while it is behind, unless the recording
    /// is past twice its budget, where a keyframe is the only way its oldest part can go.
    pub fn keyframe_may_wait(&self) -> bool {
        self.behind() && !self.far_over_budget()
    }

    /// Whether the recording is so far past its budget that a keyframe can wait for nothing more.
    pub fn far_over_budget(&self) -> bool {
        self.written > self.budget.saturating_mul(2)
    }

    /// Records the whole terminal, and drops the recording's oldest part if it is past its budget.
    pub fn keyframe(&mut self, cols: u16, rows: u16, state: &[u8]) -> std::io::Result<()> {
        let at = self.written;
        self.append(KEYFRAME, &[&cols.to_le_bytes(), &rows.to_le_bytes(), state])?;
        self.keyframes.push(at);
        self.since_keyframe = 0;
        self.last_keyframe = state.len() as u64;
        if self.over_budget() {
            self.drop_oldest()?;
        }
        Ok(())
    }

    /// Drops everything before the first keyframe in the recording's newer half — or before its last,
    /// when that is the only one there.
    fn drop_oldest(&mut self) -> std::io::Result<()> {
        let half = self.written.saturating_sub(self.budget / 2);
        let from = self
            .keyframes
            .iter()
            .copied()
            .find(|offset| *offset >= half)
            .or(self.keyframes.last().copied())
            .unwrap_or(0);
        if from <= FILE_HEADER.len() as u64 {
            return Ok(());
        }
        self.ask(Job::DropBefore(from))?;
        // What is kept follows the header in the new file, so every position moves back to that
        let kept = FILE_HEADER.len() as u64;
        self.written = self.written - from + kept;
        self.keyframes =
            self.keyframes.iter().filter(|offset| **offset >= from).map(|offset| offset - from + kept).collect();
        Ok(())
    }

    /// Waits until everything recorded so far is in the file, for whoever is about to read it.
    pub fn flush(&mut self) {
        if let Some(reached) = self.reached() {
            let _ = reached.recv();
        }
    }

    /// Something to wait on until everything recorded so far is in the file, for a caller that must not
    /// wait while holding what the recorder's owner needs. `None` for a recording that has stopped.
    pub fn reached(&mut self) -> Option<Receiver<()>> {
        let (reached, done) = channel();
        self.ask(Job::Reached(reached)).ok().map(|()| done)
    }
}

impl Drop for Recorder {
    /// **Stopped, and nothing is waited for.** Whoever drops a recorder is a terminal's reader, or is
    /// ending a session, and neither may stand still for a disk. The thread writes what it was handed
    /// and lets go of the file; a recorder that follows on the same file waits for that (`claim`).
    fn drop(&mut self) {
        self.writing.set(Stage::Finishing);
        // The channel closing is what ends the thread, after the last job in it
        self.jobs = None;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn bounded_readers_enforce_exact_limit_and_reject_one_or_more_past_it() {
        assert_eq!(MAX_RECORDING_BYTES, BUDGET * 4);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bounded.recording");

        // File-level bound (`read_bounded_to`)
        std::fs::write(&path, vec![b'a'; 16]).expect("write");
        assert_eq!(read_bounded_to(&path, 16).expect("at limit").len(), 16);
        assert!(read_bounded_to(&path, 15).is_err(), "1 byte over limit rejected");
        assert!(read_bounded_to(&path, 14).is_err(), "2 bytes over limit rejected");

        // Stream-level bound (`read_stream_bounded`, covers pipes/growing files)
        assert_eq!(read_stream_bounded(Cursor::new(vec![b'b'; 16]), 16, &path).expect("at limit").len(), 16);
        assert!(read_stream_bounded(Cursor::new(vec![b'b'; 17]), 16, &path).is_err(), "1 byte over");
        assert!(read_stream_bounded(Cursor::new(vec![b'b'; 18]), 16, &path).is_err(), "2 bytes over");

        // Segment-level bound (`Index::moment_bounded`)
        let mut rec = Recorder::start_with(&path, 1024, 512).expect("started");
        rec.keyframe(80, 24, b"state").expect("keyframe");
        rec.flush();
        let entry_len = (HEADER + 4 + 5) as u64;
        let file_len = FILE_HEADER.len() as u64 + entry_len;
        assert!(Index::open_bounded(&path, file_len).is_some(), "index opens at exact file limit");
        assert!(Index::open_bounded(&path, file_len - 1).is_none(), "index refuses 1 byte past limit");
        assert!(Index::open_bounded(&path, file_len - 2).is_none(), "index refuses 2 bytes past limit");
        let mut index = Index::open(&path).expect("opened");
        assert_eq!(index.moment_bounded(u64::MAX, entry_len).expect("exact limit").len(), 1);
        assert!(index.moment_bounded(u64::MAX, entry_len - 1).is_err(), "1 byte over limit");
        assert!(index.moment_bounded(u64::MAX, entry_len - 2).is_err(), "2 bytes over limit");
    }
}
