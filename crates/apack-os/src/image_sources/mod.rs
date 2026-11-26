//! Sandboxed, off-shard Kitty graphics named-source (`t=f` file / `t=s` shared-memory) scanner
//! and reader.
//!
//! Because `apack-vt-ghostty` explicitly disables all file/temp-file/shared-memory image mediums
//! inside `libghostty-vt` (hostile-PTY boundary), the broker scans APC graphics strings via
//! [`Scanner`], reads approved sources off-shard through [`Worker`] (with a 5 s quiet timeout and
//! 1-stuck-thread-per-session circuit breaker), and feeds the read bytes back to `libghostty-vt` as
//! direct (`t=d`) inline transmissions.

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

/// Maximum bytes read from a named file or shared-memory image source (400 MiB).
pub const SOURCE_BYTES: u64 = 400 << 20;

/// Maximum bytes read for a query (`a=q`), matching `apack-vt-ghostty`'s 64 MiB single-image cap.
pub const QUERY_SOURCE_BYTES: u64 = 64 << 20;

/// Chunk size fed per inline transmission command (3 MiB, a multiple of 3 for whole base64 groups).
pub const CHUNK_BYTES: usize = 3 << 20;

/// Maximum bytes requested per individual `read(2)` syscall so slow filesystems report progress
/// well within [`QUIET`].
pub const READ_BYTES: usize = 1 << 20;

/// Maximum width or height accepted when computing pixel byte counts for shared memory ranges.
const DIMENSION: u32 = 10_000;

/// Maximum POSIX shared memory name length (`NAME_MAX`).
const SHARED_NAME_BYTES: usize = 255;

/// Base64 encoding of a single NUL byte (`0x00`), fed to the VT core so it validates keys and
/// returns its native `EINVAL: invalid data` error without opening anything.
const NOTHING: &[u8] = b"AA==";

/// Core reply suffix indicating the command reached source loading and failed on invalid data.
const INVALID_DATA: &str = ";EINVAL: invalid data\x1b\\";

/// Named source referenced by a Kitty graphics command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Filesystem path bytes (raw bytes on POSIX, UTF-8 text on Windows).
    File(Vec<u8>),
    /// Shared memory object name passed to `shm_open`.
    SharedMemory(Vec<u8>),
    /// Overlong or invalid source name that cannot be opened.
    Unreadable,
}

/// Reason a named source was refused, matching `libghostty-vt`'s Kitty graphics error strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// `EINVAL: invalid data`.
    InvalidData,
    /// `EINVAL: dimensions too large`.
    DimensionsTooLarge,
}

impl Refused {
    /// Rewords the core's `EINVAL: invalid data` probe reply into this refusal reason.
    pub fn reworded(self, asked: &str) -> String {
        match self {
            Self::InvalidData => asked.to_owned(),
            Self::DimensionsTooLarge => asked.replacen("EINVAL: invalid data", "EINVAL: dimensions too large", 1),
        }
    }
}

/// Returns `true` when `reply` is the core's unreadable-source refusal suffix.
pub fn says_unreadable(reply: &str) -> bool {
    reply.ends_with(INVALID_DATA)
}

/// Validates a POSIX shared memory object name (`1..=255` bytes, not `"/"`, no interior NUL).
pub fn shared_memory_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= SHARED_NAME_BYTES && name != b"/" && !name.contains(&0)
}

/// Quietness level (`q=` key) controlling Kitty graphics replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quiet {
    /// Report both OK and error replies (`q=0` or omitted).
    No,
    /// Suppress OK replies (`q=1`).
    Ok,
    /// Suppress all replies (`q>=2`).
    Failures,
}

/// Intercepted Kitty graphics command that named a file or shared-memory source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Load {
    keys: Keys,
    action: u8,
    source: Source,
}

fn bytes_a_pixel(keys: &Keys) -> Option<Option<u32>> {
    match keys.get(b'f') {
        None | Some(0 | 32) => Some(Some(4)),
        Some(24) => Some(Some(3)),
        Some(100) => Some(None),
        Some(_) => None,
    }
}

impl Load {
    pub(crate) fn new(keys: Keys, action: u8, source: Source) -> Self {
        Self { keys, action, source }
    }

    /// Returns the named source.
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Returns the command's reply suppression mode (`q=`).
    pub fn quiet(&self) -> Quiet {
        match self.keys.get(b'q') {
            None | Some(0) => Quiet::No,
            Some(1) => Quiet::Ok,
            Some(_) => Quiet::Failures,
        }
    }

    /// Whether the command carries an image ID (`i>0`) or number (`I>0`) so the core replies to it.
    pub fn askable(&self) -> bool {
        self.keys.get(b'i').unwrap_or(0) > 0 || self.keys.get(b'I').unwrap_or(0) > 0
    }

    /// Whether an unaskable command should still be read directly (`a=t`/`a=T` with valid format).
    pub fn read_unasked(&self) -> bool {
        matches!(self.action, b't' | b'T') && bytes_a_pixel(&self.keys).is_some()
    }

    /// Builds a NUL-payload probe command preserving `q=` so the core rejects the source natively.
    pub fn refusal(&self) -> Vec<u8> {
        self.with_nothing(b"m")
    }

    /// Builds a NUL-payload probe command stripping `q=` so the broker observes the core's reply.
    pub fn asked(&self) -> Vec<u8> {
        self.with_nothing(b"qm")
    }

    fn with_nothing(&self, without: &[u8]) -> Vec<u8> {
        [b"G".as_slice(), &self.keys.spelled(without), b";", NOTHING, b"\x1b\\\x1b_"].concat()
    }

    /// Produces a refused [`Loaded`] result without touching the filesystem.
    pub fn refuse(self) -> Loaded {
        Loaded { keys: self.keys, data: Err(Refused::InvalidData) }
    }

    /// Reads the named source synchronously (without progress callbacks).
    pub fn read(self) -> Loaded {
        self.read_with(&|| {})
    }

    fn source_limit(&self) -> u64 {
        if self.action == b'q' { QUERY_SOURCE_BYTES } else { SOURCE_BYTES }
    }

    /// Returns `true` when a regular file of length `file_len` fits within this command's limit.
    pub fn within_file_limit(&self, file_len: u64) -> bool {
        let limit = self.source_limit();
        let size = u64::from(self.keys.get(b'S').unwrap_or(0));
        if size != 0 {
            return size <= limit;
        }
        let offset = u64::from(self.keys.get(b'O').unwrap_or(0));
        file_len.saturating_sub(offset) <= limit
    }

    /// Reads the named source, invoking `progress` after each OS read syscall (for [`Worker`]).
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

    /// Reads the requested `[O .. O+S]` (or `[O .. EOF]`) range from `source` up to `source_limit`.
    pub fn read_from(&self, source: impl Read + Seek, progress: &dyn Fn()) -> Option<Vec<Vec<u8>>> {
        self.read_from_bounded(source, progress, self.source_limit(), CHUNK_BYTES)
    }

    pub(crate) fn read_from_bounded(
        &self,
        mut source: impl Read + Seek,
        progress: &dyn Fn(),
        max_bytes: u64,
        chunk_bytes: usize,
    ) -> Option<Vec<Vec<u8>>> {
        let offset = u64::from(self.keys.get(b'O').unwrap_or(0));
        let size = u64::from(self.keys.get(b'S').unwrap_or(0));
        if size > max_bytes {
            return None;
        }
        source.seek(SeekFrom::Start(offset)).ok()?;
        let limit = if size == 0 { max_bytes + 1 } else { size };
        let mut source = Progressing { source, progress }.take(limit);
        let chunk_size = chunk_bytes.max(1);
        let (mut pieces, mut read) = (Vec::new(), 0u64);
        loop {
            let piece = read_piece_sized(&mut source, chunk_size)?;
            read += piece.len() as u64;
            let ended = piece.is_empty() || piece.len() < chunk_size;
            pieces.push(piece);
            if ended {
                break;
            }
        }
        (read <= max_bytes && (size == 0 || read == size)).then_some(pieces)
    }

    /// Computes the requested byte range within a shared memory object of `size` bytes.
    pub fn range(&self, size: u64) -> Result<Range<u64>, Refused> {
        let pixel = bytes_a_pixel(&self.keys).ok_or(Refused::InvalidData)?;
        if size == 0 {
            return Err(Refused::InvalidData);
        }
        let (width, height) = (self.keys.get(b's').unwrap_or(0), self.keys.get(b'v').unwrap_or(0));
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

fn read_piece_sized(source: &mut impl Read, chunk_bytes: usize) -> Option<Vec<u8>> {
    let mut piece = vec![0u8; chunk_bytes];
    let mut filled = 0usize;
    let mut room = piece.as_mut_slice();
    while !room.is_empty() {
        let asked = room.len().min(READ_BYTES);
        match source.read(&mut room[..asked]) {
            Ok(0) => break,
            Ok(count) => {
                room = &mut std::mem::take(&mut room)[count..];
                filled += count;
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    piece.truncate(filled);
    Some(piece)
}

struct Progressing<'a, R> {
    source: R,
    progress: &'a dyn Fn(),
}

impl<R: Read> Read for Progressing<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let res = self.source.read(buf);
        (self.progress)();
        res
    }
}

/// Completed off-shard source load result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    keys: Keys,
    data: Result<Bytes, Refused>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Bytes {
    pieces: Vec<Vec<u8>>,
    query: bool,
}

impl Loaded {
    /// Converts the loaded bytes into a streaming iterator of inline (`t=d`) Kitty APC chunks.
    pub fn body(self) -> Result<Body, Refused> {
        let Bytes { pieces, query } = self.data?;
        let mut head = if query { b"G".to_vec() } else { [ABANDON, NEXT].concat() };
        head.extend_from_slice(&self.keys.spelled(b"tmOS"));
        let payload = pieces.iter().map(|piece| piece.len().div_ceil(3) * 4).sum();
        let payload = query.then_some(payload);
        Ok(Body { head: Some(head), pieces: pieces.into_iter().peekable(), query, payload })
    }
}

const ABANDON: &[u8] = b"Ga=t,t=116,q=2";
const NEXT: &[u8] = b"\x1b\\\x1b_G";

/// Streaming base64-encoded inline APC body yielded by [`Loaded::body`].
#[derive(Debug)]
pub struct Body {
    head: Option<Vec<u8>>,
    pieces: std::iter::Peekable<std::vec::IntoIter<Vec<u8>>>,
    query: bool,
    payload: Option<usize>,
}

impl Body {
    /// Returns the total base64 payload size for a query command (`a=q`), or `None` for chunked
    /// transmissions.
    pub fn query_bytes(&self) -> Option<usize> {
        self.payload
    }
}

impl Iterator for Body {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        let piece = match (self.pieces.next(), &self.head) {
            (Some(piece), _) => piece,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn scanner_extracts_file_load_and_reads_real_file_into_inline_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let img = dir.path().join("pic.raw");
        let payload = vec![0x11u8, 0x22, 0x33, 0x44];
        std::fs::write(&img, &payload).expect("write image");

        let b64_path = encoded(img.to_string_lossy().as_bytes());
        let mut stream = Vec::new();
        stream.extend_from_slice(b"Ga=T,f=32,s=1,v=1,i=7,t=f;");
        stream.extend_from_slice(&b64_path);
        stream.extend_from_slice(b"\x1b\\");

        let mut scanner = Scanner::default();
        scanner.begin();
        assert!(scanner.reading());
        let mut passthrough = Vec::new();
        let (consumed, load) = scanner.read(&stream, |b| passthrough.extend_from_slice(b));
        assert!(passthrough.is_empty());
        assert_eq!(&stream[consumed..], b"\x1b\\");
        assert!(!scanner.reading());

        let load = load.expect("extracted load");
        assert!(load.askable());
        assert!(load.read_unasked());
        assert_eq!(load.quiet(), Quiet::No);
        assert!(load.refusal().ends_with(b";AA==\x1b\\\x1b_"));
        assert!(load.asked().ends_with(b";AA==\x1b\\\x1b_"));
        assert!(says_unreadable("\x1b_Gi=7;EINVAL: invalid data\x1b\\"));
        assert!(!says_unreadable("\x1b_Gi=7;OK\x1b\\"));

        assert_eq!(
            Refused::DimensionsTooLarge.reworded("\x1b_Gi=7;EINVAL: invalid data\x1b\\"),
            "\x1b_Gi=7;EINVAL: dimensions too large\x1b\\"
        );
        assert_eq!(
            Refused::InvalidData.reworded("\x1b_Gi=7;EINVAL: invalid data\x1b\\"),
            "\x1b_Gi=7;EINVAL: invalid data\x1b\\"
        );

        let loaded = load.read();
        let chunks: Vec<Vec<u8>> = loaded.body().expect("inline body").collect();
        assert_eq!(chunks.len(), 1);
        let chunk_str = String::from_utf8_lossy(&chunks[0]);
        assert!(chunk_str.starts_with("Ga=t,t=116,q=2\x1b\\\x1b_G"));
        assert!(chunk_str.ends_with(&*String::from_utf8_lossy(&encoded(&payload))));
    }

    #[test]
    fn constants_and_read_piece_error_handling() {
        assert_eq!(SOURCE_BYTES, 400 * 1024 * 1024);
        assert_eq!(QUERY_SOURCE_BYTES, 64 * 1024 * 1024);
        assert_eq!(CHUNK_BYTES, 3 * 1024 * 1024);
        assert_eq!(READ_BYTES, 1024 * 1024);

        struct FlakyReader {
            step: usize,
            fail_hard: bool,
        }
        impl Read for FlakyReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.step += 1;
                assert!(self.step <= 4, "read_piece_sized retried non-EINTR error indefinitely");
                if self.step == 1 {
                    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "eintr"));
                }
                if self.fail_hard {
                    return Err(std::io::Error::other("disk error"));
                }
                if self.step == 2 {
                    buf[..2].copy_from_slice(b"ok");
                    return Ok(2);
                }
                Ok(0)
            }
        }

        let mut ok_reader = FlakyReader { step: 0, fail_hard: false };
        assert_eq!(read_piece_sized(&mut ok_reader, 16), Some(b"ok".to_vec()));

        let mut err_reader = FlakyReader { step: 0, fail_hard: true };
        assert_eq!(read_piece_sized(&mut err_reader, 16), None);
    }

    #[test]
    fn load_range_and_limits_validate_dimensions_and_sizes() {
        let mut scanner = Scanner::default();
        scanner.begin();
        let (_, load) = scanner.read(b"Ga=q,f=32,s=2,v=2,t=s,q=1;L3NobQ==\x1b\\", |_| {});
        let load = load.expect("query load");
        assert_eq!(load.quiet(), Quiet::Ok);
        assert!(!load.askable());
        assert!(!load.read_unasked());
        assert_eq!(load.range(16), Ok(0..16));
        assert_eq!(load.range(0), Err(Refused::InvalidData));
        assert!(load.within_file_limit(QUERY_SOURCE_BYTES));
        assert!(!load.within_file_limit(QUERY_SOURCE_BYTES + 1));

        // Image number `I>0` makes a load askable; explicit `S` governs `within_file_limit`.
        scanner.begin();
        let (_, by_num) = scanner.read(b"Ga=q,I=9,S=100,t=f;L2E=\x1b\\", |_| {});
        let by_num = by_num.expect("by_num");
        assert!(by_num.askable());
        assert!(by_num.within_file_limit(10));
        scanner.begin();
        let (_, over_s) = scanner.read(b"Ga=q,S=67108865,t=f;L2E=\x1b\\", |_| {});
        let over_s = over_s.expect("over_s");
        assert!(!over_s.within_file_limit(10));

        // Oversized dimensions refused with DimensionsTooLarge.
        scanner.begin();
        let (_, big) = scanner.read(b"Gf=24,s=10001,v=1,t=s,q=2;L3NobQ==\x1b\\", |_| {});
        let big = big.expect("big");
        assert_eq!(big.quiet(), Quiet::Failures);
        assert_eq!(big.range(100), Err(Refused::DimensionsTooLarge));

        // Read from cursor with offset and explicit size, plus boundary limit & multi-chunk checks.
        scanner.begin();
        let (_, sliced) = scanner.read(b"Gf=100,O=2,S=3,t=f;L2E=\x1b\\", |_| {});
        let sliced = sliced.expect("sliced");
        let ticks = std::sync::atomic::AtomicUsize::new(0);
        let pieces = sliced
            .read_from(Cursor::new(b"abcdef"), &|| {
                ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("read slice");
        assert_eq!(pieces, vec![b"cde".to_vec()]);
        assert!(ticks.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        assert_eq!(
            sliced.read_from_bounded(Cursor::new(b"abcdef"), &|| {}, 3, 2),
            Some(vec![b"cd".to_vec(), b"e".to_vec()])
        );
        assert!(sliced.read_from_bounded(Cursor::new(b"abcdef"), &|| {}, 2, 2).is_none());
        // Short file fails when S cannot be satisfied.
        assert!(sliced.read_from(Cursor::new(b"ab"), &|| {}).is_none());

        // Unsized read (S=0): exact max_bytes succeeds (even on chunk boundary), max_bytes + 1 fails.
        scanner.begin();
        let (_, unsized_load) = scanner.read(b"Gf=100,t=f;L2E=\x1b\\", |_| {});
        let unsized_load = unsized_load.expect("unsized");
        assert_eq!(
            unsized_load.read_from_bounded(Cursor::new(b"abcd"), &|| {}, 4, 2),
            Some(vec![b"ab".to_vec(), b"cd".to_vec(), Vec::new()])
        );
        assert!(unsized_load.read_from_bounded(Cursor::new(b"abcde"), &|| {}, 4, 2).is_none());

        // Refusing a load produces InvalidData.
        let refused = sliced.refuse();
        assert!(matches!(refused.body(), Err(Refused::InvalidData)));

        // Shared memory name validation.
        assert!(shared_memory_name(b"/valid-name"));
        assert!(!shared_memory_name(b""));
        assert!(!shared_memory_name(b"/"));
        assert!(!shared_memory_name(b"/bad\0name"));
        assert!(!shared_memory_name(&vec![b'a'; SHARED_NAME_BYTES + 1]));
    }

    #[test]
    fn body_splits_multi_chunk_transmissions_and_handles_queries() {
        let mut scanner = Scanner::default();
        scanner.begin();
        let (_, load) = scanner.read(b"Ga=q,f=100,t=f;L2E=\x1b\\", |_| {});
        let load = load.expect("query");
        let loaded =
            Loaded { keys: load.keys, data: Ok(Bytes { pieces: vec![b"abc".to_vec(), b"def".to_vec()], query: true }) };
        let mut body = loaded.body().expect("query body");
        assert_eq!(body.query_bytes(), Some(8));
        let first = body.next().expect("chunk 1");
        let second = body.next().expect("chunk 2");
        assert!(body.next().is_none());
        assert!(!String::from_utf8_lossy(&first).contains("m=1"), "query must not emit m=1");
        assert!(first.ends_with(b";YWJj"));
        assert_eq!(second, b"ZGVm");

        // Single-chunk transmission must not emit m=1.
        scanner.begin();
        let (_, single_tx) = scanner.read(b"Gt=f,i=1;L2E=\x1b\\", |_| {});
        let single_tx = single_tx.expect("single_tx");
        let single_parts: Vec<Vec<u8>> =
            Loaded { keys: single_tx.keys, data: Ok(Bytes { pieces: vec![b"abc".to_vec()], query: false }) }
                .body()
                .expect("single body")
                .collect();
        assert_eq!(single_parts.len(), 1);
        assert!(!String::from_utf8_lossy(&single_parts[0]).contains("m=1"));

        // Multi-chunk transmission sets m=1 on first and m=0 on last.
        scanner.begin();
        let (_, tx_load) = scanner.read(b"Gt=f;L2E=\x1b\\", |_| {});
        let tx_load = tx_load.expect("tx");
        let loaded_tx = Loaded {
            keys: tx_load.keys,
            data: Ok(Bytes { pieces: vec![b"abc".to_vec(), b"def".to_vec()], query: false }),
        };
        let body_tx = loaded_tx.body().expect("tx body");
        assert_eq!(body_tx.query_bytes(), None);
        let parts: Vec<Vec<u8>> = body_tx.collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].ends_with(b"m=1;YWJj"));
        assert!(parts[1].starts_with(b"\x1b\\\x1b_Gm=0;"));
        assert!(parts[1].ends_with(b"ZGVm"));
    }
}
