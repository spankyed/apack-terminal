//! WAL keyframe capture, image-blob persistence/hydration, and transactional VT replay helpers.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use apack_core::marks::MarksMode;
use apack_core::session::{ReplayRecord, RestoreKeyframe, SessionEffect, SessionMachine};
use apack_core::time::WallMs;
use apack_core::vt::VtEngine;
use apack_proto::types::{CellPx, GridSize, ImageEncoding, MarksKey, PackedRow, ResizePolicy, SessionId};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine, GhosttyError, ImageStore};
use apack_wal::{
    EngineCheckpoint, KeyframeV2, MAX_ENGINE_SNAPSHOT_BYTES, MAX_KEYFRAME_HISTORY_ROWS, MAX_KEYFRAME_UNCOMPRESSED,
    MAX_RECORD_PAYLOAD, RecordingRegistry, SeekSlice, VisualKeyframe, WalHandle, WalReader, WalReaper, WalRecord,
};

use crate::{now_unix_ms, now_wall_ms};

use super::LiveSession;

/// Detaches a permanently failed [`WalHandle`] off-shard via `wal_reaper` so hot-shard output
/// turns never spin locking `ImageStore` or capturing keyframes for a dead recorder.
pub(super) fn detach_failed_wal(sess: &mut LiveSession, wal_reaper: &WalReaper) -> bool {
    if !sess.wal.as_ref().is_some_and(WalHandle::is_failed) {
        return false;
    }
    if let Some(wal) = sess.wal.take() {
        wal_reaper.close_async(wal);
    }
    sess.wal_has_segment = false;
    sess.pending_wal_rollover = false;
    sess.wal_written_blobs.clear();
    true
}

/// Persists any newly created image blobs (direct + virtual `U=1`) referenced by `sess.machine`
/// into `sess.wal` (`<dir>/blobs/`), rehydrating from `GhosttyEngine` on `ImageStore` eviction
/// and retrying automatically if `WalWorker` is transiently backlogged.
pub(super) fn sync_wal_image_blobs(
    sess: &mut LiveSession,
    image_store: &Arc<Mutex<ImageStore>>,
    wal_reaper: &WalReaper,
) {
    if detach_failed_wal(sess, wal_reaper) {
        return;
    }
    let Some(wal) = &sess.wal else {
        return;
    };
    let cur_gen = sess.machine.vt().generations().images;
    if cur_gen == sess.wal_images_gen {
        return;
    }
    let blobs = sess.machine.vt().active_image_blobs();
    let mut active = HashSet::with_capacity(blobs.len());
    let mut all_persisted = true;
    for blob in blobs {
        active.insert(blob);
        if sess.wal_written_blobs.contains(&blob) {
            continue;
        }
        let mut loaded = {
            let mut store = image_store.lock().unwrap_or_else(|e| e.into_inner());
            store.get_blob(blob)
        };
        if loaded.is_none() && sess.machine.vt_mut().rehydrate_image_blob(blob) {
            let mut store = image_store.lock().unwrap_or_else(|e| e.into_inner());
            loaded = store.get_blob(blob);
        }
        if let Some((encoding, bytes)) = loaded
            && wal.write_blob(encoding, bytes)
        {
            sess.wal_written_blobs.insert(blob);
        } else {
            all_persisted = false;
        }
    }
    sess.wal_written_blobs.retain(|h| active.contains(h));
    if all_persisted {
        sess.wal_images_gen = cur_gen;
    }
}

/// Checks for pending WAL rollover and starts a fresh keyframe segment once the VT is at ground
/// and all active image blobs referenced by the keyframe have been persisted to `<dir>/blobs/`.
pub(super) fn maybe_rollover_wal(
    sess: &mut LiveSession,
    now_ms: u64,
    at_ground: bool,
    abi_hash: &[u8; 32],
    image_store: &Arc<Mutex<ImageStore>>,
    wal_reaper: &WalReaper,
) {
    if detach_failed_wal(sess, wal_reaper) {
        return;
    }
    if sess.wal.as_ref().is_some_and(WalHandle::take_rollover_needed) {
        sess.pending_wal_rollover = true;
    }
    if !at_ground || !sess.pending_wal_rollover {
        return;
    }
    if sess.wal_images_gen != sess.machine.vt().generations().images {
        sync_wal_image_blobs(sess, image_store, wal_reaper);
        if sess.wal_images_gen != sess.machine.vt().generations().images {
            return;
        }
    }
    let Some(wal) = &sess.wal else {
        return;
    };
    let kf = capture_keyframe(&sess.machine, abi_hash);
    if wal.start_segment(now_ms, kf) {
        sess.wal_has_segment = true;
        sess.pending_wal_rollover = false;
    }
}

/// Non-blockingly finalizes and detaches a session's [`WalHandle`] on exit/teardown:
/// 1. Syncs any pending image blobs.
/// 2. If a rollover keyframe (e.g. from a shed `> 16 MiB` `t=f`/`t=s` image or deferred initial
///    segment) is pending and all active blobs are persisted, commits one terminal keyframe via
///    `start_segment_on_close` so short-lived image commands are never lost.
/// 3. Seals the active segment iff a segment exists and all active blobs were persisted.
/// 4. Hands the worker thread to `wal_reaper` without blocking the shard thread.
pub(super) fn close_session_wal(
    sess: &mut LiveSession,
    abi_hash: &[u8; 32],
    image_store: &Arc<Mutex<ImageStore>>,
    wal_reaper: &WalReaper,
) {
    sync_wal_image_blobs(sess, image_store, wal_reaper);
    let blobs_synced = sess.wal_images_gen == sess.machine.vt().generations().images;
    if sess.pending_wal_rollover && blobs_synced {
        let now_ms = now_unix_ms();
        maybe_rollover_wal(sess, now_ms, true, abi_hash, image_store, wal_reaper);
        if sess.pending_wal_rollover
            && let Some(wal) = &sess.wal
        {
            let kf = capture_keyframe(&sess.machine, abi_hash);
            if wal.start_segment_on_close(now_ms, kf) {
                sess.wal_has_segment = true;
                sess.pending_wal_rollover = false;
            }
        }
    }
    if let Some(wal) = sess.wal.take() {
        if sess.wal_has_segment && sess.wal_images_gen == sess.machine.vt().generations().images {
            let _ = wal.seal();
        }
        wal_reaper.close_async(wal);
    }
    sess.wal_has_segment = false;
    sess.pending_wal_rollover = false;
}

/// Hard wire-byte budget for history rows extracted into a single WAL `VisualKeyframe` (`16 MiB`).
const MAX_KEYFRAME_HISTORY_BYTES: usize = 16 << 20;

/// Captures a dual-layer [`KeyframeV2`] (broker-owned [`VisualKeyframe`] + ABI-hashed Ghostty snapshot).
pub(crate) fn capture_keyframe(machine: &SessionMachine<GhosttyEngine>, abi_hash: &[u8; 32]) -> KeyframeV2 {
    let vt = machine.vt();
    let meta = vt.meta();
    let vp_indices: Vec<u16> = (0..meta.size.rows.get()).collect();
    let mut primary_rows = Vec::new();
    let mut alt_rows = Vec::new();
    if meta.modes.alt_screen {
        vt.extract_viewport_rows(&vp_indices, &mut alt_rows);
    } else {
        let hist_take = meta.scrollback_lines.min(MAX_KEYFRAME_HISTORY_ROWS as u64);
        if hist_take != 0 {
            let from_line = meta.first_active_line.saturating_sub(hist_take);
            vt.extract_history_rows(from_line, hist_take as u32, MAX_KEYFRAME_HISTORY_BYTES, &mut primary_rows);
        }
        vt.extract_viewport_rows(&vp_indices, &mut primary_rows);
    }

    let visual = VisualKeyframe {
        size: meta.size,
        cell_px: machine.current_cell_px(),
        active_alt_screen: meta.modes.alt_screen,
        cursor: meta.cursor,
        modes: meta.modes,
        palette: vt.palette_overrides(),
        primary_rows,
        alt_rows,
        images: vt.images(),
        marks_key: machine.marks_key(),
        marks_last_seq: machine.marks_last_seq(),
        marks_active_vouched_seq: machine.marks_active_vouched_seq(),
        marks_refused: machine.marks_refused(),
        commands: machine.commands(),
    };

    let engine = vt
        .export_snapshot(MAX_ENGINE_SNAPSHOT_BYTES)
        .ok()
        .map(|snapshot_bytes| EngineCheckpoint { engine_abi_hash: *abi_hash, snapshot_bytes });

    let mut kf = KeyframeV2 { visual, engine };
    kf.degrade_to_limits(MAX_KEYFRAME_UNCOMPRESSED, MAX_RECORD_PAYLOAD);
    kf
}

/// Loads every image blob referenced by `slice.keyframe` (direct placements AND virtual `U=1`
/// row pieces) from `<dir>/blobs/` into `image_store`.
pub(crate) fn hydrate_slice_blobs(reader: &WalReader, slice: &SeekSlice, image_store: &Arc<Mutex<ImageStore>>) {
    for blob in slice.keyframe.referenced_blobs() {
        let already = {
            let mut store = image_store.lock().unwrap_or_else(|e| e.into_inner());
            store.touch(blob)
        };
        if !already && let Ok(bytes) = reader.read_blob(blob, ImageEncoding::Png) {
            let mut store = image_store.lock().unwrap_or_else(|e| e.into_inner());
            let _ = store.insert_encoded(ImageEncoding::Png, bytes);
        }
    }
}

/// Reconstructs sanitized printable text for a [`PackedRow`], stripping all C0 (`< 0x20`, `0x7F`)
/// and C1 (`U+0080..=U+009F`) control characters, clamping column offsets to `cols`, skipping
/// overlapping/out-of-order runs, and bounding total scanned + emitted bytes by `max_bytes`.
pub(super) fn reconstruct_visual_row(row: &PackedRow, cols: u16, max_bytes: usize) -> (String, u16) {
    let mut budget_used = 0usize;
    if row.runs.is_empty() {
        let mut out = String::new();
        let mut emitted_chars = 0usize;
        for ch in row.text.chars() {
            budget_used = budget_used.saturating_add(ch.len_utf8());
            if budget_used > max_bytes {
                break;
            }
            if ch.is_control() {
                continue;
            }
            if emitted_chars >= usize::from(cols) {
                break;
            }
            out.push(ch);
            emitted_chars += 1;
        }
        return (out, 0);
    }
    let mut out = String::with_capacity(row.text.len().min(max_bytes));
    let mut cur_col = 0u16;
    let mut had_any_control = false;
    for run in &row.runs {
        if run.col_start >= cols || run.col_start < cur_col {
            continue;
        }
        let start = run.byte_offset as usize;
        let end = start.saturating_add(run.byte_len as usize);
        let Some(slice) = row.text.get(start..end) else {
            continue;
        };
        let pad = usize::from(run.col_start - cur_col);
        budget_used = budget_used.saturating_add(pad);
        if budget_used > max_bytes {
            break;
        }
        for _ in 0..pad {
            out.push(' ');
        }
        cur_col = run.col_start;
        let mut hit_budget = false;
        for ch in slice.chars() {
            budget_used = budget_used.saturating_add(ch.len_utf8());
            if budget_used > max_bytes {
                hit_budget = true;
                break;
            }
            if ch.is_control() {
                had_any_control = true;
                continue;
            }
            out.push(ch);
        }
        if hit_budget {
            break;
        }
        cur_col = cur_col.saturating_add(run.col_len).min(cols);
    }
    (out, if had_any_control { 0 } else { cur_col })
}

fn feed_primary_visual_rows(engine: &mut GhosttyEngine, rows: &[PackedRow], cols: u16) {
    let mut events = Vec::new();
    let mut prev_wrapped_full = false;
    for (idx, row) in rows.iter().enumerate() {
        if idx > 0 && !prev_wrapped_full {
            events.clear();
            engine.feed(b"\r\n", &mut events);
        }
        let (clean, end_col) = reconstruct_visual_row(row, cols, apack_proto::MAX_PACKED_ROW_TEXT_BYTES);
        if !clean.is_empty() {
            events.clear();
            engine.feed(clean.as_bytes(), &mut events);
        }
        prev_wrapped_full = (row.flags & PackedRow::FLAG_WRAPPED) != 0 && end_col >= cols;
    }
}

fn feed_alt_visual_rows(engine: &mut GhosttyEngine, rows: &[PackedRow], cols: u16, max_rows: u16) {
    let mut events = Vec::new();
    // Disable DECAWM auto-wrap so wide chars or crafted overlong runs on the bottom alt row
    // can never wrap and scroll earlier alt-screen rows off screen.
    engine.feed(b"\x1b[?7l", &mut events);
    for (idx, row) in rows.iter().take(usize::from(max_rows)).enumerate() {
        let (clean, _) = reconstruct_visual_row(row, cols, apack_proto::MAX_PACKED_ROW_TEXT_BYTES);
        if !clean.is_empty() {
            let pos = format!("\x1b[{};1H", idx + 1);
            events.clear();
            engine.feed(pos.as_bytes(), &mut events);
            events.clear();
            engine.feed(clean.as_bytes(), &mut events);
        }
    }
    events.clear();
    engine.feed(b"\x1b[?7h", &mut events);
}

/// Reconstructs visible primary/alt screen text and cursor position safely when the `libghostty-vt`
/// binary snapshot ABI hash is absent or incompatible.
pub(crate) fn apply_visual_fallback(engine: &mut GhosttyEngine, visual: &VisualKeyframe) {
    let mut events = Vec::new();
    engine.resize(visual.size, visual.cell_px, &mut events);
    feed_primary_visual_rows(engine, &visual.primary_rows, visual.size.cols.get());
    if visual.active_alt_screen {
        events.clear();
        engine.feed(b"\x1b[?1049h", &mut events);
        feed_alt_visual_rows(engine, &visual.alt_rows, visual.size.cols.get(), visual.size.rows.get());
    }
    let move_cursor = format!("\x1b[{};{}H", visual.cursor.row.saturating_add(1), visual.cursor.col.saturating_add(1));
    events.clear();
    engine.feed(move_cursor.as_bytes(), &mut events);
}

pub(crate) fn build_keyframe_engine(
    slice: &SeekSlice,
    abi_hash: &[u8; 32],
    image_store: &Arc<Mutex<ImageStore>>,
    scrollback_lines: usize,
) -> Result<GhosttyEngine, GhosttyError> {
    let mut vt_cfg = GhosttyConfig::new(slice.keyframe.visual.size);
    vt_cfg.cell_px = slice.keyframe.visual.cell_px;
    vt_cfg.scrollback_lines = scrollback_lines;
    vt_cfg.image_store = Some(Arc::clone(image_store));
    let mut engine = GhosttyEngine::new(vt_cfg)?;
    let restored_lossless = slice
        .keyframe
        .engine_snapshot_if_compatible(abi_hash)
        .is_some_and(|snap| engine.restore_snapshot(snap).is_ok());
    if !restored_lossless {
        apply_visual_fallback(&mut engine, &slice.keyframe.visual);
    }
    Ok(engine)
}

fn slice_to_replay_inputs(
    slice: &SeekSlice,
    trailing_resize: Option<(GridSize, CellPx)>,
) -> (RestoreKeyframe, Vec<ReplayRecord<'_>>) {
    let visual = &slice.keyframe.visual;
    let restore_kf = RestoreKeyframe {
        cell_px: visual.cell_px,
        marks_mode: visual.marks_key.map_or(MarksMode::Claimed, MarksMode::Keyed),
        marks_last_seq: visual.marks_last_seq,
        marks_active_vouched_seq: visual.marks_active_vouched_seq,
        marks_refused: visual.marks_refused,
        commands: visual.commands.clone(),
    };
    let mut tail: Vec<ReplayRecord<'_>> = slice
        .tail_records
        .iter()
        .filter_map(|timed| match &timed.record {
            WalRecord::Output(bytes) => Some(ReplayRecord::Output { at_ms: WallMs::from_millis(timed.at_ms), bytes }),
            WalRecord::Resize { size, cell_px } => Some(ReplayRecord::Resize { size: *size, cell_px: *cell_px }),
            WalRecord::Marks { key, last_seq } => Some(ReplayRecord::Marks { key: *key, last_seq: *last_seq }),
            WalRecord::Keyframe(_) | WalRecord::Gap { .. } => None,
        })
        .collect();
    if let Some((size, cell_px)) = trailing_resize {
        tail.push(ReplayRecord::Resize { size, cell_px });
    }
    (restore_kf, tail)
}

pub(super) fn build_machine_from_slice(
    session_id: SessionId,
    fresh_engine: GhosttyEngine,
    slice: &SeekSlice,
) -> SessionMachine<GhosttyEngine> {
    let (restore_kf, tail) = slice_to_replay_inputs(slice, None);
    SessionMachine::from_replay(session_id, fresh_engine, ResizePolicy::ExplicitOverride, restore_kf, &tail)
}

pub(crate) fn restore_machine_from_slice(
    machine: &mut SessionMachine<GhosttyEngine>,
    fresh_engine: GhosttyEngine,
    slice: &SeekSlice,
    trailing_resize: Option<(GridSize, CellPx)>,
    emit_status_events: bool,
    effects: &mut Vec<SessionEffect>,
) {
    let (restore_kf, tail) = slice_to_replay_inputs(slice, trailing_resize);
    machine.restore_from_replay(fresh_engine, restore_kf, &tail, emit_status_events, effects);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn restore_machine_from_dir(
    machine: &mut SessionMachine<GhosttyEngine>,
    dir: &Path,
    registry: &RecordingRegistry,
    abi_hash: &[u8; 32],
    image_store: &Arc<Mutex<ImageStore>>,
    scrollback_lines: usize,
    live_marks_key: Option<MarksKey>,
    target_size: GridSize,
    target_cell_px: CellPx,
) -> Result<(), apack_wal::WalError> {
    let reader = WalReader::open(dir, registry)?;
    let Some(slice) = reader.latest_segment_records()? else {
        return Ok(());
    };
    hydrate_slice_blobs(&reader, &slice, image_store);
    let fresh_engine = build_keyframe_engine(&slice, abi_hash, image_store, scrollback_lines)
        .map_err(|e| apack_wal::WalError::Io(std::io::Error::other(e.to_string())))?;
    let mut effects = Vec::new();
    restore_machine_from_slice(machine, fresh_engine, &slice, Some((target_size, target_cell_px)), false, &mut effects);
    let live_mode = live_marks_key.map_or(MarksMode::Claimed, MarksMode::Keyed);
    machine.prepare_for_live_after_restore(live_mode, now_wall_ms());
    Ok(())
}
