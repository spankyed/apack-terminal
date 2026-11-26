//! Append-only segmented [`WalWriter`] and bounded background [`WalWorker`].
//!
//! Responsibilities:
//! - Monotonic timestamps (`at_ms = at_ms.max(last_at_ms)`).
//! - Automatic segment rollover hint at `segment_target_bytes` when the VT parser is at ground.
//! - Non-ground infinite-escape cap (`max_non_ground_bytes`) emitting explicit [`GapReason::NonGroundCap`].
//! - `sync_data()` + 32-byte [`SegmentFooter`] on seal.
//! - $O(1)$ budget eviction unlinking oldest unpinned closed segments + sweeping unreferenced blobs.
//! - Torn-tail crash recovery on resume (`File::set_len(valid_records_end)`).
//! - Non-blocking async [`WalWorker`] shedding slow-disk overflow with explicit [`GapReason::BacklogOverflow`].

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use apack_proto::types::{BlobHash, CellPx, GridSize, ImageEncoding, MarksKey};

use crate::blobs::{create_private_file, sweep_unreferenced, write_blob};
use crate::directory::{
    RecordingRegistry, WriterClaim, discover_segments, prepare_recording_dir, segment_filename, write_meta_atomic,
};
use crate::format::{
    GapReason, KeyframeV2, MAX_RECORD_PAYLOAD, RecordingMeta, SegmentFooter, WalRecord, encode_framed_record,
    scan_segment,
};
use crate::{
    DEFAULT_BUDGET_BYTES, DEFAULT_MAX_BACKLOG_BYTES, DEFAULT_MAX_NON_GROUND_BYTES, DEFAULT_SEGMENT_TARGET_BYTES,
    MIN_SEGMENT_TARGET_BYTES, WalError,
};

/// Tuning parameters governing segment size, total budget, non-ground cap, and async backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalConfig {
    /// Target byte size at which an active segment requests a rollover keyframe (clamped `>= 4 KiB`).
    pub segment_target_bytes: u64,
    /// Maximum aggregate bytes retained across closed + active segments before $O(1)$ eviction.
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
    writer: BufWriter<File>,
    size_bytes: u64,
    first_ms: u64,
    last_ms: u64,
    record_count: u32,
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
    next_segment_id: u64,
    last_at_ms: u64,
    non_ground_run: u64,
    non_ground_dropped: u64,
    encode_buf: Vec<u8>,
}

impl WalWriter {
    /// Opens an existing recording directory (recovering any torn active segment) or creates a new one.
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

        let total = discovered.len();
        for (idx, (seg_id, path)) in discovered.into_iter().enumerate() {
            next_segment_id = next_segment_id.max(seg_id.saturating_add(1));
            let is_last = idx + 1 == total;
            let bytes = fs::read(&path)?;
            let scan = scan_segment(&bytes);

            if !is_last {
                // Earlier segment: keep if it has a valid keyframe at record 0.
                if let Some(first) = scan.records.first()
                    && let WalRecord::Keyframe(kf) = &first.record
                {
                    let seg_last_ms = scan.records.last().map_or(first.at_ms, |r| r.at_ms);
                    last_at_ms = last_at_ms.max(seg_last_ms);
                    closed.push(ClosedSegment {
                        id: seg_id,
                        path,
                        size_bytes: bytes.len() as u64,
                        referenced_blobs: kf.referenced_blobs(),
                    });
                }
                continue;
            }

            // Last segment: either already sealed (stays in `closed`) or active tail (crash-recover & resume).
            if scan.footer.is_some() {
                if let Some(first) = scan.records.first()
                    && let WalRecord::Keyframe(kf) = &first.record
                {
                    let seg_last_ms = scan.records.last().map_or(first.at_ms, |r| r.at_ms);
                    last_at_ms = last_at_ms.max(seg_last_ms);
                    closed.push(ClosedSegment {
                        id: seg_id,
                        path,
                        size_bytes: bytes.len() as u64,
                        referenced_blobs: kf.referenced_blobs(),
                    });
                }
                continue;
            }

            // Unsealed active tail segment: truncate torn tail if present.
            let Some(first) = scan.records.first() else {
                // Record 0 was torn/empty; remove unusable segment cleanly.
                let _ = fs::remove_file(&path);
                continue;
            };
            let WalRecord::Keyframe(kf) = &first.record else {
                let _ = fs::remove_file(&path);
                continue;
            };

            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            if scan.had_torn_tail {
                file.set_len(scan.valid_records_end as u64)?;
                file.sync_data()?;
            }
            file.seek(SeekFrom::End(0))?;
            let seg_last_ms = scan.records.last().map_or(first.at_ms, |r| r.at_ms);
            last_at_ms = last_at_ms.max(seg_last_ms);
            active = Some(ActiveSegment {
                id: seg_id,
                path,
                writer: BufWriter::new(file),
                size_bytes: scan.valid_records_end as u64,
                first_ms: first.at_ms,
                last_ms: seg_last_ms,
                record_count: u32::try_from(scan.records.len()).unwrap_or(u32::MAX),
                referenced_blobs: kf.referenced_blobs(),
            });
        }

        Ok(Self {
            dir,
            meta,
            config,
            claim,
            registry,
            closed,
            active,
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

    /// Returns the number of retained segments (closed + active).
    pub fn segment_count(&self) -> usize {
        self.closed.len() + usize::from(self.active.is_some())
    }

    /// Returns aggregate byte size across all retained segments (closed + active).
    pub fn total_segment_bytes(&self) -> u64 {
        let closed_bytes: u64 = self.closed.iter().map(|s| s.size_bytes).sum();
        closed_bytes.saturating_add(self.active.as_ref().map_or(0, |a| a.size_bytes))
    }

    /// Persists an immutable image blob under `<dir>/blobs/`.
    pub fn write_blob(&self, encoding: ImageEncoding, bytes: &[u8]) -> Result<BlobHash, WalError> {
        write_blob(&self.dir, encoding, bytes)
    }

    /// Seals any currently active segment, opens a new segment starting with `keyframe` as record 0,
    /// and evicts oldest unpinned closed segments to enforce `config.budget_bytes`.
    pub fn start_segment_with_keyframe(
        &mut self,
        at_ms: u64,
        keyframe: &KeyframeV2,
    ) -> Result<EvictionReport, WalError> {
        self.seal_active()?;

        let at_ms = self.monotonic_ms(at_ms);
        let seg_id = self.next_segment_id;
        self.next_segment_id = self.next_segment_id.saturating_add(1);
        let path = self.dir.join(segment_filename(seg_id));

        let file = create_private_file(&path)?;
        let mut writer = BufWriter::new(file);
        self.encode_buf.clear();
        encode_framed_record(at_ms, &WalRecord::Keyframe(Box::new(keyframe.clone())), &mut self.encode_buf)?;
        writer.write_all(&self.encode_buf)?;
        let size_bytes = self.encode_buf.len() as u64;

        self.active = Some(ActiveSegment {
            id: seg_id,
            path,
            writer,
            size_bytes,
            first_ms: at_ms,
            last_ms: at_ms,
            record_count: 1,
            referenced_blobs: keyframe.referenced_blobs(),
        });
        self.non_ground_run = 0;
        self.non_ground_dropped = 0;

        self.evict_to_budget()
    }

    /// Appends raw PTY output bytes to the active segment, enforcing the non-ground escape run cap.
    ///
    /// Returns [`RolloverHint::NeedKeyframe`] when the active segment has reached
    /// `segment_target_bytes` and `vt_at_ground` is `true`.
    pub fn record_output(&mut self, at_ms: u64, bytes: &[u8], vt_at_ground: bool) -> Result<RolloverHint, WalError> {
        if self.active.is_none() {
            return Err(WalError::CorruptSegment("no active segment"));
        }
        let at_ms = self.monotonic_ms(at_ms);

        if vt_at_ground {
            if self.non_ground_dropped > 0 {
                let dropped = self.non_ground_dropped;
                self.non_ground_dropped = 0;
                self.append_record_unchecked(
                    at_ms,
                    &WalRecord::Gap { dropped_bytes: dropped, reason: GapReason::NonGroundCap },
                )?;
            }
            self.non_ground_run = 0;
        } else {
            self.non_ground_run = self.non_ground_run.saturating_add(bytes.len() as u64);
            if self.non_ground_run > self.config.max_non_ground_bytes {
                self.non_ground_dropped = self.non_ground_dropped.saturating_add(bytes.len() as u64);
                return Ok(RolloverHint::Continue);
            }
        }

        if !bytes.is_empty() {
            for chunk in bytes.chunks(MAX_RECORD_PAYLOAD) {
                self.append_record_unchecked(at_ms, &WalRecord::Output(chunk.to_vec()))?;
            }
        }

        let needs_rollover = vt_at_ground
            && self.active.as_ref().is_some_and(|a| a.size_bytes >= self.config.effective_segment_target());
        Ok(if needs_rollover { RolloverHint::NeedKeyframe } else { RolloverHint::Continue })
    }

    /// Appends a `Resize` record to the active segment.
    pub fn record_resize(&mut self, at_ms: u64, size: GridSize, cell_px: CellPx) -> Result<(), WalError> {
        let at_ms = self.monotonic_ms(at_ms);
        self.append_record_unchecked(at_ms, &WalRecord::Resize { size, cell_px })
    }

    /// Appends a `Marks` record to the active segment.
    pub fn record_marks(&mut self, at_ms: u64, key: Option<MarksKey>, last_seq: u64) -> Result<(), WalError> {
        let at_ms = self.monotonic_ms(at_ms);
        self.append_record_unchecked(at_ms, &WalRecord::Marks { key, last_seq })
    }

    /// Appends an explicit `Gap` marker to the active segment.
    pub fn record_gap(&mut self, at_ms: u64, dropped_bytes: u64, reason: GapReason) -> Result<(), WalError> {
        let at_ms = self.monotonic_ms(at_ms);
        self.append_record_unchecked(at_ms, &WalRecord::Gap { dropped_bytes, reason })
    }

    /// Flushes buffered active-segment writes to the OS page cache (without sealing).
    pub fn flush(&mut self) -> Result<(), WalError> {
        if let Some(active) = self.active.as_mut() {
            active.writer.flush()?;
        }
        Ok(())
    }

    /// Seals the currently active segment by flushing pending non-ground gaps, syncing data,
    /// appending its 32-byte [`SegmentFooter`], and syncing again.
    pub fn seal_active(&mut self) -> Result<(), WalError> {
        if self.non_ground_dropped > 0 && self.active.is_some() {
            let dropped = self.non_ground_dropped;
            self.non_ground_dropped = 0;
            let at_ms = self.last_at_ms;
            self.append_record_unchecked(
                at_ms,
                &WalRecord::Gap { dropped_bytes: dropped, reason: GapReason::NonGroundCap },
            )?;
        }
        self.non_ground_run = 0;

        let Some(mut active) = self.active.take() else {
            return Ok(());
        };
        active.writer.flush()?;
        active.writer.get_ref().sync_data()?;

        let footer =
            SegmentFooter { first_ms: active.first_ms, last_ms: active.last_ms, record_count: active.record_count };
        let encoded_footer = footer.encode();
        active.writer.write_all(&encoded_footer)?;
        active.writer.flush()?;
        active.writer.get_ref().sync_data()?;

        self.closed.push(ClosedSegment {
            id: active.id,
            path: active.path,
            size_bytes: active.size_bytes.saturating_add(encoded_footer.len() as u64),
            referenced_blobs: active.referenced_blobs,
        });
        Ok(())
    }

    /// Evicts oldest unpinned closed segments until aggregate segment bytes fit within
    /// `config.budget_bytes`, then sweeps unreferenced image blobs in `blobs/`.
    pub fn evict_to_budget(&mut self) -> Result<EvictionReport, WalError> {
        let mut report = EvictionReport::default();
        let canonical = self.claim.canonical_dir().to_path_buf();
        let mut total = self.total_segment_bytes();
        let old_closed = std::mem::take(&mut self.closed);
        let mut kept = Vec::with_capacity(old_closed.len());
        let mut iter = old_closed.into_iter();
        while let Some(seg) = iter.next() {
            if total <= self.config.budget_bytes {
                kept.push(seg);
                continue;
            }
            if self.registry.is_pinned(&canonical, seg.id) {
                report.skipped_pinned += 1;
                kept.push(seg);
                continue;
            }
            if let Err(err) = fs::remove_file(&seg.path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                kept.push(seg);
                kept.extend(iter);
                self.closed = kept;
                return Err(WalError::Io(err));
            }
            total = total.saturating_sub(seg.size_bytes);
            report.evicted_segments += 1;
        }
        self.closed = kept;

        if report.evicted_segments > 0 {
            let live = self.collect_live_blobs();
            report.swept_blobs = sweep_unreferenced(&self.dir, &live)?;
        }

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
        live
    }

    fn monotonic_ms(&mut self, at_ms: u64) -> u64 {
        let clamped = at_ms.max(self.last_at_ms);
        self.last_at_ms = clamped;
        clamped
    }

    fn append_record_unchecked(&mut self, at_ms: u64, record: &WalRecord) -> Result<(), WalError> {
        let Some(active) = self.active.as_mut() else {
            return Err(WalError::CorruptSegment("no active segment"));
        };
        self.encode_buf.clear();
        encode_framed_record(at_ms, record, &mut self.encode_buf)?;
        active.writer.write_all(&self.encode_buf)?;
        active.size_bytes = active.size_bytes.saturating_add(self.encode_buf.len() as u64);
        active.last_ms = active.last_ms.max(at_ms);
        active.record_count = active.record_count.saturating_add(1);
        Ok(())
    }
}

enum WorkerCmd {
    StartSegment { at_ms: u64, keyframe: Box<KeyframeV2> },
    Output { at_ms: u64, bytes: Vec<u8>, vt_at_ground: bool },
    Resize { at_ms: u64, size: GridSize, cell_px: CellPx },
    Marks { at_ms: u64, key: Option<MarksKey>, last_seq: u64 },
    Blob { encoding: ImageEncoding, bytes: Vec<u8> },
    Seal,
    FlushBarrier(mpsc::SyncSender<Result<(), String>>),
}

struct QueueState {
    queue: VecDeque<(usize, WorkerCmd)>,
    queued_bytes: usize,
    pending_backlog_drop: u64,
    pending_drop_at_ms: u64,
    closed: bool,
}

/// Shared background WAL worker thread factory.
pub struct WalWorker;

impl WalWorker {
    /// Spawns a background thread owning `writer` with bounded backlog shedding.
    pub fn spawn(writer: WalWriter) -> WalHandle {
        let max_backlog = writer.config().max_backlog_bytes;
        let shared = Arc::new((
            Mutex::new(QueueState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                pending_backlog_drop: 0,
                pending_drop_at_ms: 0,
                closed: false,
            }),
            Condvar::new(),
        ));
        let failed = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));

        let worker_shared = Arc::clone(&shared);
        let worker_failed = Arc::clone(&failed);
        let worker_done = Arc::clone(&done);
        let thread = thread::spawn(move || {
            run_worker_loop(writer, worker_shared, worker_failed);
            worker_done.store(true, Ordering::Release);
        });

        WalHandle { shared, failed, done, max_backlog, thread: Some(thread) }
    }
}

pub(crate) const fn should_worker_wait(queue_empty: bool, has_active: bool, pending_drop: u64, closed: bool) -> bool {
    queue_empty && !(has_active && pending_drop > 0) && !closed
}

fn run_worker_loop(mut writer: WalWriter, shared: Arc<(Mutex<QueueState>, Condvar)>, failed: Arc<AtomicBool>) {
    let mut sticky_error: Option<String> = None;

    loop {
        let has_active = writer.active_segment_id().is_some();
        let (maybe_cmd, dropped_before) = {
            let (lock, cv) = &*shared;
            let mut state = lock.lock().expect("wal worker lock poisoned");
            while should_worker_wait(state.queue.is_empty(), has_active, state.pending_backlog_drop, state.closed) {
                state = cv.wait(state).expect("wal worker condvar poisoned");
            }
            let dropped = if has_active && state.pending_backlog_drop > 0 {
                let pair = (state.pending_backlog_drop, state.pending_drop_at_ms);
                state.pending_backlog_drop = 0;
                Some(pair)
            } else {
                None
            };
            let cmd = state.queue.pop_front().map(|(cost, cmd)| {
                state.queued_bytes = state.queued_bytes.saturating_sub(cost);
                cmd
            });
            if cmd.is_none() && dropped.is_none() {
                break;
            }
            (cmd, dropped)
        };

        if let Some((dropped_bytes, drop_at_ms)) = dropped_before
            && sticky_error.is_none()
            && let Err(err) = writer.record_gap(drop_at_ms, dropped_bytes, GapReason::BacklogOverflow)
        {
            sticky_error = Some(err.to_string());
            failed.store(true, Ordering::Release);
        }

        let Some(cmd) = maybe_cmd else {
            continue;
        };

        match cmd {
            WorkerCmd::FlushBarrier(reply) => {
                let result = if let Some(err) = &sticky_error {
                    Err(err.clone())
                } else if let Err(err) = writer.flush() {
                    let msg = err.to_string();
                    sticky_error = Some(msg.clone());
                    failed.store(true, Ordering::Release);
                    Err(msg)
                } else {
                    Ok(())
                };
                let _ = reply.send(result);
            }
            other => {
                if sticky_error.is_some() {
                    continue;
                }
                let res = match other {
                    WorkerCmd::StartSegment { at_ms, keyframe } => {
                        writer.start_segment_with_keyframe(at_ms, &keyframe).map(|_| ())
                    }
                    WorkerCmd::Output { at_ms, bytes, vt_at_ground } => {
                        writer.record_output(at_ms, &bytes, vt_at_ground).map(|_| ())
                    }
                    WorkerCmd::Resize { at_ms, size, cell_px } => writer.record_resize(at_ms, size, cell_px),
                    WorkerCmd::Marks { at_ms, key, last_seq } => writer.record_marks(at_ms, key, last_seq),
                    WorkerCmd::Blob { encoding, bytes } => writer.write_blob(encoding, &bytes).map(|_| ()),
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

    if sticky_error.is_none() {
        let _ = writer.flush();
    }
}

/// Non-blocking handle to a background [`WalWorker`] thread.
pub struct WalHandle {
    shared: Arc<(Mutex<QueueState>, Condvar)>,
    failed: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    max_backlog: usize,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn try_reserve_backlog(queued: &mut usize, cost: usize, max_backlog: usize) -> bool {
    let next = queued.saturating_add(cost);
    if next > max_backlog {
        return false;
    }
    *queued = next;
    true
}

impl WalHandle {
    /// Returns `true` if a disk failure permanently marked this recording failed.
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Enqueues a new segment starting with `keyframe`.
    pub fn start_segment(&self, at_ms: u64, keyframe: KeyframeV2) -> bool {
        self.push_control(WorkerCmd::StartSegment { at_ms, keyframe: Box::new(keyframe) })
    }

    /// Enqueues raw PTY output bytes without blocking; sheds with [`GapReason::BacklogOverflow`]
    /// if the queue exceeds `max_backlog_bytes`.
    pub fn write_output(&self, at_ms: u64, bytes: Vec<u8>, vt_at_ground: bool) -> bool {
        if self.is_failed() {
            return false;
        }
        let cost = bytes.len();
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().expect("wal worker lock poisoned");
        if state.closed {
            return false;
        }
        if !try_reserve_backlog(&mut state.queued_bytes, cost, self.max_backlog) {
            state.pending_backlog_drop = state.pending_backlog_drop.saturating_add(cost as u64);
            state.pending_drop_at_ms = state.pending_drop_at_ms.max(at_ms);
            cv.notify_one();
            return false;
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

    /// Enqueues an immutable blob write (`blobs/<sha256>.<ext>`), respecting `max_backlog_bytes`.
    pub fn write_blob(&self, encoding: ImageEncoding, bytes: Vec<u8>) -> bool {
        if self.is_failed() {
            return false;
        }
        let cost = bytes.len();
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().expect("wal worker lock poisoned");
        if state.closed || !try_reserve_backlog(&mut state.queued_bytes, cost, self.max_backlog) {
            return false;
        }
        state.queue.push_back((cost, WorkerCmd::Blob { encoding, bytes }));
        cv.notify_one();
        true
    }

    /// Enqueues an explicit active-segment seal.
    pub fn seal(&self) -> bool {
        self.push_control(WorkerCmd::Seal)
    }

    /// Blocks until every previously enqueued command has been applied and flushed to OS buffers.
    pub fn flush_barrier(&self) -> Result<(), WalError> {
        self.flush_barrier_timeout(Duration::from_secs(30))
    }

    /// Blocks up to `timeout` until every previously enqueued command has been applied and flushed.
    pub fn flush_barrier_timeout(&self, timeout: Duration) -> Result<(), WalError> {
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let (lock, cv) = &*self.shared;
            let mut state = lock.lock().expect("wal worker lock poisoned");
            if self.done.load(Ordering::Acquire) {
                return Err(WalError::WorkerClosed);
            }
            state.queue.push_back((0, WorkerCmd::FlushBarrier(tx)));
            cv.notify_one();
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(WalError::WorkerClosed);
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(25))) {
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
        let mut state = lock.lock().expect("wal worker lock poisoned");
        if state.closed {
            return false;
        }
        state.queue.push_back((0, cmd));
        cv.notify_one();
        true
    }

    fn stop_thread(&mut self) {
        {
            let (lock, cv) = &*self.shared;
            let mut state = lock.lock().expect("wal worker lock poisoned");
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
