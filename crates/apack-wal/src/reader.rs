//! Fast footer-indexed [`WalReader`] for `Rewind`, `Seek(at_ms)`, and `Spawn { restore }`.
//!
//! Opening a recording validates the directory against symlinks, atomically discovers and pins all
//! segments in [`RecordingRegistry`] before reading their 40-byte footers (falling back to a
//! bounded scan only for an unsealed active tail segment), and verifies monotonic segment bounds
//! for binary-search seeking.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use apack_proto::types::{BlobHash, ImageEncoding};

use crate::blobs::{check_file_size, read_blob, read_regular_file_bounded};
use crate::directory::{PinGuard, RecordingRegistry, read_meta};
use crate::format::{
    KeyframeV2, MAX_RECORD_PAYLOAD, MIN_RECORD_LEN, RECORD_HEADER_LEN, RECORD_TRAILER_LEN, RecordingMeta,
    SEGMENT_FOOTER_LEN, SegmentFooter, TimedRecord, WalRecord, check_payload_limit, decode_one_record, scan_segment,
};
use crate::{MAX_SEGMENT_FILE_BYTES, WalError};

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
    /// `true` if opened in $O(1)$ via its 40-byte sealed [`SegmentFooter`].
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
    /// Opens a recording directory, atomically discovers and pins all segments against concurrent
    /// eviction, and indexes their `[first_ms, last_ms]` spans using 40-byte footers whenever sealed.
    pub fn open(dir: impl AsRef<Path>, registry: &RecordingRegistry) -> Result<Self, WalError> {
        let dir = dir.as_ref().to_path_buf();
        let meta = read_meta(&dir)?;
        let (discovered, pin) = registry.discover_and_pin(&dir)?;

        let mut segments: Vec<SegmentSummary> = Vec::with_capacity(discovered.len());
        for (seg_id, path) in discovered {
            if let Some(summary) = index_segment(seg_id, path)? {
                if segments.last().is_some_and(|prev| summary.first_ms < prev.first_ms) {
                    return Err(WalError::CorruptSegment("non-monotonic segment timestamps"));
                }
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
        // Monotonic `first_ms` across `self.segments` was verified in `WalReader::open`.
        let idx = self.segments.partition_point(|seg| seg.first_ms <= at_ms).saturating_sub(1);
        let target = &self.segments[idx];
        let slice = load_segment_slice(target, at_ms)?;
        Ok(Some(slice))
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

    // Unsealed active tail segment: bounded regular-file read + scan.
    let bytes = read_regular_file_bounded(&path, MAX_SEGMENT_FILE_BYTES, "WAL segment file")?;
    let scan = scan_segment(&bytes);
    let Some(first) = scan.records.first() else {
        return Ok(None);
    };
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

/// Reads and verifies a sealed segment's 40-byte footer in $O(1)$, verifying that `file_len`
/// matches `footer.records_end_offset + SEGMENT_FOOTER_LEN` and contains at least one record.
pub(crate) fn try_read_footer(path: &Path) -> Result<Option<SegmentFooter>, WalError> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(WalError::InvalidFormat("segment is not a regular file"));
    }
    let len = meta.len();
    let min_sealed_len = (MIN_RECORD_LEN + SEGMENT_FOOTER_LEN) as u64;
    if len < min_sealed_len {
        return Ok(None);
    }
    file.seek(SeekFrom::End(-(SEGMENT_FOOTER_LEN as i64)))?;
    let mut buf = [0u8; SEGMENT_FOOTER_LEN];
    file.read_exact(&mut buf)?;
    let Ok(footer) = SegmentFooter::decode(&buf) else {
        return Ok(None);
    };
    if footer.records_end_offset != len - SEGMENT_FOOTER_LEN as u64 {
        return Ok(None);
    }
    Ok(Some(footer))
}

/// Reads ONLY record 0 (the `KeyframeV2`) and the 40-byte footer of a sealed segment without
/// loading the entire segment body into RAM.
pub(crate) fn read_sealed_header_and_footer(
    path: &Path,
) -> Result<Option<(TimedRecord, SegmentFooter, u64)>, WalError> {
    let Some(footer) = try_read_footer(path)? else {
        return Ok(None);
    };
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    check_file_size(file_len, MAX_SEGMENT_FILE_BYTES, "WAL segment file")?;
    let mut head = [0u8; RECORD_HEADER_LEN];
    file.read_exact(&mut head)?;
    let payload_len = u32::from_le_bytes(head[12..16].try_into().expect("4 bytes")) as usize;
    check_payload_limit(payload_len, MAX_RECORD_PAYLOAD, "WAL record payload")?;
    let rec_len = RECORD_HEADER_LEN + payload_len + RECORD_TRAILER_LEN;
    if check_file_size(rec_len as u64, footer.records_end_offset, "WAL record 0").is_err() {
        return Ok(None);
    }
    let mut raw = vec![0u8; rec_len];
    raw[0..RECORD_HEADER_LEN].copy_from_slice(&head);
    file.read_exact(&mut raw[RECORD_HEADER_LEN..])?;
    let Ok((first, _)) = decode_one_record(&raw) else {
        return Ok(None);
    };
    if !matches!(first.record, WalRecord::Keyframe(_)) || first.at_ms != footer.first_ms {
        return Ok(None);
    }
    Ok(Some((first, footer, file_len)))
}

fn load_segment_slice(summary: &SegmentSummary, up_to_ms: u64) -> Result<SeekSlice, WalError> {
    let bytes = read_regular_file_bounded(&summary.path, MAX_SEGMENT_FILE_BYTES, "WAL segment file")?;
    let scan = scan_segment(&bytes);
    if summary.sealed && scan.footer.is_none() {
        return Err(WalError::CorruptSegment("sealed segment failed integrity check"));
    }
    let mut iter = scan.records.into_iter();
    let Some(first) = iter.next() else {
        return Err(WalError::CorruptSegment("segment has no records"));
    };
    let WalRecord::Keyframe(keyframe) = first.record else {
        return Err(WalError::CorruptSegment("segment record 0 is not KeyframeV2"));
    };
    let tail_records = iter.take_while(|rec| rec.at_ms <= up_to_ms).collect();
    Ok(SeekSlice { segment_id: summary.id, keyframe: *keyframe, keyframe_at_ms: first.at_ms, tail_records })
}
