//! Append-only segmented [`WalWriter`] and bounded background [`WalWorker`].
//!
//! Responsibilities:
//! - Monotonic timestamps (`at_ms = at_ms.max(last_at_ms)`).
//! - Direct whole-record `File::write_all` (no `BufWriter` partial-record flushes across 8 KiB edges).
//! - Automatic segment rollover hint at `segment_target_bytes` when the VT parser is at ground.
//! - Non-ground infinite-escape cap (`max_non_ground_bytes`) emitting explicit [`GapReason::NonGroundCap`].
//! - `sync_data()` + 40-byte [`SegmentFooter`] (committing `records_end_offset`) + parent dir sync on seal.
//! - $O(1)$ budget eviction accounting for both segments and `blobs/`, serialized against reader discovery.
//! - Fast crash recovery on resume: sealed closed segments read only record 0 + footer; only an
//!   unsealed active tail segment is scanned and truncated via `File::set_len(valid_records_end)`.
//! - Non-blocking async [`WalWorker`] emitting chronologically ordered [`GapReason::BacklogOverflow`]
//!   records and exposing `take_rollover_needed()` for shard keyframe capture.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use apack_proto::types::{BlobHash, CellPx, GridSize, ImageEncoding, MarksKey};
use sha2::{Digest, Sha256};

use crate::blobs::{WalBlob, blobs_total_bytes, sweep_unreferenced, write_blob};
use crate::directory::{
    RecordingRegistry, WriterClaim, discover_segments, prepare_recording_dir, segment_filename, write_meta_atomic,
};
use crate::format::{
    GapReason, KeyframeV2, MAX_RECORD_PAYLOAD, RecordingMeta, SegmentFooter, WalRecord, encode_framed_record,
    hash_segment_records, scan_segment,
};
use crate::reader::read_sealed_header_and_footer;
use crate::safe_fs::{create_private_file, open_regular_rw, read_regular_file_bounded, sync_dir};
use crate::{
    DEFAULT_BUDGET_BYTES, DEFAULT_MAX_BACKLOG_BYTES, DEFAULT_MAX_NON_GROUND_BYTES, DEFAULT_SEGMENT_TARGET_BYTES,
    MAX_SEGMENT_FILE_BYTES, MIN_SEGMENT_TARGET_BYTES, WalError,
};

/// Maximum queued control commands (resize/marks/segment/seal/barrier) permitted in [`WalWorker`].
pub const MAX_CONTROL_QUEUE_LEN: usize = 1_024;

/// Tuning parameters governing segment size, total budget, non-ground cap, and async backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalConfig {
    /// Target byte size at which an active segment requests a rollover keyframe (clamped `>= 4 KiB`).
    pub segment_target_bytes: u64,
    /// Maximum aggregate bytes retained across segments + blobs before $O(1)$ eviction.
    pub budget_bytes: u64,
    /// Maximum queued bytes accepted by [`WalWorker`] before shedding with `BacklogOverflow`.
    pub max_backlog_bytes: usize,
    /// Maximum continuous bytes written while the VT parser is not in ground state before pausing.
    pub max_non_ground_bytes: u64,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            segment_target_bytes: DEFAULT_SEGMENT_TARGET_BYTES,
            budget_bytes: DEFAULT_BUDGET_BYTES,
            max_backlog_bytes: DEFAULT_MAX_BACKLOG_BYTES,
            max_non_ground_bytes: DEFAULT_MAX_NON_GROUND_BYTES,
        }
    }
}

impl WalConfig {
    /// Effective segment rollover threshold (enforcing [`MIN_SEGMENT_TARGET_BYTES`]).
    pub fn effective_segment_target(&self) -> u64 {
        self.segment_target_bytes.max(MIN_SEGMENT_TARGET_BYTES)
    }
}

/// Hint returned by [`WalWriter::record_output`] telling the session shard whether to capture a
/// fresh [`KeyframeV2`] and roll over to a new segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloverHint {
    /// Active segment is below target size (or VT is not yet at ground).
    Continue,
    /// Active segment reached `segment_target_bytes` at a ground boundary; capture a keyframe next.
    NeedKeyframe,
}

/// Accounting summary returned after budget eviction runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvictionReport {
    /// Number of old closed segments unlinked.
    pub evicted_segments: usize,
    /// Number of unreferenced blob files swept from `blobs/`.
    pub swept_blobs: usize,
    /// Number of candidate segments spared because an open reader pinned them.
    pub skipped_pinned: usize,
}

#[derive(Debug)]
struct ClosedSegment {
    id: u64,
    path: PathBuf,
    size_bytes: u64,
    referenced_blobs: BTreeSet<BlobHash>,
}

struct ActiveSegment {
    id: u64,
    path: PathBuf,
    file: File,
    size_bytes: u64,
    first_ms: u64,
    last_ms: u64,
    record_count: u32,
    hasher: Sha256,
    referenced_blobs: BTreeSet<BlobHash>,
}

/// Synchronous append-only segmented WAL writer.
pub struct WalWriter {
    dir: PathBuf,
    meta: RecordingMeta,
    config: WalConfig,
    claim: WriterClaim,
    registry: RecordingRegistry,
    closed: Vec<ClosedSegment>,
    active: Option<ActiveSegment>,
    /// Blobs written since the previous keyframe, protected from GC until the next rollover keyframe.
    pending_blobs: BTreeSet<BlobHash>,
    next_segment_id: u64,
    last_at_ms: u64,
    non_ground_run: u64,
    non_ground_dropped: u64,
    encode_buf: Vec<u8>,
}

impl WalWriter {
    /// Opens an existing recording directory (recovering any torn active segment) or creates a new one.
    ///
    /// Sealed closed segments read ONLY record 0 (keyframe) + their 40-byte footer; only an
    /// unsealed tail segment is scanned sequentially and truncated at the last valid CRC32C record.
    pub fn open_or_create(
        dir: impl AsRef<Path>,
        session_uuid: [u8; 16],
        created_ms: u64,
        config: WalConfig,
        registry: RecordingRegistry,
    ) -> Result<Self, WalError> {
        let dir = dir.as_ref().to_path_buf();
        let existing_meta = prepare_recording_dir(&dir)?;
        let claim = registry.claim_writer(&dir)?;
        let meta = match existing_meta {
            Some(m) => m,
            None => {
                let m = RecordingMeta::new(created_ms, session_uuid);
                write_meta_atomic(&dir, &m)?;
                m
            }
        };

        let discovered = discover_segments(&dir)?;
        let mut closed = Vec::new();
        let mut active = None;
        let mut next_segment_id = 1u64;
        let mut last_at_ms = 0u64;

        let canonical = claim.canonical_dir().to_path_buf();
        let total = discovered.len();
        for (idx, (seg_id, path)) in discovered.into_iter().enumerate() {
            next_segment_id =
                next_segment_id.max(seg_id.checked_add(1).ok_or(WalError::LimitExceeded("segment id overflow"))?);
            let is_last = idx + 1 == total;

            // Fast path: closed non-tail sealed segments read ONLY record 0 + 72B footer.
            // Never trust the O(1) footer fast path on the tail segment (`is_last`).
            if !is_last
                && let Some((first, footer, file_len)) = read_sealed_header_and_footer(&path)?
                && let WalRecord::Keyframe(kf) = first.record
            {
                last_at_ms = last_at_ms.max(footer.last_ms);
                closed.push(ClosedSegment {
                    id: seg_id,
                    path,
                    size_bytes: file_len,
                    referenced_blobs: kf.referenced_blobs(),
                });
                continue;
            }

            // Tail segment or unsealed/corrupt earlier segment: bounded read + full SHA-256 scan.
            let bytes = read_regular_file_bounded(&path, MAX_SEGMENT_FILE_BYTES, "WAL segment file")?;
            let scan = scan_segment(&bytes);
            let Some(first) = scan.records.first() else {
                // Record 0 was torn/empty or not a KeyframeV2: unlink only if no reader holds a pin.
                let _ = registry.try_unlink_if_unpinned(&canonical, seg_id, || {
                    let _ = fs::remove_file(&path);
                    Ok(())
                });
                continue;
            };
            let WalRecord::Keyframe(kf) = &first.record else {
                let _ = registry.try_unlink_if_unpinned(&canonical, seg_id, || {
                    let _ = fs::remove_file(&path);
                    Ok(())
                });
                continue;
            };

            if let Some(footer) = scan.footer {
                last_at_ms = last_at_ms.max(footer.last_ms);
                closed.push(ClosedSegment {
                    id: seg_id,
                    path,
                    size_bytes: bytes.len() as u64,
                    referenced_blobs: kf.referenced_blobs(),
                });
                continue;
            }

            let seg_last_ms = scan.records.last().map_or(first.at_ms, |r| r.at_ms);
            last_at_ms = last_at_ms.max(seg_last_ms);
            let record_count = u32::try_from(scan.records.len()).unwrap_or(u32::MAX);

            // Atomically check pin state and truncate/seal under the registry mutex so no
            // concurrent `WalReader::open` (`discover_and_pin`) can observe a half-mutated segment.
            let mutated = registry.try_mutate_if_unpinned(&canonical, seg_id, || {
                let (mut file, _) = open_regular_rw(&path, "segment is not a regular file")?;
                if scan.had_torn_tail {
                    file.set_len(scan.valid_records_end as u64)?;
                    file.sync_data()?;
                }
                file.seek(SeekFrom::End(0))?;

                if !is_last {
                    // Earlier unsealed segment: seal it now so `closed` holds only properly sealed segments.
                    let footer = SegmentFooter {
                        first_ms: first.at_ms,
                        last_ms: seg_last_ms,
                        records_end_offset: scan.valid_records_end as u64,
                        record_count,
                        records_sha256: hash_segment_records(&bytes[..scan.valid_records_end]),
                    };
                    let encoded_footer = footer.encode();
                    file.write_all(&encoded_footer)?;
                    file.sync_data()?;
                    Ok((None, Some((scan.valid_records_end + encoded_footer.len()) as u64)))
                } else {
                    Ok((Some(file), None))
                }
            })?;

            match mutated {
                None => {
                    closed.push(ClosedSegment {
                        id: seg_id,
                        path,
                        size_bytes: bytes.len() as u64,
                        referenced_blobs: kf.referenced_blobs(),
                    });
                }
                Some((None, Some(sealed_len))) => {
                    closed.push(ClosedSegment {
                        id: seg_id,
                        path,
                        size_bytes: sealed_len,
                        referenced_blobs: kf.referenced_blobs(),
                    });
                }
                Some((Some(file), None)) => {
                    let mut hasher = Sha256::new();
                    hasher.update(&bytes[..scan.valid_records_end]);
                    active = Some(ActiveSegment {
                        id: seg_id,
                        path,
                        file,
                        size_bytes: scan.valid_records_end as u64,
                        first_ms: first.at_ms,
                        last_ms: seg_last_ms,
                        record_count,
                        hasher,
                        referenced_blobs: kf.referenced_blobs(),
                    });
                }
                Some(_) => unreachable!("try_mutate_if_unpinned returns either sealed len or active file"),
            }
        }

        Ok(Self {
            dir,
            meta,
            config,
            claim,
            registry,
            closed,
            active,
            pending_blobs: BTreeSet::new(),
            next_segment_id,
            last_at_ms,
            non_ground_run: 0,
            non_ground_dropped: 0,
            encode_buf: Vec::new(),
        })
    }

    /// Returns the recording directory path.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the recording metadata from `meta.apkm`.
    pub fn meta(&self) -> RecordingMeta {
        self.meta
    }

    /// Returns the active [`WalConfig`].
    pub fn config(&self) -> WalConfig {
        self.config
    }

    /// Returns the ID of the currently open active segment, if any.
    pub fn active_segment_id(&self) -> Option<u64> {
        self.active.as_ref().map(|a| a.id)
    }

    #[cfg(test)]
    pub(crate) fn active_segment_size(&self) -> Option<u64> {
        self.active.as_ref().map(|a| a.size_bytes)
    }

    /// Returns the number of retained segments (closed + active).
    pub fn segment_count(&self) -> usize {
        self.closed.len() + usize::from(self.active.is_some())
    }

    /// Returns aggregate byte size across all retained segments (closed + active).
    pub fn total_segment_bytes(&self) -> u64 {
        let closed_bytes: u64 = self.closed.iter().map(|s| s.size_bytes).sum();
        closed_bytes.saturating_add(self.active.as_ref().map_or(0, |a| a.size_bytes))
    }

    /// Persists an immutable image blob under `<dir>/blobs/` and pins its hash as live until
    /// captured by the next rollover keyframe (preventing premature GC and serializing sweeps).
    pub fn write_blob(&mut self, encoding: ImageEncoding, bytes: &[u8]) -> Result<BlobHash, WalError> {
        let hash = write_blob(&self.dir, encoding, bytes)?;
        self.pending_blobs.insert(hash);
        if let Some(active) = self.active.as_mut() {
            active.referenced_blobs.insert(hash);
        }
        Ok(hash)
    }

    /// Seals any currently active segment, opens a new segment starting with `keyframe` as record 0,
    /// evicts oldest unpinned closed segments to enforce `config.budget_bytes`, and sweeps
    /// unreferenced image blobs.
    pub fn start_segment_with_keyframe(
        &mut self,
        at_ms: u64,
        keyframe: &KeyframeV2,
    ) -> Result<EvictionReport, WalError> {
        // Validate & encode keyframe BEFORE sealing the active segment or creating any file on disk.
        let effective_at_ms = at_ms.max(self.last_at_ms);
        let mut encoded_kf = Vec::new();
        encode_framed_record(effective_at_ms, &WalRecord::Keyframe(Box::new(keyframe.clone())), &mut encoded_kf)?;

        let seg_id = self.next_segment_id;
        let next_id = seg_id.checked_add(1).ok_or(WalError::LimitExceeded("segment id overflow"))?;

        self.seal_active()?;

        let at_ms = self.monotonic_ms(effective_at_ms);
        let path = self.dir.join(segment_filename(seg_id));
        let mut file = create_private_file(&path)?;
        if let Err(err) = file.write_all(&encoded_kf) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(WalError::Io(err));
        }
        let _ = sync_dir(&self.dir);
        let size_bytes = encoded_kf.len() as u64;

        self.next_segment_id = next_id;
        let mut hasher = Sha256::new();
        hasher.update(&encoded_kf);
        self.active = Some(ActiveSegment {
            id: seg_id,
            path,
            file,
            size_bytes,
            first_ms: at_ms,
            last_ms: at_ms,
            record_count: 1,
            hasher,
            referenced_blobs: keyframe.referenced_blobs(),
        });
        // The newly captured keyframe now reflects the authoritative live image set; clear staged pins.
        self.pending_blobs.clear();
        self.non_ground_run = 0;
        self.non_ground_dropped = 0;

        // `evict_to_budget` sweeps unreferenced blobs both before checking budget and after evicting
        // any image-bearing segment, so images cleared within a single segment are always reclaimed.
        self.evict_to_budget()
    }

    /// Appends raw PTY output bytes to the active segment, enforcing the non-ground escape run cap.
    ///
    /// Returns [`RolloverHint::NeedKeyframe`] when the active segment has reached
    /// `segment_target_bytes` and `vt_at_ground` is `true`, or when the hard segment size cap is hit.
    pub fn record_output(&mut self, at_ms: u64, bytes: &[u8], vt_at_ground: bool) -> Result<RolloverHint, WalError> {
        self.record_output_capped(at_ms, bytes, vt_at_ground, crate::MAX_SEGMENT_FILE_BYTES)
    }

    pub(crate) fn record_output_capped(
        &mut self,
        at_ms: u64,
        bytes: &[u8],
        vt_at_ground: bool,
        max_file_bytes: u64,
    ) -> Result<RolloverHint, WalError> {
        if self.active.is_none() {
            return Err(WalError::CorruptSegment("no active segment"));
        }
        let at_ms = self.monotonic_ms(at_ms);

        if vt_at_ground {
            if self.non_ground_dropped > 0 {
                let dropped = self.non_ground_dropped;
                if !self.append_record_capped(
                    at_ms,
                    &WalRecord::Gap { dropped_bytes: dropped, reason: GapReason::NonGroundCap },
                    max_file_bytes,
                )? {
                    return Ok(RolloverHint::NeedKeyframe);
                }
                self.non_ground_dropped = 0;
            }
            self.non_ground_run = 0;
        } else {
            self.non_ground_run = self.non_ground_run.saturating_add(bytes.len() as u64);
            if self.non_ground_run > self.config.max_non_ground_bytes {
                self.non_ground_dropped = self.non_ground_dropped.saturating_add(bytes.len() as u64);
                return Ok(RolloverHint::Continue);
            }
        }

        // Hard ceiling leaving room for a final Gap record and 40-byte SegmentFooter so an active
        // segment can NEVER exceed `max_file_bytes` and brick subsequent reopen/reads.
        let footer_reserve = crate::SEGMENT_FOOTER_LEN as u64;
        let hard_data_cap = max_file_bytes.saturating_sub(footer_reserve).saturating_sub(1024);
        if !bytes.is_empty() {
            let mut hit_hard_cap = false;
            let mut shed_bytes = 0u64;
            for chunk in bytes.chunks(MAX_RECORD_PAYLOAD) {
                let framed_len = (crate::RECORD_HEADER_LEN
                    .saturating_add(chunk.len())
                    .saturating_add(crate::RECORD_TRAILER_LEN)) as u64;
                let cur_size = self.active.as_ref().map_or(0, |a| a.size_bytes);
                if cur_size.saturating_add(framed_len) > hard_data_cap {
                    hit_hard_cap = true;
                    shed_bytes = shed_bytes.saturating_add(chunk.len() as u64);
                    continue;
                }
                let _ = self.append_record_capped(at_ms, &WalRecord::Output(chunk.to_vec()), max_file_bytes)?;
            }
            if hit_hard_cap {
                let _ = self.append_record_capped(
                    at_ms,
                    &WalRecord::Gap { dropped_bytes: shed_bytes, reason: GapReason::BacklogOverflow },
                    max_file_bytes,
                )?;
                return Ok(RolloverHint::NeedKeyframe);
            }
        }

        let needs_rollover = vt_at_ground
            && self.active.as_ref().is_some_and(|a| a.size_bytes >= self.config.effective_segment_target());
        Ok(if needs_rollover { RolloverHint::NeedKeyframe } else { RolloverHint::Continue })
    }

    /// Appends a `Resize` record to the active segment (returning [`RolloverHint::NeedKeyframe`] if
    /// the hard segment file cap prevents appending without a rollover).
    pub fn record_resize(&mut self, at_ms: u64, size: GridSize, cell_px: CellPx) -> Result<RolloverHint, WalError> {
        self.record_control_capped(at_ms, &WalRecord::Resize { size, cell_px }, crate::MAX_SEGMENT_FILE_BYTES)
    }

    /// Appends a `Marks` record to the active segment (returning [`RolloverHint::NeedKeyframe`] if
    /// the hard segment file cap prevents appending without a rollover).
    pub fn record_marks(&mut self, at_ms: u64, key: Option<MarksKey>, last_seq: u64) -> Result<RolloverHint, WalError> {
        self.record_control_capped(at_ms, &WalRecord::Marks { key, last_seq }, crate::MAX_SEGMENT_FILE_BYTES)
    }

    /// Appends an explicit `Gap` marker to the active segment (returning [`RolloverHint::NeedKeyframe`]
    /// if the hard segment file cap prevents appending without a rollover).
    pub fn record_gap(&mut self, at_ms: u64, dropped_bytes: u64, reason: GapReason) -> Result<RolloverHint, WalError> {
        self.record_control_capped(at_ms, &WalRecord::Gap { dropped_bytes, reason }, crate::MAX_SEGMENT_FILE_BYTES)
    }

    pub(crate) fn record_control_capped(
        &mut self,
        at_ms: u64,
        record: &WalRecord,
        max_file_bytes: u64,
    ) -> Result<RolloverHint, WalError> {
        if self.active.is_none() {
            return Err(WalError::CorruptSegment("no active segment"));
        }
        let at_ms = self.monotonic_ms(at_ms);
        let written = self.append_record_capped(at_ms, record, max_file_bytes)?;
        Ok(if written { RolloverHint::Continue } else { RolloverHint::NeedKeyframe })
    }

    /// Seals the currently active segment by flushing pending non-ground gaps, syncing data,
    /// appending its 40-byte [`SegmentFooter`], and syncing again. Keeps `self.active` intact on
    /// I/O error so no segment is orphaned.
    pub fn seal_active(&mut self) -> Result<(), WalError> {
        if self.non_ground_dropped > 0 && self.active.is_some() {
            let dropped = self.non_ground_dropped;
            let at_ms = self.last_at_ms;
            let _ = self.append_record_capped(
                at_ms,
                &WalRecord::Gap { dropped_bytes: dropped, reason: GapReason::NonGroundCap },
                crate::MAX_SEGMENT_FILE_BYTES,
            )?;
            self.non_ground_dropped = 0;
        }
        self.non_ground_run = 0;

        let Some(active) = self.active.as_mut() else {
            return Ok(());
        };
        active.file.sync_data()?;

        let footer = SegmentFooter {
            first_ms: active.first_ms,
            last_ms: active.last_ms,
            records_end_offset: active.size_bytes,
            record_count: active.record_count,
            records_sha256: active.hasher.clone().finalize().into(),
        };
        let encoded_footer = footer.encode();
        if let Err(err) = active.file.write_all(&encoded_footer).and_then(|()| active.file.sync_data()) {
            let _ = active.file.set_len(active.size_bytes);
            let _ = active.file.seek(SeekFrom::Start(active.size_bytes));
            return Err(WalError::Io(err));
        }
        let _ = sync_dir(&self.dir);

        let sealed = self.active.take().expect("checked Some above");
        self.closed.push(ClosedSegment {
            id: sealed.id,
            path: sealed.path,
            size_bytes: sealed.size_bytes.saturating_add(encoded_footer.len() as u64),
            referenced_blobs: sealed.referenced_blobs,
        });
        Ok(())
    }

    /// Evicts oldest unpinned closed segments until aggregate segment + blob bytes fit within
    /// `config.budget_bytes`, sweeping unreferenced blobs before and during eviction so freeing a
    /// large image blob immediately credits the budget without cascading through unrelated segments.
    pub fn evict_to_budget(&mut self) -> Result<EvictionReport, WalError> {
        let mut report = EvictionReport::default();
        let canonical = self.claim.canonical_dir().to_path_buf();
        // Sweep any already-unreferenced blobs first before counting toward budget.
        let initial_live = self.collect_live_blobs();
        report.swept_blobs = report.swept_blobs.saturating_add(sweep_unreferenced(&self.dir, &initial_live)?);
        let blob_bytes = blobs_total_bytes(&self.dir)?;
        let mut total = self.total_segment_bytes().saturating_add(blob_bytes);

        let old_closed = std::mem::take(&mut self.closed);
        let mut kept = Vec::with_capacity(old_closed.len());
        let mut iter = old_closed.into_iter();
        while let Some(seg) = iter.next() {
            if total <= self.config.budget_bytes {
                kept.push(seg);
                continue;
            }
            let had_blobs = !seg.referenced_blobs.is_empty();
            let unlink_res = self.registry.try_unlink_if_unpinned(&canonical, seg.id, || {
                if let Err(err) = fs::remove_file(&seg.path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(WalError::Io(err));
                }
                Ok(())
            });
            match unlink_res {
                Ok(true) => {
                    total = total.saturating_sub(seg.size_bytes);
                    report.evicted_segments += 1;
                    if had_blobs {
                        // Immediately sweep blobs freed by this segment and credit them to `total`
                        // before deciding whether additional closed segments must be evicted.
                        // Always restore `self.closed` on error so remaining segments are never lost.
                        let mut live = HashSet::new();
                        for s in &kept {
                            live.extend(s.referenced_blobs.iter().copied());
                        }
                        for s in iter.as_slice() {
                            live.extend(s.referenced_blobs.iter().copied());
                        }
                        if let Some(active) = &self.active {
                            live.extend(active.referenced_blobs.iter().copied());
                        }
                        live.extend(self.pending_blobs.iter().copied());
                        let swept = match sweep_unreferenced(&self.dir, &live) {
                            Ok(n) => n,
                            Err(err) => {
                                kept.extend(iter);
                                self.closed = kept;
                                return Err(err);
                            }
                        };
                        report.swept_blobs = report.swept_blobs.saturating_add(swept);
                        let rem_blob_bytes = match blobs_total_bytes(&self.dir) {
                            Ok(b) => b,
                            Err(err) => {
                                kept.extend(iter);
                                self.closed = kept;
                                return Err(err);
                            }
                        };
                        let rem_seg_bytes: u64 = kept
                            .iter()
                            .chain(iter.as_slice().iter())
                            .map(|s| s.size_bytes)
                            .sum::<u64>()
                            .saturating_add(self.active.as_ref().map_or(0, |a| a.size_bytes));
                        total = rem_seg_bytes.saturating_add(rem_blob_bytes);
                    }
                }
                Ok(false) => {
                    report.skipped_pinned += 1;
                    kept.push(seg);
                }
                Err(err) => {
                    kept.push(seg);
                    kept.extend(iter);
                    self.closed = kept;
                    return Err(err);
                }
            }
        }
        self.closed = kept;

        Ok(report)
    }

    fn collect_live_blobs(&self) -> HashSet<BlobHash> {
        let mut live = HashSet::new();
        for seg in &self.closed {
            live.extend(seg.referenced_blobs.iter().copied());
        }
        if let Some(active) = &self.active {
            live.extend(active.referenced_blobs.iter().copied());
        }
        live.extend(self.pending_blobs.iter().copied());
        live
    }

    fn monotonic_ms(&mut self, at_ms: u64) -> u64 {
        let clamped = at_ms.max(self.last_at_ms);
        self.last_at_ms = clamped;
        clamped
    }

    pub(crate) fn append_record_capped(
        &mut self,
        at_ms: u64,
        record: &WalRecord,
        max_file_bytes: u64,
    ) -> Result<bool, WalError> {
        let Some(active) = self.active.as_mut() else {
            return Err(WalError::CorruptSegment("no active segment"));
        };
        self.encode_buf.clear();
        encode_framed_record(at_ms, record, &mut self.encode_buf)?;
        let max_records_end = max_file_bytes.saturating_sub(crate::SEGMENT_FOOTER_LEN as u64);
        if active.size_bytes.saturating_add(self.encode_buf.len() as u64) > max_records_end {
            return Ok(false);
        }
        // Write the complete framed record in one contiguous `write_all` call; roll back length/cursor on failure.
        if let Err(err) = active.file.write_all(&self.encode_buf) {
            let _ = active.file.set_len(active.size_bytes);
            let _ = active.file.seek(SeekFrom::Start(active.size_bytes));
            return Err(WalError::Io(err));
        }
        active.hasher.update(&self.encode_buf);
        active.size_bytes = active.size_bytes.saturating_add(self.encode_buf.len() as u64);
        active.last_ms = active.last_ms.max(at_ms);
        active.record_count = active.record_count.saturating_add(1);
        Ok(true)
    }
}

enum WorkerCmd {
    StartSegment { at_ms: u64, keyframe: Box<KeyframeV2> },
    Output { at_ms: u64, bytes: Vec<u8>, vt_at_ground: bool },
    Resize { at_ms: u64, size: GridSize, cell_px: CellPx },
    Marks { at_ms: u64, key: Option<MarksKey>, last_seq: u64 },
    BacklogGap { at_ms: u64, dropped_bytes: u64 },
    Blob(WalBlob),
    Seal,
    FlushBarrier(mpsc::SyncSender<Result<(), String>>),
}

struct QueueState {
    queue: VecDeque<(usize, WorkerCmd)>,
    queued_bytes: usize,
    control_queued: usize,
    closed: bool,
}

/// Shared background WAL worker thread factory.
pub struct WalWorker;

impl WalWorker {
    /// Spawns a background thread owning `writer` with bounded backlog shedding.
    pub fn spawn(writer: WalWriter) -> WalHandle {
        let max_backlog = writer.config().max_backlog_bytes;
        let shared = Arc::new((
            Mutex::new(QueueState { queue: VecDeque::new(), queued_bytes: 0, control_queued: 0, closed: false }),
            Condvar::new(),
        ));
        let failed = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let rollover_needed = Arc::new(AtomicBool::new(false));

        let worker_shared = Arc::clone(&shared);
        let worker_failed = Arc::clone(&failed);
        let worker_done = Arc::clone(&done);
        let worker_rollover = Arc::clone(&rollover_needed);
        let thread = thread::spawn(move || {
            run_worker_loop(writer, worker_shared, worker_failed, worker_rollover);
            worker_done.store(true, Ordering::Release);
        });

        WalHandle { shared, failed, done, rollover_needed, max_backlog, thread: Some(thread) }
    }
}

pub(crate) const fn should_worker_wait(queue_empty: bool, closed: bool) -> bool {
    queue_empty && !closed
}

pub(crate) fn try_reserve_control_slot(closed: bool, control_queued: &mut usize) -> bool {
    if closed || *control_queued >= MAX_CONTROL_QUEUE_LEN {
        return false;
    }
    *control_queued += 1;
    true
}

pub(crate) fn try_coalesce_output(
    prev_bytes: &mut Vec<u8>,
    prev_ground: bool,
    next_bytes: &[u8],
    next_ground: bool,
    max_payload: usize,
) -> bool {
    if prev_ground != next_ground || prev_bytes.len().saturating_add(next_bytes.len()) > max_payload {
        return false;
    }
    prev_bytes.extend_from_slice(next_bytes);
    true
}

fn note_rollover_hint(rollover_needed: &AtomicBool, hint: RolloverHint) {
    if hint == RolloverHint::NeedKeyframe {
        rollover_needed.store(true, Ordering::Release);
    }
}

fn run_worker_loop(
    mut writer: WalWriter,
    shared: Arc<(Mutex<QueueState>, Condvar)>,
    failed: Arc<AtomicBool>,
    rollover_needed: Arc<AtomicBool>,
) {
    let mut sticky_error: Option<String> = None;

    loop {
        let cmd = {
            let (lock, cv) = &*shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            while should_worker_wait(state.queue.is_empty(), state.closed) {
                state = cv.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            let Some((cost, cmd)) = state.queue.pop_front() else {
                break;
            };
            state.queued_bytes = state.queued_bytes.saturating_sub(cost);
            if matches!(cmd, WorkerCmd::StartSegment { .. } | WorkerCmd::Resize { .. } | WorkerCmd::Marks { .. })
                || matches!(&cmd, WorkerCmd::Output { bytes, .. } if bytes.is_empty())
            {
                state.control_queued = state.control_queued.saturating_sub(1);
            }
            cmd
        };

        match cmd {
            WorkerCmd::FlushBarrier(reply) => {
                let result = match &sticky_error {
                    Some(err) => Err(err.clone()),
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
            other => {
                if sticky_error.is_some() {
                    continue;
                }
                let res = match other {
                    WorkerCmd::StartSegment { at_ms, keyframe } => {
                        rollover_needed.store(false, Ordering::Release);
                        writer.start_segment_with_keyframe(at_ms, &keyframe).map(|_| ())
                    }
                    WorkerCmd::Output { at_ms, bytes, vt_at_ground } => writer
                        .record_output(at_ms, &bytes, vt_at_ground)
                        .map(|hint| note_rollover_hint(&rollover_needed, hint)),
                    WorkerCmd::Resize { at_ms, size, cell_px } => writer
                        .record_resize(at_ms, size, cell_px)
                        .map(|hint| note_rollover_hint(&rollover_needed, hint)),
                    WorkerCmd::Marks { at_ms, key, last_seq } => {
                        writer.record_marks(at_ms, key, last_seq).map(|hint| note_rollover_hint(&rollover_needed, hint))
                    }
                    WorkerCmd::BacklogGap { at_ms, dropped_bytes } => {
                        if writer.active_segment_id().is_some() {
                            writer
                                .record_gap(at_ms, dropped_bytes, GapReason::BacklogOverflow)
                                .map(|hint| note_rollover_hint(&rollover_needed, hint))
                        } else {
                            Ok(())
                        }
                    }
                    WorkerCmd::Blob(blob) => writer.write_blob(blob.encoding(), blob.bytes()).map(|_| ()),
                    WorkerCmd::Seal => writer.seal_active(),
                    WorkerCmd::FlushBarrier(_) => unreachable!("handled above"),
                };
                if let Err(err) = res {
                    sticky_error = Some(err.to_string());
                    failed.store(true, Ordering::Release);
                }
            }
        }
    }
}

/// Non-blocking handle to a background [`WalWorker`] thread.
pub struct WalHandle {
    shared: Arc<(Mutex<QueueState>, Condvar)>,
    failed: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    rollover_needed: Arc<AtomicBool>,
    max_backlog: usize,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn release_empty_output_control_slot(prev_cost: usize, control_queued: &mut usize) {
    if prev_cost == 0 {
        *control_queued = control_queued.saturating_sub(1);
    }
}

pub(crate) fn try_reserve_backlog(queued: &mut usize, cost: usize, max_backlog: usize) -> bool {
    let next = queued.saturating_add(cost);
    if next > max_backlog {
        return false;
    }
    *queued = next;
    true
}

pub(crate) fn try_reserve_blob_backlog(queued: &mut usize, cost: usize, max_backlog: usize) -> bool {
    if *queued == 0 {
        *queued = cost;
        return true;
    }
    try_reserve_backlog(queued, cost, max_backlog)
}

impl WalHandle {
    /// Returns `true` if a disk failure permanently marked this recording failed.
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Returns `true` once when the background writer reports [`RolloverHint::NeedKeyframe`],
    /// atomically clearing the flag so the caller can capture and submit a fresh keyframe.
    pub fn take_rollover_needed(&self) -> bool {
        self.rollover_needed.swap(false, Ordering::AcqRel)
    }

    /// Enqueues a new segment starting with `keyframe` (bounded by [`MAX_CONTROL_QUEUE_LEN`]).
    pub fn start_segment(&self, at_ms: u64, keyframe: KeyframeV2) -> bool {
        self.push_control(WorkerCmd::StartSegment { at_ms, keyframe: Box::new(keyframe) })
    }

    /// Enqueues raw PTY output bytes without blocking; sheds with an in-order
    /// [`GapReason::BacklogOverflow`] record coalesced at the tail of the queue if overloaded.
    pub fn write_output(&self, at_ms: u64, bytes: Vec<u8>, vt_at_ground: bool) -> bool {
        if self.is_failed() {
            return false;
        }
        let cost = bytes.len();
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return false;
        }
        if cost == 0 {
            // Coalesce repeated empty ground notifications so zero-byte writes cannot grow the queue unboundedly.
            if matches!(
                state.queue.back(),
                Some((_, WorkerCmd::Output { bytes: prev, vt_at_ground: prev_g, .. }))
                    if prev.is_empty() && *prev_g == vt_at_ground
            ) {
                return true;
            }
            if !try_reserve_control_slot(state.closed, &mut state.control_queued) {
                return false;
            }
            state.queue.push_back((0, WorkerCmd::Output { at_ms, bytes, vt_at_ground }));
            cv.notify_one();
            return true;
        }
        if !try_reserve_backlog(&mut state.queued_bytes, cost, self.max_backlog) {
            if let Some((_, WorkerCmd::BacklogGap { at_ms: gap_ms, dropped_bytes })) = state.queue.back_mut() {
                *dropped_bytes = dropped_bytes.saturating_add(cost as u64);
                *gap_ms = (*gap_ms).max(at_ms);
            } else {
                state.queue.push_back((0, WorkerCmd::BacklogGap { at_ms, dropped_bytes: cost as u64 }));
            }
            cv.notify_one();
            return false;
        }
        // Coalesce adjacent output chunks with matching ground state up to `MAX_RECORD_PAYLOAD`.
        if let Some((prev_cost, WorkerCmd::Output { at_ms: prev_ms, bytes: prev_bytes, vt_at_ground: prev_g })) =
            state.queue.back_mut()
            && try_coalesce_output(prev_bytes, *prev_g, &bytes, vt_at_ground, MAX_RECORD_PAYLOAD)
        {
            let was_cost = *prev_cost;
            *prev_cost = prev_cost.saturating_add(cost);
            *prev_ms = (*prev_ms).max(at_ms);
            release_empty_output_control_slot(was_cost, &mut state.control_queued);
            cv.notify_one();
            return true;
        }
        state.queue.push_back((cost, WorkerCmd::Output { at_ms, bytes, vt_at_ground }));
        cv.notify_one();
        true
    }

    /// Enqueues a `Resize` record.
    pub fn write_resize(&self, at_ms: u64, size: GridSize, cell_px: CellPx) -> bool {
        self.push_control(WorkerCmd::Resize { at_ms, size, cell_px })
    }

    /// Enqueues a `Marks` record.
    pub fn write_marks(&self, at_ms: u64, key: Option<MarksKey>, last_seq: u64) -> bool {
        self.push_control(WorkerCmd::Marks { at_ms, key, last_seq })
    }

    /// Enqueues an immutable non-empty blob write (`blobs/<sha256>.<ext>`), respecting
    /// `max_backlog_bytes` while still admitting a single valid blob up to [`MAX_BLOB_FILE_BYTES`]
    /// when the queue is empty. Rejects empty/oversized blobs or writes after worker close without
    /// poisoning the background worker.
    pub fn write_blob(&self, encoding: ImageEncoding, bytes: Vec<u8>) -> bool {
        if self.is_failed() {
            return false;
        }
        let Ok(blob) = WalBlob::new(encoding, bytes) else {
            return false;
        };
        let cost = blob.bytes().len();
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || !try_reserve_blob_backlog(&mut state.queued_bytes, cost, self.max_backlog) {
            return false;
        }
        state.queue.push_back((cost, WorkerCmd::Blob(blob)));
        cv.notify_one();
        true
    }

    /// Enqueues an explicit active-segment seal (never starved by output queue length).
    pub fn seal(&self) -> bool {
        if self.is_failed() {
            return false;
        }
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return false;
        }
        state.queue.push_back((0, WorkerCmd::Seal));
        cv.notify_one();
        true
    }

    /// Blocks until every previously enqueued command has been applied and flushed to OS buffers.
    pub fn flush_barrier(&self) -> Result<(), WalError> {
        self.flush_barrier_timeout(Duration::from_secs(30))
    }

    /// Blocks up to `timeout` until every previously enqueued command has been applied and flushed.
    pub fn flush_barrier_timeout(&self, timeout: Duration) -> Result<(), WalError> {
        if timeout.is_zero() {
            return Err(WalError::Timeout);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let (lock, cv) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            if self.done.load(Ordering::Acquire) {
                return Err(WalError::WorkerClosed);
            }
            state.queue.push_back((0, WorkerCmd::FlushBarrier(tx)));
            cv.notify_one();
        }
        let deadline = Instant::now().checked_add(timeout);
        loop {
            let slice = match deadline {
                Some(dl) => {
                    let rem = dl.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        return Err(WalError::Timeout);
                    }
                    rem.min(Duration::from_millis(25))
                }
                None => Duration::from_millis(25),
            };
            match rx.recv_timeout(slice) {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(msg)) => return Err(WalError::Io(std::io::Error::other(msg))),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(WalError::WorkerClosed),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.done.load(Ordering::Acquire) {
                        return Err(WalError::WorkerClosed);
                    }
                }
            }
        }
    }

    /// Flushes pending work, shuts down the background worker thread, and joins it cleanly.
    pub fn shutdown(mut self) -> Result<(), WalError> {
        let flush_res = self.flush_barrier();
        self.stop_thread();
        flush_res
    }

    fn push_control(&self, cmd: WorkerCmd) -> bool {
        if self.is_failed() {
            return false;
        }
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !try_reserve_control_slot(state.closed, &mut state.control_queued) {
            return false;
        }
        state.queue.push_back((0, cmd));
        cv.notify_one();
        true
    }

    pub(crate) fn stop_thread(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            cv.notify_all();
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WalHandle {
    fn drop(&mut self) {
        self.stop_thread();
    }
}
