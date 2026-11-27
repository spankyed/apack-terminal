//! Session creation, rewind/seek, kill/signal/owns, stream attachment, and WAL keyframe helpers.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use apack_core::flow::OutboundPriority;
use apack_core::lease::ConnId;
use apack_core::marks::MarksMode;
use apack_core::session::SessionMachine;
use apack_core::vt::VtEngine;
use apack_os::image_sources::{Scanner as ImageScanner, Worker as ImageWorker};
use apack_os::pty::{PtySpawnConfig, spawn_pty};
#[cfg(unix)]
use apack_os::pty_poller::{Interest, PollToken};
#[cfg(windows)]
use apack_os::pty_windows::ConPtyBridge;
use apack_os::signal::deliver_signal;
use apack_proto::messages::{BrokerMsg, SpawnSpec};
use apack_proto::types::{
    ErrorCode, NonZeroStreamId, RequestId, ResizePolicy, SessionId, SignalKind, SignalScope, SignalTarget, StreamId,
    SubFlags, ViewportHint,
};
use apack_vt_ghostty::{GhosttyConfig, GhosttyEngine};
use apack_wal::{
    EngineCheckpoint, KeyframeV2, MAX_ENGINE_SNAPSHOT_BYTES, MAX_KEYFRAME_HISTORY_ROWS, RecordingRegistry,
    VisualKeyframe, WalReader, WalRecord, WalWorker, WalWriter,
};

use crate::config::AUTH_KEY_ENV;
use crate::{fill_random, now_unix_ms};

use super::{DEFAULT_SCROLLBACK_LINES, LivePty, LiveSession, ReactorEvent, ShardRunner};

impl ShardRunner {
    pub(super) fn handle_spawn(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId, spec: SpawnSpec) {
        if self.tearing_down_all {
            self.emit(ReactorEvent::SpawnOrRewindFailed);
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Busy, "broker is shutting down");
            return;
        }
        let scrollback_lines = resolve_scrollback_lines(spec.scrollback_limit);
        let mut vt_cfg = GhosttyConfig::new(spec.size);
        vt_cfg.cell_px = spec.cell_px;
        vt_cfg.scrollback_lines = scrollback_lines;
        vt_cfg.image_store = Some(Arc::clone(&self.shared.image_store));

        let mut engine = match GhosttyEngine::new(vt_cfg) {
            Ok(e) => e,
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
        };

        // Optional dual-layer restoration BEFORE spawning the live shell.
        if let Some(restore_dir) = &spec.restore_from {
            if let Err(err) = restore_engine_from_dir(
                &mut engine,
                Path::new(restore_dir),
                &self.shared.wal_registry,
                &self.shared.engine_abi_hash,
            ) {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("restore failed: {err}"));
                return;
            }
            // Align restored VT geometry with the live PTY dimensions requested by the caller.
            if engine.meta().size != spec.size {
                let mut evs = Vec::new();
                engine.resize(spec.size, spec.cell_px, &mut evs);
            }
        }

        // Optional segmented WAL recording setup.
        let created_ms = now_unix_ms();
        let wal = if let Some(record_dir) = &spec.record_dir {
            let mut uuid = [0u8; 16];
            if let Err(err) = fill_random(&mut uuid) {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err);
                return;
            }
            match WalWriter::open_or_create(
                record_dir,
                uuid,
                created_ms,
                self.shared.wal_config,
                self.shared.wal_registry.clone(),
            ) {
                Ok(writer) => Some(WalWorker::spawn(writer)),
                Err(err) => {
                    self.emit(ReactorEvent::SpawnOrRewindFailed);
                    self.send_error(
                        conn,
                        StreamId::CONTROL,
                        req_id,
                        ErrorCode::Pty,
                        &format!("recording failed: {err}"),
                    );
                    return;
                }
            }
        } else {
            None
        };

        // Spawn OS PTY process. Never leak `AUTH_KEY_ENV` or `spec.marks_key` into child env!
        let (shell, args) = match spec.argv.split_first() {
            Some((prog, rest)) => (Some(prog.clone()), rest.to_vec()),
            None => (None, Vec::new()),
        };
        let pty_cfg = PtySpawnConfig {
            size: spec.size,
            cell_px: spec.cell_px,
            shell,
            args,
            env: spec.env,
            unset: vec![String::from(AUTH_KEY_ENV)],
            cwd: spec.cwd,
        };
        let mut spawned = match spawn_pty(&pty_cfg) {
            Ok(s) => s,
            Err(err) => {
                if let Some(w) = wal {
                    let _ = w.shutdown();
                }
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, err.error_code(), &err.to_string());
                return;
            }
        };

        let marks_mode = match spec.marks_key {
            Some(key) => MarksMode::Keyed(key),
            None => MarksMode::Claimed,
        };
        let machine = SessionMachine::new(session_id, engine, spec.resize_policy, marks_mode, spec.cell_px);

        // Capture initial KeyframeV2 as record 0 if recording is enabled.
        if let Some(handle) = &wal {
            let kf = capture_keyframe(&machine, &self.shared.engine_abi_hash);
            let _ = handle.start_segment(created_ms, kf);
        }

        // Register PTY with shard multiplexer; kill & reap child if registration fails.
        #[cfg(unix)]
        let live_pty = {
            let token = PollToken(session_id.get());
            if let Err(err) = self.poller.register(token, spawned.master.raw_fd(), Interest::READ) {
                if let Some(w) = wal {
                    let _ = w.shutdown();
                }
                let _ = deliver_signal(spawned.pid, SignalKind::Kill, SignalScope::Group);
                let _ = spawned.killer.kill();
                let _ = spawned.child.wait();
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
            self.token_to_session.insert(token, session_id);
            LivePty::Unix { master: spawned.master, token, eof: false, write_broken: false }
        };

        #[cfg(windows)]
        let live_pty = {
            let wake = self.self_wake.clone();
            match ConPtyBridge::start(spawned.master, move || wake.wake()) {
                Ok(bridge) => LivePty::Windows { bridge, eof: false },
                Err(err) => {
                    if let Some(w) = wal {
                        let _ = w.shutdown();
                    }
                    drop(spawned.tree);
                    let _ = spawned.killer.kill();
                    let _ = spawned.child.wait();
                    self.emit(ReactorEvent::SpawnOrRewindFailed);
                    self.send_error(conn, StreamId::CONTROL, req_id, err.error_code(), &err.to_string());
                    return;
                }
            }
        };

        let pid_target = i32::try_from(spawned.pid).ok().and_then(SignalTarget::new);
        let kept = spec.kept;
        let (image_scanner, image_worker) = if spec.image_files {
            (Some(ImageScanner::default()), Some(Arc::new(ImageWorker::default())))
        } else {
            (None, None)
        };
        self.sessions.insert(
            session_id,
            LiveSession {
                machine,
                pty: Some(live_pty),
                child: Some(spawned.child),
                killer: Some(spawned.killer),
                tree: spawned.tree,
                pid: pid_target,
                kept,
                rewound: false,
                created_ms,
                wal,
                pending_wal_rollover: false,
                rewind_reader: None,
                rewind_at_ms: 0,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: false,
                paused_by_raw_flow: false,
                terminating_since: None,
                escalated_sigkill: false,
                view_slot_free: HashSet::new(),
                raw_output_subs: HashSet::new(),
                image_scanner,
                image_worker,
                image_ticket: 0,
                pending_image: None,
                pending_image_tail: Vec::new(),
                reaped_exit: None,
            },
        );
        self.load.store(self.sessions.len(), Ordering::Relaxed);

        self.emit(ReactorEvent::SessionRegistered {
            session: session_id,
            shard_idx: self.shard_idx,
            rewound: false,
            kept,
        });
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Spawned { session: session_id, pid: pid_target },
        });
    }

    pub(super) fn handle_rewind(
        &mut self,
        session_id: SessionId,
        conn: ConnId,
        req_id: RequestId,
        record_dir: &str,
        at_ms: Option<u64>,
    ) {
        if self.tearing_down_all {
            self.emit(ReactorEvent::SpawnOrRewindFailed);
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Busy, "broker is shutting down");
            return;
        }
        let reader = match WalReader::open(record_dir, &self.shared.wal_registry) {
            Ok(r) => r,
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("rewind failed: {err}"));
                return;
            }
        };
        let Some((first_ms, last_ms)) = reader.span() else {
            self.emit(ReactorEvent::SpawnOrRewindFailed);
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "recording contains no segments");
            return;
        };
        let target_ms = at_ms.map_or(last_ms, |t| first_ms.saturating_add(t).min(last_ms));
        let duration_ms = last_ms.saturating_sub(first_ms);

        let slice = match reader.records_for_moment(target_ms) {
            Ok(Some(s)) => s,
            Ok(None) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "recording has no seek slice");
                return;
            }
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("rewind read failed: {err}"));
                return;
            }
        };

        let mut vt_cfg = GhosttyConfig::new(slice.keyframe.visual.size);
        vt_cfg.cell_px = slice.keyframe.visual.cell_px;
        vt_cfg.image_store = Some(Arc::clone(&self.shared.image_store));
        let mut engine = match GhosttyEngine::new(vt_cfg) {
            Ok(e) => e,
            Err(err) => {
                self.emit(ReactorEvent::SpawnOrRewindFailed);
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
        };

        let effective_cell_px = apply_seek_slice(&mut engine, &slice, &self.shared.engine_abi_hash);
        let machine = SessionMachine::new(
            session_id,
            engine,
            ResizePolicy::ExplicitOverride,
            MarksMode::Claimed,
            effective_cell_px,
        );
        let actual_offset_ms = target_ms.saturating_sub(first_ms);

        self.sessions.insert(
            session_id,
            LiveSession {
                machine,
                pty: None,
                child: None,
                killer: None,
                tree: None,
                pid: None,
                kept: false,
                rewound: true,
                created_ms: reader.meta().created_ms,
                wal: None,
                pending_wal_rollover: false,
                rewind_reader: Some(reader),
                rewind_at_ms: actual_offset_ms,
                last_activity: Instant::now(),
                compressed_idle: false,
                paused_by_stream: false,
                paused_by_raw_flow: false,
                terminating_since: None,
                escalated_sigkill: false,
                view_slot_free: HashSet::new(),
                raw_output_subs: HashSet::new(),
                image_scanner: None,
                image_worker: None,
                image_ticket: 0,
                pending_image: None,
                pending_image_tail: Vec::new(),
                reaped_exit: None,
            },
        );
        self.load.store(self.sessions.len(), Ordering::Relaxed);

        self.emit(ReactorEvent::SessionRegistered {
            session: session_id,
            shard_idx: self.shard_idx,
            rewound: true,
            kept: false,
        });
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Rewound { session: session_id, duration_ms, at_ms: actual_offset_ms },
        });
    }

    pub(super) fn handle_seek(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId, at_ms: u64) {
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            self.send_no_such_session(conn, req_id);
            return;
        };
        let Some(reader) = &sess.rewind_reader else {
            self.send_error(
                conn,
                StreamId::CONTROL,
                req_id,
                ErrorCode::Malformed,
                "session is not a rewound recording",
            );
            return;
        };
        let Some((first_ms, last_ms)) = reader.span() else {
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "recording is empty");
            return;
        };
        let target_ms = first_ms.saturating_add(at_ms).min(last_ms);
        let slice = match reader.records_for_moment(target_ms) {
            Ok(Some(s)) => s,
            Ok(None) => {
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "seek target missing");
                return;
            }
            Err(err) => {
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &format!("seek failed: {err}"));
                return;
            }
        };

        // Construct a clean VT engine on seek so ABI-mismatch fallback never appends onto old seek rows!
        let mut vt_cfg = GhosttyConfig::new(slice.keyframe.visual.size);
        vt_cfg.cell_px = slice.keyframe.visual.cell_px;
        vt_cfg.image_store = Some(Arc::clone(&self.shared.image_store));
        let mut fresh_engine = match GhosttyEngine::new(vt_cfg) {
            Ok(e) => e,
            Err(err) => {
                self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                return;
            }
        };
        let effective_cell_px = apply_seek_slice(&mut fresh_engine, &slice, &self.shared.engine_abi_hash);
        *sess.machine.vt_mut() = fresh_engine;
        sess.machine.sync_vt_geometry(effective_cell_px);
        sess.machine.mark_all_views_full_sync();
        let actual_offset_ms = target_ms.saturating_sub(first_ms);
        sess.rewind_at_ms = actual_offset_ms;

        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::Sought { at_ms: actual_offset_ms },
        });
        self.try_publish_dirty_views(session_id);
    }

    pub(super) fn handle_kill(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId) {
        if !self.sessions.contains_key(&session_id) {
            self.send_no_such_session(conn, req_id);
            return;
        }
        self.initiate_session_termination(session_id);
        self.send_ok(conn, StreamId::CONTROL, req_id);
    }

    pub(super) fn handle_signal(
        &mut self,
        session_id: SessionId,
        conn: ConnId,
        req_id: RequestId,
        signal: SignalKind,
        scope: SignalScope,
    ) {
        let Some(sess) = self.sessions.get(&session_id) else {
            self.send_no_such_session(conn, req_id);
            return;
        };
        let Some(pid) = sess.pid else {
            self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Malformed, "session has no live process");
            return;
        };
        match deliver_signal(pid.pid() as u32, signal, scope) {
            Ok(()) => self.send_ok(conn, StreamId::CONTROL, req_id),
            Err(err) => self.send_error(conn, StreamId::CONTROL, req_id, err.error_code(), &err.to_string()),
        }
    }

    pub(super) fn handle_owns(&mut self, session_id: SessionId, conn: ConnId, req_id: RequestId, pid: SignalTarget) {
        let Some(sess) = self.sessions.get(&session_id) else {
            self.send_no_such_session(conn, req_id);
            return;
        };
        let owned = match &sess.tree {
            Some(tree) => match tree.owns(pid.pid() as u32) {
                Ok(v) => v,
                Err(err) => {
                    self.send_error(conn, StreamId::CONTROL, req_id, ErrorCode::Pty, &err.to_string());
                    return;
                }
            },
            None => false,
        };
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: BrokerMsg::OwnsReply { owned },
        });
    }

    pub(super) fn handle_attach(
        &mut self,
        session_id: SessionId,
        conn: ConnId,
        req_id: RequestId,
        stream_id: NonZeroStreamId,
        flags: SubFlags,
        initial_viewport: Option<ViewportHint>,
    ) {
        let mut effects = Vec::new();
        let reply = {
            let Some(sess) = self.sessions.get_mut(&session_id) else {
                self.send_no_such_session(conn, req_id);
                return;
            };
            sess.machine.attach_stream((conn, stream_id), flags, initial_viewport, &mut effects);
            if flags.contains(SubFlags::VIEW) {
                sess.view_slot_free.insert((conn, stream_id));
            }
            if flags.contains(SubFlags::RAW_OUTPUT) {
                sess.raw_output_subs.insert((conn, stream_id));
            }
            let meta = sess.machine.vt().meta();
            let foreground = match &sess.pty {
                #[cfg(unix)]
                Some(LivePty::Unix { master, .. }) => master.foreground_process(),
                _ => None,
            };
            BrokerMsg::Attached {
                session: session_id,
                stream_id,
                size: meta.size,
                epoch: meta.epoch,
                first_active_line: meta.first_active_line,
                scrollback_lines: meta.scrollback_lines,
                foreground,
            }
        };
        self.emit(ReactorEvent::SendMsg {
            conn,
            stream_id: StreamId::CONTROL,
            req_id,
            priority: OutboundPriority::Control,
            msg: reply,
        });
        self.apply_effects(session_id, effects);
    }
}

/// Resolves `SpawnSpec::scrollback_limit` (`0` selects [`DEFAULT_SCROLLBACK_LINES`]).
pub(crate) fn resolve_scrollback_lines(limit: u32) -> usize {
    if limit == 0 { DEFAULT_SCROLLBACK_LINES } else { limit as usize }
}

/// Captures a dual-layer [`KeyframeV2`] (broker-owned [`VisualKeyframe`] + ABI-hashed Ghostty snapshot).
pub(crate) fn capture_keyframe(machine: &SessionMachine<GhosttyEngine>, abi_hash: &[u8; 32]) -> KeyframeV2 {
    let vt = machine.vt();
    let meta = vt.meta();
    let mut primary_rows = Vec::new();
    let hist_take = meta.scrollback_lines.min(MAX_KEYFRAME_HISTORY_ROWS as u64);
    if hist_take != 0 {
        let from_line = meta.first_active_line.saturating_sub(hist_take);
        vt.extract_history_rows(from_line, hist_take as u32, &mut primary_rows);
    }
    let vp_indices: Vec<u16> = (0..meta.size.rows.get()).collect();
    vt.extract_viewport_rows(&vp_indices, &mut primary_rows);

    let visual = VisualKeyframe {
        size: meta.size,
        cell_px: machine.current_cell_px(),
        active_alt_screen: meta.modes.alt_screen,
        cursor: meta.cursor,
        modes: meta.modes,
        palette: vt.palette_overrides(),
        primary_rows,
        alt_rows: Vec::new(),
        images: vt.images(),
        marks_key: machine.marks_key(),
        marks_last_seq: machine.marks_last_seq(),
        marks_refused: machine.marks_refused(),
        commands: machine.commands(),
    };

    let engine = vt
        .export_snapshot(MAX_ENGINE_SNAPSHOT_BYTES)
        .ok()
        .map(|snapshot_bytes| EngineCheckpoint { engine_abi_hash: *abi_hash, snapshot_bytes });

    KeyframeV2 { visual, engine }
}

fn restore_engine_from_dir(
    engine: &mut GhosttyEngine,
    dir: &Path,
    registry: &RecordingRegistry,
    abi_hash: &[u8; 32],
) -> Result<(), apack_wal::WalError> {
    let reader = WalReader::open(dir, registry)?;
    let Some(slice) = reader.latest_segment_records()? else {
        return Ok(());
    };
    apply_seek_slice(engine, &slice, abi_hash);
    Ok(())
}

pub(crate) fn apply_seek_slice(
    engine: &mut GhosttyEngine,
    slice: &apack_wal::SeekSlice,
    abi_hash: &[u8; 32],
) -> apack_proto::types::CellPx {
    let mut events = Vec::new();
    let mut restored_lossless = false;
    let mut effective_cell_px = slice.keyframe.visual.cell_px;
    if let Some(snap) = slice.keyframe.engine_snapshot_if_compatible(abi_hash)
        && engine.restore_snapshot(snap).is_ok()
    {
        restored_lossless = true;
    }
    if !restored_lossless {
        // Visual fallback when `libghostty-vt` ABI hash changed across an upgrade:
        // resize to visual geometry and reconstruct visible text lines cleanly.
        engine.resize(slice.keyframe.visual.size, slice.keyframe.visual.cell_px, &mut events);
        for (idx, row) in slice.keyframe.visual.primary_rows.iter().enumerate() {
            if idx > 0 {
                engine.feed(b"\r\n", &mut events);
            }
            if !row.text.is_empty() {
                engine.feed(row.text.as_bytes(), &mut events);
            }
        }
    }
    for timed in &slice.tail_records {
        events.clear();
        match &timed.record {
            WalRecord::Output(bytes) => engine.feed(bytes, &mut events),
            WalRecord::Resize { size, cell_px } => {
                effective_cell_px = *cell_px;
                engine.resize(*size, *cell_px, &mut events);
            }
            WalRecord::Keyframe(_) | WalRecord::Marks { .. } | WalRecord::Gap { .. } => {}
        }
    }
    effective_cell_px
}
