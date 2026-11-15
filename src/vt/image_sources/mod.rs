//! An image a program names rather than sends — a file on this machine, or a shared memory object — read
//! by the broker and handed to the core as the bytes it holds.
//!
//! **The core opens nothing.** A session's core never takes a file or a shared memory object
//! (`Screen::allow_images`), so the one way a name reaches the disk is through here, and here it is
//! opened the way the core would not: without waiting (`O_NONBLOCK`, so a FIFO answers at once rather
//! than when something writes to it), asked about through the descriptor it was opened as rather than
//! through its path again, and read without the screen's lock held (`Screen::feed_until_load`). What
//! was read is fed to the core as a transmission of its bytes (`t=d`), with the program's other keys as
//! the core read them, so it answers the program as it would have.
//!
//! **What follows from that.** A FIFO, a directory, a device or a socket is refused without a wait, and
//! so is a file on the filesystems the core refuses (`/proc`, `/sys`, `/dev`). Nothing can be swapped in
//! between the question and the read, because the question is asked of what was opened. And a file
//! that is slow to read holds up its own session's output — as any program writing slowly does — and
//! nothing else: not a view, not a snapshot, not another client of the screen.
//!
//! **What is not looked at.** A regular file on a mount that has stopped answering is read until it
//! answers, and nothing can cancel a read the system is in the middle of; the session's own output waits
//! for it. A regular file on another filesystem mounted beneath `/proc` or `/sys` is read like any other.

pub mod rules;
mod scan;

#[cfg(unix)]
pub mod posix;
#[cfg(unix)]
use posix as platform;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
use windows as platform;

pub use scan::{CONTROL_BYTES, Keys, PATH_BYTES, Scanner, encoded};

use super::images::IMAGE_STORAGE_BYTES;

/// The most of a source read: no more than a session's images may hold altogether, which is all an image
/// read from one could be — compressed or a PNG it is smaller than its pixels, and its pixels are kept.
pub const SOURCE_BYTES: u64 = IMAGE_STORAGE_BYTES;

/// How much of a source one command carries. A multiple of three, so every piece but the last is whole
/// groups of base64, and well inside what the core takes in one command.
pub const CHUNK_BYTES: usize = 3 << 20;

/// The most a query may name. The core loads what a query names in one command and keeps none of it
/// (`a=q`), and one command holds at most 65 MiB of base64.
pub const QUERY_BYTES: usize = 48 << 20;

/// The largest width or height the core takes for pixels it is not told the size of (`max_dimension`).
const DIMENSION: u32 = 10_000;

/// What a command names as its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A path: on POSIX its bytes, on Windows its text
    File(Vec<u8>),
    /// The name of a shared memory object, passed to `shm_open` as it came
    SharedMemory(Vec<u8>),
    /// A name that could not be read as one
    Unreadable,
}

/// A command that named its source, taken out of the stream until its source has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Load {
    keys: Keys,
    /// `t` transmits, `T` transmits and shows, `q` asks (the core loads it whole and keeps nothing) and
    /// `f` sends a frame of an image already sent
    action: u8,
    source: Source,
    /// Refused before anything is read
    refused: bool,
}

/// The bytes a format has a pixel, for the formats whose size the core reckons from the picture's.
fn bytes_a_pixel(keys: &Keys) -> Option<Option<u32>> {
    match keys.get(b'f') {
        None | Some(0 | 32) => Some(Some(4)),
        Some(24) => Some(Some(3)),
        // A PNG's size is its own
        Some(100) => Some(None),
        // One the core does not take, which it refuses before it reads anything
        Some(_) => None,
    }
}

impl Load {
    fn new(keys: Keys, action: u8, source: Source) -> Self {
        Self { keys, action, source, refused: false }
    }

    /// What the command named.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The image a frame is for, by its id: a frame is read only for an image that is there, which is the
    /// screen's to say (`refuse_unless`).
    pub fn frame_of(&self) -> Option<u32> {
        (self.action == b'f').then(|| self.keys.get(b'i').unwrap_or(0)).filter(|id| *id > 0)
    }

    /// The same command, to be refused without anything being read unless `there`: what a frame for an
    /// image that is not there is (`ENOENT`).
    pub fn refuse_unless(mut self, there: bool) -> Self {
        self.refused |= !there;
        self
    }

    /// Whether the core answers the command without reading what it names (`execute`, `query`,
    /// `transmitAnimationFrame`): an image given both an id and a number, a query with no id, and a
    /// frame with neither.
    fn answered_unread(&self) -> bool {
        let (id, number) = (self.keys.get(b'i').unwrap_or(0), self.keys.get(b'I').unwrap_or(0));
        self.refused
            || (id > 0 && number > 0)
            || (self.action == b'q' && id == 0)
            || (self.action == b'f' && id == 0 && number == 0)
    }

    /// Reads what the command named, as the core would have and refusing what it refuses. This is the
    /// part that reaches the disk, and the one to do with nothing locked.
    pub fn read(self) -> Loaded {
        let data = match (&self.source, bytes_a_pixel(&self.keys)) {
            _ if self.answered_unread() => None,
            // A format the core does not take is refused before anything is opened
            (_, None) | (Source::Unreadable, _) => None,
            (Source::File(path), Some(_)) => platform::open_file(path).and_then(|file| self.read_file(file)),
            (Source::SharedMemory(name), Some(pixel)) => platform::shared_memory(name, |size| self.range(size, pixel)),
        };
        self.answered(data)
    }

    /// The same command, refused without anything being read: what a replay answers, its recording holding
    /// the names it was shown and not what they named.
    pub fn refuse(self) -> Loaded {
        self.answered(None)
    }

    fn answered(self, data: Option<Vec<u8>>) -> Loaded {
        let query = self.action == b'q';
        let limit = if query { QUERY_BYTES } else { SOURCE_BYTES as usize };
        Loaded { keys: self.keys, query, data: data.filter(|data| data.len() <= limit) }
    }

    /// A file's bytes, from its `O` offset on: its `S` size of them exactly, or to its end where no size
    /// is given (`readFile`).
    fn read_file(&self, mut file: std::fs::File) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let offset = u64::from(self.keys.get(b'O').unwrap_or(0));
        let size = u64::from(self.keys.get(b'S').unwrap_or(0));
        file.seek(SeekFrom::Start(offset)).ok()?;
        if size > 0 {
            if size > SOURCE_BYTES {
                return None;
            }
            let mut data = vec![0; usize::try_from(size).ok()?];
            file.read_exact(&mut data).ok()?;
            return Some(data);
        }
        let mut data = Vec::new();
        file.take(SOURCE_BYTES + 1).read_to_end(&mut data).ok()?;
        (data.len() as u64 <= SOURCE_BYTES).then_some(data)
    }

    /// Which bytes of a shared memory object `size` long are the image's (`sharedMemoryRange`): from its
    /// `O` offset, its `S` size of them; or with no size given, as many as the picture's pixels are where
    /// those are not compressed, and the rest of the object where they are.
    fn range(&self, size: u64, pixel: Option<u32>) -> Option<std::ops::Range<u64>> {
        if size == 0 {
            return None;
        }
        let picture = |bytes: u32| -> Option<u64> {
            let (width, height) = (self.keys.get(b's').unwrap_or(0), self.keys.get(b'v').unwrap_or(0));
            if width > DIMENSION || height > DIMENSION {
                return None;
            }
            Some(u64::from(width) * u64::from(height) * u64::from(bytes))
        };
        // Asked of every picture whose size is its pixels, whether or not it is what decides the range
        let pixels = match pixel {
            Some(bytes) => Some(picture(bytes)?),
            None => None,
        };
        let start = u64::from(self.keys.get(b'O').unwrap_or(0));
        let available = size.checked_sub(start)?;
        let length = match (self.keys.get(b'S').unwrap_or(0), self.keys.get(b'o'), pixels) {
            (0, None, Some(pixels)) => pixels,
            (0, _, _) => available,
            (given, _, _) => u64::from(given),
        };
        (length <= available && length <= SOURCE_BYTES).then_some(start..start + length)
    }
}

/// A command whose source has been read, or refused: what the core is fed in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    keys: Keys,
    query: bool,
    data: Option<Vec<u8>>,
}

impl Loaded {
    /// Whether what was named was read.
    pub fn read(&self) -> bool {
        self.data.is_some()
    }

    /// The body to feed the core inside the string the program began, the program's own byte ending the
    /// string after it.
    ///
    /// **The command as the core read it, sending what was read.** Its keys are the program's in the
    /// core's own spelling, less the four that said where the bytes were and how many of them (`t`, `O`,
    /// `S`) and whether more were to follow (`m`), which mean something else of bytes that are sent. Sent
    /// in pieces of `CHUNK_BYTES`, the first with the keys and each saying whether another follows.
    ///
    /// **A transmission begins by abandoning one the program left part sent**, as the core does on a
    /// command whose bytes are named rather than sent: the bytes here are sent, and would otherwise be
    /// taken for that transmission's next piece. A query takes no part in one, and leaves it alone.
    ///
    /// **A source that was not read is sent as nothing**, which the core answers as it answers a file it
    /// could not read: `EINVAL: invalid data`, with the program's image number.
    pub fn body(&self) -> Vec<u8> {
        const NEXT: &[u8] = b"\x1b\\\x1b_G";
        let mut body = Vec::new();
        if !self.query {
            // A transmission named by a temporary file, which the core abandons the part sent for and then
            // refuses, saying nothing
            body.extend_from_slice(b"Ga=t,t=116,q=2");
            body.extend_from_slice(NEXT);
        } else {
            body.push(b'G');
        }
        body.extend_from_slice(&self.keys.spelled(b"tmOS"));
        let data = self.data.as_deref().unwrap_or_default();
        let pieces: Vec<&[u8]> = if self.query { vec![data] } else { data.chunks(CHUNK_BYTES).collect() };
        for (index, piece) in pieces.iter().enumerate() {
            let more = index + 1 < pieces.len();
            if index > 0 {
                body.extend_from_slice(NEXT);
                body.extend_from_slice(if more { b"m=1" } else { b"m=0" });
            } else if more {
                body.extend_from_slice(if body.ends_with(b"G") { b"m=1" } else { b",m=1" });
            }
            body.push(b';');
            body.extend_from_slice(&encoded(piece));
        }
        if pieces.is_empty() {
            body.push(b';');
        }
        body
    }
}
