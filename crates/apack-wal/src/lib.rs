//! Immutable segmented write-ahead log (WAL) and dual-layer keyframe storage for `apack-terminal` v2.
//!
//! Zero `unsafe` code. Every recording lives inside an owner-private directory (`0o700` on Unix)
//! containing:
//!
//! ```text
//! <recording-dir>/
//! ├── meta.apkm                # 40B CRC32C-verified metadata header
//! ├── blobs/<sha256>.png|zst   # Immutable content-addressed image blobs
//! ├── seg-000001.apks          # Sealed immutable segment (KEYFRAME_V2 + records + 72B SHA-256 footer)
//! └── seg-000002.apks          # Active append-only tail segment
//! ```
//!
//! Operational guarantees:
//! - **$O(1)$ budget eviction:** unlinks oldest unpinned closed segments and sweeps unreferenced
//!   blobs — zero full-file copies and zero Windows rename sharing violations.
//! - **Crash-only safety:** `sync_data()` + directory sync on segment seal; on resume, closed
//!   non-tail segments read only record 0 + their 72-byte footer while the tail segment and any
//!   unsealed segment are scanned with SHA-256 verification and truncated at the last valid CRC32C
//!   boundary via `set_len()`.
//! - **Dual-layer `KeyframeV2`:** always carries a broker-owned, version-stable [`VisualKeyframe`]
//!   plus an optional ABI-hashed [`EngineCheckpoint`] (`libghostty-vt` snapshot) for lossless
//!   restore across matching builds with automatic visual fallback on VT upgrades.
//! - **Bounded non-ground & backlog handling:** never drops bytes silently — emits explicit,
//!   chronologically ordered [`WalRecord::Gap`] records whenever non-ground streams or slow disks
//!   hit configured caps.

#![forbid(unsafe_code)]

use std::fmt;
use std::io;

use apack_proto::codec::WireError;

pub mod blobs;
pub mod crc32c;
pub mod directory;
pub mod format;
pub mod reader;
pub mod reaper;
pub(crate) mod safe_fs;
pub mod writer;

#[cfg(test)]
mod tests;

pub use blobs::{WalBlob, blob_filename, hash_blob, parse_blob_filename, read_blob, sweep_unreferenced, write_blob};
pub use directory::{PinGuard, RecordingRegistry, WriterClaim};
pub use format::{
    EngineCheckpoint, FORMAT_VERSION, GapReason, KeyframeV2, MAX_ENGINE_SNAPSHOT_BYTES, MAX_KEYFRAME_HISTORY_ROWS,
    MAX_KEYFRAME_UNCOMPRESSED, MAX_RECORD_PAYLOAD, META_LEN, META_MAGIC, RECORD_HEADER_LEN, RECORD_TRAILER_LEN,
    RecordingMeta, SEGMENT_FOOTER_LEN, SEGMENT_FOOTER_MAGIC, SegmentFooter, TimedRecord, VisualKeyframe, WalRecord,
    hash_segment_records,
};
pub use reader::{SeekSlice, SegmentSummary, WalReader};
pub use reaper::{MAX_REAP_HANDLES, WalReaper};
pub use writer::{EvictionReport, RolloverHint, WalConfig, WalHandle, WalWorker, WalWriter};

/// Default target size for sealing an active segment (`2 MiB`).
pub const DEFAULT_SEGMENT_TARGET_BYTES: u64 = 2 << 20;

/// Minimum allowed segment target size (`4 KiB`).
pub const MIN_SEGMENT_TARGET_BYTES: u64 = 4 * 1024;

/// Default total on-disk segment + blob budget (`32 MiB`).
pub const DEFAULT_BUDGET_BYTES: u64 = 32 << 20;

/// Default asynchronous disk worker backlog cap (`16 MiB`).
pub const DEFAULT_MAX_BACKLOG_BYTES: usize = 16 << 20;

/// Default maximum bytes written during a continuous non-ground escape run (`4 MiB`).
pub const DEFAULT_MAX_NON_GROUND_BYTES: u64 = 4 << 20;

/// Maximum size of a single content-addressed blob file (`64 MiB`).
pub const MAX_BLOB_FILE_BYTES: u64 = 64 << 20;

/// Hard maximum size of a single segment file read into memory (`64 MiB`).
pub const MAX_SEGMENT_FILE_BYTES: u64 = 64 << 20;

/// Errors returned by WAL directory, blob, writer, or reader operations.
#[derive(Debug)]
pub enum WalError {
    /// Underlying filesystem I/O failure.
    Io(io::Error),
    /// Corrupt or malformed wire encoding inside a WAL record or metadata file.
    Wire(WireError),
    /// CRC32C checksum mismatch on metadata, record, or segment footer.
    ChecksumMismatch(&'static str),
    /// Target directory already exists with non-recording files (lacks valid `meta.apkm`).
    ForeignDirectory,
    /// Target directory or file path is a symbolic link (refused for security).
    SymlinkRefused,
    /// Another active writer in this process already owns the recording directory.
    AlreadyClaimed,
    /// Unsupported format magic, non-regular file, or version mismatch.
    InvalidFormat(&'static str),
    /// Payload, segment file, or decompressed keyframe exceeded a hard memory bound.
    LimitExceeded(&'static str),
    /// Zlib decompression failure inside a compressed record.
    DecompressFailed,
    /// Content-addressed blob failed SHA-256 verification on read.
    CorruptBlob,
    /// Segment or recording violates structural invariant (e.g., record 0 is not a keyframe).
    CorruptSegment(&'static str),
    /// Background WAL worker shut down or exited before answering.
    WorkerClosed,
    /// Caller's flush barrier deadline elapsed while the worker was still running.
    Timeout,
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "WAL I/O error: {err}"),
            Self::Wire(err) => write!(f, "WAL wire error: {err}"),
            Self::ChecksumMismatch(what) => write!(f, "WAL CRC32C mismatch in {what}"),
            Self::ForeignDirectory => {
                write!(f, "refusing foreign, non-private, or non-empty directory without meta.apkm")
            }
            Self::SymlinkRefused => write!(f, "refusing symlinked recording path"),
            Self::AlreadyClaimed => write!(f, "recording directory is already open for writing"),
            Self::InvalidFormat(what) => write!(f, "invalid WAL format: {what}"),
            Self::LimitExceeded(what) => write!(f, "WAL limit exceeded: {what}"),
            Self::DecompressFailed => write!(f, "WAL zlib decompression failed"),
            Self::CorruptBlob => write!(f, "WAL blob SHA-256 verification failed"),
            Self::CorruptSegment(what) => write!(f, "corrupt WAL segment: {what}"),
            Self::WorkerClosed => write!(f, "WAL background worker closed"),
            Self::Timeout => write!(f, "WAL flush barrier timed out"),
        }
    }
}

impl std::error::Error for WalError {}

impl From<io::Error> for WalError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<WireError> for WalError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}
