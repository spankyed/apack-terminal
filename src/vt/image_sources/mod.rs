//! An image a program names rather than sends — a file on this machine, or a shared memory object — read
//! by the broker and handed to the core as the bytes it holds, the core telling the program what it
//! would have told it had it read the source itself.
//!
//! **The core opens nothing.** A session's core takes no file and no shared memory object
//! (`Screen::allow_images`) but for a command naming one with a NUL in it, which it refuses before it
//! opens anything; so the one way a name reaches the disk is through here, and here it is opened the way
//! the core would not: without waiting (`O_NONBLOCK`, so a FIFO answers at once rather than when
//! something writes to it), asked about through the descriptor it was opened as rather than through its
//! path again, and read without the screen's lock held (`Screen::feed_until_image`). What was read is fed
//! to the core as a transmission of its bytes (`t=d`), with the program's other keys as the core read
//! them, so it answers the program as it would have.
//!
//! **The core decides everything up to the opening.** Whether a frame's image is there, whether an id and
//! a number are both given, whether the format is one it takes, what a program left part sent and what
//! image of the same id goes: all of that is the core's, and is done by the core, to the command with a
//! source it will not open (`Load::asked`). Its answer says whether it got as far as the source; only
//! then is the source read, and a source that cannot be read is refused with the core's own answer. What
//! is written out again here is what the core does once it has a source: which files it reads, which
//! bytes of them, and how a name decodes.
//!
//! **What follows from that.** A FIFO, a directory, a device or a socket is refused without a wait, and
//! so is a file on the filesystems the core refuses: what is mounted at or beneath `/proc`, `/sys` and
//! `/dev`, but for `/dev/shm`. Nothing can be swapped in between the question and the read, because the
//! question is asked of what was opened. And a file that is slow to read holds up its own session's
//! output — as any program writing slowly does — and nothing else: not a view, not a snapshot, not
//! another client of the screen.
//!
//! **A source that has stopped answering is given up on rather than waited for** (`Worker`): it is read
//! on a thread that can be left in a wait nothing can end, and the session goes on once that read has
//! gone `QUIET` without the system answering it.
//!
//! **What was read is sent as the core would have read it, and the core decides the rest**: a source is
//! read up to the most the core reads of one itself (`SOURCE_BYTES`), so an image too large to keep is
//! refused by the core in its own words. It is held in the pieces it is sent in, each let go of once it
//! has been, so a source is held about once, as the core alone holds it — a query a third more, the core
//! holding its one command as base64 until it is whole.
//!
//! **Where a session answers otherwise than the core alone would**: a source given up on as quiet is
//! refused as one that could not be read, where the core alone would have waited on it for ever.

pub mod rules;
mod scan;
mod worker;

#[cfg(unix)]
pub mod posix;
#[cfg(unix)]
use posix as platform;

#[cfg(windows)]
pub mod windows;
#[cfg(windows)]
use windows as platform;

pub use scan::{Base64, Decoded, Keys, PATH_BYTES, Scanner, encoded};
pub use worker::{QUIET, Worker};

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;

/// The most of a source read: the most the core reads of one itself (`max_size`), whether named as a file
/// or as shared memory, and whether or not what is made of it could be kept.
pub const SOURCE_BYTES: u64 = 400 << 20;

/// The most of a source read for a query (`a=q`): a query is sent to the core in one base64 command, so
/// capping it at `IMAGE_STORAGE_BYTES` (64 MiB) prevents a query from spiking memory while still letting
/// any image a session could keep be queried.
pub const QUERY_SOURCE_BYTES: u64 = super::images::IMAGE_STORAGE_BYTES;

/// How much of a source one command carries, and so the pieces it is read and held in. A multiple of
/// three, so every piece but the last is whole groups of base64, and well inside what the core takes in
/// one command.
pub const CHUNK_BYTES: usize = 3 << 20;

/// The most one read of a source asks the system for. **Each answer is progress** (`Worker`), and a read
/// that has gone `QUIET` without one is given up on, so a read asks for no more than a slow source — a
/// network or FUSE mount — answers well inside that: a megabyte in five seconds is 0.2 MB/s. A piece
/// (`CHUNK_BYTES`) is several reads.
pub const READ_BYTES: usize = 1 << 20;

/// The largest width or height the core takes for pixels it is not told the size of (`max_dimension`).
const DIMENSION: u32 = 10_000;

/// The longest name of a shared memory object the core passes on (`NAME_MAX`).
const SHARED_NAME_BYTES: usize = 255;

/// A name with a NUL in it, as base64: what the core is given to answer a command it is not to read for.
/// It refuses such a name before it opens anything, having done all that comes first.
const NOTHING: &[u8] = b"AA==";

/// The core's reply to a command naming a source it could not read, as its reply ends.
const INVALID_DATA: &str = ";EINVAL: invalid data\x1b\\";

/// What a command names as its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A path: on POSIX its bytes, on Windows its text
    File(Vec<u8>),
    /// The name of a shared memory object, passed to `shm_open` as it came
    SharedMemory(Vec<u8>),
    /// A name longer than any path, which the core tries to open and cannot
    Unreadable,
}

/// Why a source the core reached was not read, as the core would say it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// `EINVAL: invalid data`: what it says of a source it cannot open or read, or one of the wrong size
    InvalidData,
    /// `EINVAL: dimensions too large`: a shared memory object whose picture is wider or taller than it takes
    DimensionsTooLarge,
}

impl Refused {
    /// The core's reply to `asked`, its refusal of a source it could not read, saying this instead.
    pub fn reworded(self, asked: &str) -> String {
        match self {
            Self::InvalidData => asked.to_owned(),
            Self::DimensionsTooLarge => asked.replacen("EINVAL: invalid data", "EINVAL: dimensions too large", 1),
        }
    }
}

/// Whether a reply is the core's to a source it could not read: what it answers a name with a NUL in
/// it, and so what says a command reached the reading of its source (`Load::asked`).
pub fn says_unreadable(reply: &str) -> bool {
    reply.ends_with(INVALID_DATA)
}

/// Whether the core passes a shared memory object's name to `shm_open`: one that is not empty, not
/// longer than a name may be, not `/`, and with no NUL in it (`validSharedMemoryName`).
pub fn shared_memory_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= SHARED_NAME_BYTES && name != b"/" && !name.contains(&0)
}

/// How the core would answer a program, given its `q`: everything, only failures, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiet {
    No,
    Ok,
    Failures,
}

/// A command that named its source, taken out of the stream until its source has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Load {
    keys: Keys,
    /// `t` transmits, `T` transmits and shows, `q` asks (the core loads it whole and keeps nothing) and
    /// `f` sends a frame of an image already sent
    action: u8,
    source: Source,
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
        Self { keys, action, source }
    }

    /// What the command named.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// How the core would filter its replies to the command (`execute`): `q` of 1 drops the ones that
    /// say all went well, and more drops them all.
    pub fn quiet(&self) -> Quiet {
        match self.keys.get(b'q') {
            None | Some(0) => Quiet::No,
            Some(1) => Quiet::Ok,
            Some(_) => Quiet::Failures,
        }
    }

    /// Whether the core can be asked if it would read what the command names (`asked`): only a command
    /// with an image's id or number is answered at all.
    pub fn askable(&self) -> bool {
        self.keys.get(b'i').unwrap_or(0) > 0 || self.keys.get(b'I').unwrap_or(0) > 0
    }

    /// Whether the core would read what a command it cannot be asked about names: a transmission, in a
    /// format it takes. A query or a frame needs an id or a number before anything is read.
    pub fn read_unasked(&self) -> bool {
        matches!(self.action, b't' | b'T') && bytes_a_pixel(&self.keys).is_some()
    }

    /// The command, naming a source with a NUL in it, its own string ended and a new one begun: what the
    /// core is fed with its file and shared memory sources allowed, to do all it would do for the command
    /// up to opening its source and refuse it there — or to refuse it sooner, as it would. What a program
    /// sent part of before is abandoned, an image of the same id let go of, a frame's image looked for,
    /// and the reply worded, all by the core.
    ///
    /// **`asked` is the same with its replies let through**, `q` being left out: the core's answer says
    /// whether it got as far as the source (`says_unreadable`), which is when it is read here.
    pub fn refusal(&self) -> Vec<u8> {
        self.with_nothing(b"m")
    }

    /// See `refusal`.
    pub fn asked(&self) -> Vec<u8> {
        self.with_nothing(b"qm")
    }

    fn with_nothing(&self, without: &[u8]) -> Vec<u8> {
        [b"G".as_slice(), &self.keys.spelled(without), b";", NOTHING, b"\x1b\\\x1b_"].concat()
    }

    /// The command, not read: what a replay answers, its recording holding the names it was shown and not
    /// what they named, and what a read given up on answers.
    pub fn refuse(self) -> Loaded {
        Loaded { keys: self.keys, data: Err(Refused::InvalidData) }
    }

    /// Reads what the command named, as the core would have and refusing what it refuses. This is the
    /// part that reaches the disk, and the one to do with nothing locked.
    pub fn read(self) -> Loaded {
        self.read_with(&|| {})
    }

    /// The most bytes this command may read from its source: `QUERY_SOURCE_BYTES` for a query (`a=q`),
    /// which is forwarded in a single base64 command, and `SOURCE_BYTES` otherwise.
    fn source_limit(&self) -> u64 {
        if self.action == b'q' { QUERY_SOURCE_BYTES } else { SOURCE_BYTES }
    }

    /// Whether a regular file `file_len` bytes long can fit inside this command's source limit without
    /// reading it first: when `S` is given, `read_from` caps at `S` directly; when `S` is zero, the tail
    /// from `O` to EOF is read, so a tail already longer than the limit is refused before reading a byte.
    pub fn within_file_limit(&self, file_len: u64) -> bool {
        let limit = self.source_limit();
        let size = u64::from(self.keys.get(b'S').unwrap_or(0));
        if size != 0 {
            return size <= limit;
        }
        let offset = u64::from(self.keys.get(b'O').unwrap_or(0));
        file_len.saturating_sub(offset) <= limit
    }

    /// The same, calling `progress` each time the system answers a read of a file (`Worker`).
    pub fn read_with(self, progress: &dyn Fn()) -> Loaded {
        let data = match (&self.source, bytes_a_pixel(&self.keys)) {
            (_, None) | (Source::Unreadable, _) => Err(Refused::InvalidData),
            (Source::File(path), Some(_)) => platform::open_file(path)
                .filter(|file| file.metadata().is_ok_and(|meta| self.within_file_limit(meta.len())))
                .and_then(|file| self.read_from(file, progress))
                .ok_or(Refused::InvalidData),
            (Source::SharedMemory(name), Some(_)) => platform::shared_memory(name, |size| self.range(size)),
        };
        let query = self.action == b'q';
        Loaded { keys: self.keys, data: data.map(|pieces| Bytes { pieces, query }) }
    }

    /// The bytes of a file the command names, from its `O` offset: its `S` size of them exactly, or to its
    /// end where no size is given (`readFile`), and no more than this command's source limit either way: in
    /// pieces of `CHUNK_BYTES`, the last of them short, and empty where the rest were all there was.
    ///
    /// **Read up to the limit and no further**, so a size or a file of gigabytes costs what the limit does:
    /// a size past it is refused before anything is read, and where no size is given one byte past it is
    /// read, to tell a file that ends at the limit from one that goes on. `progress` is called each time
    /// the system answers a read.
    pub fn read_from(&self, mut source: impl Read + Seek, progress: &dyn Fn()) -> Option<Vec<Vec<u8>>> {
        let max_bytes = self.source_limit();
        let offset = u64::from(self.keys.get(b'O').unwrap_or(0));
        let size = u64::from(self.keys.get(b'S').unwrap_or(0));
        if size > max_bytes {
            return None;
        }
        source.seek(SeekFrom::Start(offset)).ok()?;
        let limit = if size == 0 { max_bytes + 1 } else { size };
        let mut source = Progressing { source: source.take(limit), progress };
        let (mut pieces, mut read) = (Vec::new(), 0);
        loop {
            let piece = read_piece(&mut source)?;
            read += piece.len() as u64;
            let ended = piece.len() < CHUNK_BYTES;
            pieces.push(piece);
            if ended {
                break;
            }
        }
        (read <= max_bytes && (size == 0 || read == size)).then_some(pieces)
    }

    /// Which bytes of a shared memory object `size` long are the image's (`sharedMemoryRange`): from its
    /// `O` offset, its `S` size of them; or with no size given, as many as the picture's pixels are where
    /// those are not compressed, and the rest of the object where they are. A range may run past the
    /// object's end, which whatever takes it out refuses.
    pub fn range(&self, size: u64) -> Result<Range<u64>, Refused> {
        let pixel = bytes_a_pixel(&self.keys).ok_or(Refused::InvalidData)?;
        if size == 0 {
            return Err(Refused::InvalidData);
        }
        let (width, height) = (self.keys.get(b's').unwrap_or(0), self.keys.get(b'v').unwrap_or(0));
        // Asked of every picture whose size is its pixels, whether or not it is what decides the range
        let pixels = match pixel {
            Some(bytes) if width <= DIMENSION && height <= DIMENSION => {
                Some(u64::from(width) * u64::from(height) * u64::from(bytes))
            }
            Some(_) => return Err(Refused::DimensionsTooLarge),
            None => None,
        };
        let start = u64::from(self.keys.get(b'O').unwrap_or(0));
        let length = match (self.keys.get(b'S').unwrap_or(0), self.keys.get(b'o'), pixels) {
            (0, None, Some(pixels)) => pixels,
            (0, _, _) => size.checked_sub(start).ok_or(Refused::InvalidData)?,
            (given, _, _) => u64::from(given),
        };
        if length <= self.source_limit() { Ok(start..start + length) } else { Err(Refused::InvalidData) }
    }
}

/// The next `CHUNK_BYTES` of `source`, or as many as are left of it.
///
/// **Read into room made whole and zeroed by the allocator**, rather than by `read_to_end`, which grows a
/// piece by doubling and zeroes each stretch of it a byte at a time before letting a read write there: a
/// source is up to `SOURCE_BYTES`, and in a debug build — the suite's — that zeroing costs more than the
/// reads, CPU that a busy machine multiplies against a test's deadline, which is a clock.
fn read_piece(source: &mut impl Read) -> Option<Vec<u8>> {
    let mut piece = vec![0; CHUNK_BYTES];
    // **Counted as each read answers, not worked out from the room left.** A full piece is what tells
    // `read_from` there may be more, so a piece is never longer than what was read into it: one said to be
    // full that the source did not fill would have the next piece read, and the next, for ever
    let mut filled = 0;
    let mut room = piece.as_mut_slice();
    while !room.is_empty() {
        let asked = room.len().min(READ_BYTES);
        match source.read(&mut room[..asked]) {
            Ok(0) => break,
            Ok(count) => {
                room = &mut std::mem::take(&mut room)[count..];
                filled += count;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    piece.truncate(filled);
    Some(piece)
}

/// A source being read, calling `progress` each time the system answers.
struct Progressing<'a, R> {
    source: R,
    progress: &'a dyn Fn(),
}

impl<R: Read> Read for Progressing<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let answered = self.source.read(buffer);
        (self.progress)();
        answered
    }
}

/// A command whose source has been read, or why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    keys: Keys,
    data: Result<Bytes, Refused>,
}

/// What was read, in the pieces it is sent in, and whether it answers a query, which is sent in one command
/// and abandons nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bytes {
    pieces: Vec<Vec<u8>>,
    query: bool,
}

impl Loaded {
    /// What to feed the core for a source that was read, inside the string the program began, the
    /// program's own byte ending the string after it; or why it was not read.
    ///
    /// **The command as the core read it, sending what was read.** Its keys are the program's in the
    /// core's own spelling, less the four that said where the bytes were and how many of them (`t`, `O`,
    /// `S`) and whether more were to follow (`m`), which mean something else of bytes that are sent. A
    /// transmission is sent a piece to a command, the first with the keys and each saying whether another
    /// follows; a query is one command, which the core reads whole and keeps nothing of.
    ///
    /// **A transmission begins by abandoning one the program left part sent**, as the core does on a
    /// command whose bytes are named rather than sent: the bytes here are sent, and would otherwise be
    /// taken for that transmission's next piece. A query takes no part in one, and leaves it alone.
    pub fn body(self) -> Result<Body, Refused> {
        let Bytes { pieces, query } = self.data?;
        let mut head = if query { b"G".to_vec() } else { [ABANDON, NEXT].concat() };
        head.extend_from_slice(&self.keys.spelled(b"tmOS"));
        let payload = pieces.iter().map(|piece| piece.len().div_ceil(3) * 4).sum();
        let payload = query.then_some(payload);
        Ok(Body { head: Some(head), pieces: pieces.into_iter().peekable(), query, payload })
    }
}

/// A transmission named by a temporary file, which the core abandons the part sent for and then refuses,
/// saying nothing.
const ABANDON: &[u8] = b"Ga=t,t=116,q=2";

/// What ends one command and begins the next.
const NEXT: &[u8] = b"\x1b\\\x1b_G";

/// What the core is fed for a command whose source was read (`Loaded::body`), a piece at a time: each
/// piece of the source is let go of once it has been encoded to be fed.
#[derive(Debug)]
pub struct Body {
    /// What comes before the first piece, until it has been
    head: Option<Vec<u8>>,
    pieces: std::iter::Peekable<std::vec::IntoIter<Vec<u8>>>,
    query: bool,
    /// What a query's one command carries
    payload: Option<usize>,
}

impl Body {
    /// What the one command a query is sent in carries, as the core counts it — its payload — for the core
    /// to be let take it whole; `None` for a transmission, whose every command is well inside what the core
    /// takes.
    pub fn query_bytes(&self) -> Option<usize> {
        self.payload
    }
}

impl Iterator for Body {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        let piece = match (self.pieces.next(), &self.head) {
            (Some(piece), _) => piece,
            // Nothing was read of a range that was empty, and the command is still sent
            (None, Some(_)) => Vec::new(),
            (None, None) => return None,
        };
        let more = self.pieces.peek().is_some();
        let mut fed = match self.head.take() {
            Some(mut head) if more && !self.query => {
                head.extend_from_slice(if head.ends_with(b"G") { b"m=1;" } else { b",m=1;" });
                head
            }
            Some(mut head) => {
                head.push(b';');
                head
            }
            None if self.query => Vec::new(),
            None => [NEXT, if more { b"m=1;" } else { b"m=0;" }].concat(),
        };
        fed.extend_from_slice(&encoded(&piece));
        Some(fed)
    }
}
