//! Fast footer-indexed [`WalReader`] for `Rewind`, `Seek(at_ms)`, and `Spawn { restore }`.
//!
//! Opening a recording reads only `meta.apkm` plus the 32-byte footers of sealed segments
//! (falling back to scanning only an unsealed active tail segment), and pins every discovered
//! segment in [`RecordingRegistry`] for the lifetime of the reader so concurrent $O(1)$ budget
//! eviction never deletes an open segment.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use apack_proto::types::{BlobHash, ImageEncoding};

use crate::WalError;
use crate::blobs::read_blob;
use crate::directory::{PinGuard, RecordingRegistry, discover_segments, read_meta};
use crate::format::{
    KeyframeV2, RecordingMeta, SEGMENT_FOOTER_LEN, SegmentFooter, TimedRecord, WalRecord, scan_segment,
};

/// Indexed metadata for one segment discovered by [`WalReader::open`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSummary {
    /// Monotonic segment ID (`>= 1`).
    pub id: u64,
    /// Path to `seg-NNNNNN.apks`.
    pub path: PathBuf,
    /// Timestamp of record 0 in this segment.
    pub first_ms: u64,
    /// Timestamp of the last valid record in this segment.
    pub last_ms: u64,
    /// Number of valid records in this segment.
    pub record_count: u32,
    /// `true` if opened in $O(1)$ via its 32-byte sealed [`SegmentFooter`].
    pub sealed: bool,
}

/// Replay slice returned by [`WalReader::records_for_moment`] for `Seek(at_ms)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekSlice {
    /// Segment ID that satisfied `at_ms`.
    pub segment_id: u64,
    /// Dual-layer keyframe from record 0 of the segment.
    pub keyframe: KeyframeV2,
    /// Timestamp of the segment's keyframe.
    pub keyframe_at_ms: u64,
    /// Subsequent records in that segment with `record.at_ms <= at_ms`.
    pub tail_records: Vec<TimedRecord>,
}

/// Read-only handle over a segmented WAL directory with RAII eviction pinning.
pub struct WalReader {
    dir: PathBuf,
    meta: RecordingMeta,
    segments: Vec<SegmentSummary>,
    _pin: PinGuard,
}

impl WalReader {
    /// Opens a recording directory, pins all discovered segments against concurrent eviction,
    /// and indexes their `[first_ms, last_ms]` spans using 32-byte footers whenever sealed.
    pub fn open(dir: impl AsRef<Path>, registry: &RecordingRegistry) -> Result<Self, WalError> {
        let dir = dir.as_ref().to_path_buf();
        let meta = read_meta(&dir)?;
        let discovered = discover_segments(&dir)?;
        let seg_ids: Vec<u64> = discovered.iter().map(|(id, _)| *id).collect();
        let pin = registry.pin_segments(&dir, &seg_ids)?;

        let mut segments = Vec::with_capacity(discovered.len());
        for (seg_id, path) in discovered {
            if let Some(summary) = index_segment_allow_vanished(seg_id, path)? {
                segments.push(summary);
            }
        }

        Ok(Self { dir, meta, segments, _pin: pin })
    }

    /// Returns the recording metadata from `meta.apkm`.
    pub fn meta(&self) -> RecordingMeta {
        self.meta
    }

    /// Returns indexed segment summaries in ascending segment-ID order.
    pub fn segments(&self) -> &[SegmentSummary] {
        &self.segments
    }

    /// Returns the aggregate `(first_ms, last_ms)` time span covered by this recording, if non-empty.
    pub fn span(&self) -> Option<(u64, u64)> {
        let first = self.segments.first()?;
        let last = self.segments.last()?;
        Some((first.first_ms, last.last_ms))
    }

    /// Reads and verifies a content-addressed image blob from `<dir>/blobs/`.
    pub fn read_blob(&self, hash: BlobHash, encoding: ImageEncoding) -> Result<Vec<u8>, WalError> {
        read_blob(&self.dir, hash, encoding)
    }

    /// Reads ONLY the newest segment (for `Spawn { restore }`), returning its keyframe and all
    /// valid subsequent records.
    pub fn latest_segment_records(&self) -> Result<Option<SeekSlice>, WalError> {
        let Some(last) = self.segments.last() else {
            return Ok(None);
        };
        let slice = load_segment_slice(last, u64::MAX)?;
        Ok(Some(slice))
    }

    /// Binary-searches indexed segment bounds and reads **at most 1 segment** to construct the
    /// exact replay slice for `Seek(at_ms)`.
    pub fn records_for_moment(&self, at_ms: u64) -> Result<Option<SeekSlice>, WalError> {
        if self.segments.is_empty() {
            return Ok(None);
        }
        // `partition_point` returns the first index where `seg.first_ms > at_ms`.
        // Subtracting 1 selects the latest segment whose `first_ms <= at_ms` (clamped to 0 if
        // `at_ms` precedes the earliest retained segment).
        let idx = self.segments.partition_point(|seg| seg.first_ms <= at_ms).saturating_sub(1);
        let target = &self.segments[idx];
        let slice = load_segment_slice(target, at_ms)?;
        Ok(Some(slice))
    }
}

pub(crate) fn index_segment_allow_vanished(seg_id: u64, path: PathBuf) -> Result<Option<SegmentSummary>, WalError> {
    match index_segment(seg_id, path) {
        Ok(summary) => Ok(summary),
        Err(WalError::Io(err)) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn index_segment(seg_id: u64, path: PathBuf) -> Result<Option<SegmentSummary>, WalError> {
    if let Some(footer) = try_read_footer(&path)? {
        return Ok(Some(SegmentSummary {
            id: seg_id,
            path,
            first_ms: footer.first_ms,
            last_ms: footer.last_ms,
            record_count: footer.record_count,
            sealed: true,
        }));
    }

    // Unsealed active tail segment: scan sequentially.
    let bytes = fs::read(&path)?;
    let scan = scan_segment(&bytes);
    let Some(first) = scan.records.first() else {
        return Ok(None);
    };
    if !matches!(first.record, WalRecord::Keyframe(_)) {
        return Ok(None);
    }
    let last_ms = scan.records.last().map_or(first.at_ms, |r| r.at_ms);
    Ok(Some(SegmentSummary {
        id: seg_id,
        path,
        first_ms: first.at_ms,
        last_ms,
        record_count: u32::try_from(scan.records.len()).unwrap_or(u32::MAX),
        sealed: false,
    }))
}

pub(crate) fn try_read_footer(path: &Path) -> Result<Option<SegmentFooter>, WalError> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len < SEGMENT_FOOTER_LEN as u64 {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(SEGMENT_FOOTER_LEN as i64)))?;
    let mut buf = [0u8; SEGMENT_FOOTER_LEN];
    file.read_exact(&mut buf)?;
    Ok(SegmentFooter::decode(&buf).ok())
}

fn load_segment_slice(summary: &SegmentSummary, up_to_ms: u64) -> Result<SeekSlice, WalError> {
    let bytes = fs::read(&summary.path)?;
    let scan = scan_segment(&bytes);
    let mut iter = scan.records.into_iter();
    let Some(first) = iter.next() else {
        return Err(WalError::CorruptSegment("segment has no records"));
    };
    let WalRecord::Keyframe(keyframe) = first.record else {
        return Err(WalError::CorruptSegment("segment record 0 is not KeyframeV2"));
    };
    let tail_records = iter.filter(|rec| rec.at_ms <= up_to_ms).collect();
    Ok(SeekSlice { segment_id: summary.id, keyframe: *keyframe, keyframe_at_ms: first.at_ms, tail_records })
}
