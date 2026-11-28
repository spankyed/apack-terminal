//! Session effect dispatch, PTY read/write pumps, and Unix poller interest management.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use apack_core::flow::OutboundPriority;
use apack_core::session::{SessionEffect, StreamAttachmentId, StreamKey};
use apack_core::time::WallMs;
use apack_core::vt::VtEngine;
use apack_os::image_sources::Load;
#[cfg(unix)]
use apack_os::pty::{NonBlockingRead, PtyMaster};
#[cfg(unix)]
use apack_os::pty_poller::{Interest, PollEvent};
use apack_proto::messages::{BrokerMsg, FrameUpdate};
use apack_proto::types::{ErrorCode, SessionId};

#[cfg(windows)]
use crate::shard_windows::drain_windows_pty_writes;
use crate::{now_monotonic_ms, now_unix_ms};

use super::images::{ImageRefusal, probe_named_image_load};
use super::protocol::{FgVerifyReq, ShardCmdPermit};
use super::{LivePty, ReactorEvent, ShardRunner};
#[cfg(unix)]
use super::{LiveSession, PTY_READ_QUANTUM_BYTES, PTY_WRITE_CHUNK_BYTES};

impl ShardRunner {
    pub(super) fn apply_effects(&mut self, session_id: SessionId, effects: Vec<SessionEffect>) {
        self.apply_effects_inner(session_id, effects, true, None);
    }

    pub(super) fn apply_effects_with_permit(
        &mut self,
        session_id: SessionId,
        effects: Vec<SessionEffect>,
        permit: Option<Arc<ShardCmdPermit>>,
    ) {
        self.apply_effects_inner(session_id, effects, true, permit);
    }

    pub(super) fn apply_effects_inner(
        &mut self,
        session_id: SessionId,
        effects: Vec<SessionEffect>,
        sync_and_reap: bool,
        permit: Option<Arc<ShardCmdPermit>>,
    ) {
        let mut views_dirty = false;
        let mut write_ready = false;
        let mut fg_dirty = false;

        for effect in effects {
            match effect {
                SessionEffect::Send { conn, stream_id, attachment, req_id, priority, msg } => {
                    if let (BrokerMsg::Exited { .. }, true) = (&msg, views_dirty) {
                        views_dirty = false;
                        self.try_publish_dirty_views(session_id);
                    }
                    self.emit(ReactorEvent::SendMsg {
                        conn,
                        stream_id,
                        attachment,
                        req_id,
                        priority,
                        msg,
                        permit: permit.clone(),
                    });
                }
                SessionEffect::ResizePty { size, cell_px } => {
                    if let Some(sess) = self.sessions.get_mut(&session_id) {
                        match &sess.pty {
                            #[cfg(unix)]
                            Some(LivePty::Unix { master, .. }) => {
                                let _ = master.resize(size, cell_px);
                            }
                            #[cfg(windows)]
                            Some(LivePty::Windows { bridge, .. }) => {
                                let _ = bridge.resize(size, cell_px);
                            }
                            None => {}
                        }
                        if !super::replay::detach_failed_wal(sess, &self.shared.wal_reaper)
                            && let Some(wal) = &sess.wal
                        {
                            let _ = wal.write_resize(now_unix_ms(), size, cell_px);
                        }
                    }
                }
                SessionEffect::PtyWriteReady => {
                    write_ready = true;
                }
                SessionEffect::ViewsDirty => {
                    views_dirty = true;
                }
                SessionEffect::VerifyForeground { id, changed, app } => {
                    self.fg_pending.entry(session_id).or_default().insert(id, (changed, app));
                    fg_dirty = true;
                }
                SessionEffect::FetchImageBlob { conn, stream_id, attachment, req_id, blob, offset, max_bytes } => {
                    let mut chunk_opt = {
                        let mut store = self.shared.image_store.lock().unwrap_or_else(|e| e.into_inner());
                        store.get_chunk(blob, offset, max_bytes)
                    };
                    if chunk_opt.is_none()
                        && let Some(sess) = self.sessions.get_mut(&session_id)
                        && sess.machine.vt_mut().rehydrate_image_blob(blob)
                    {
                        let mut store = self.shared.image_store.lock().unwrap_or_else(|e| e.into_inner());
                        chunk_opt = store.get_chunk(blob, offset, max_bytes);
                    }
                    let msg = match chunk_opt {
                        Some(chunk) => BrokerMsg::ImageBlobChunk {
                            blob: chunk.blob,
                            encoding: chunk.encoding,
                            total_bytes: chunk.total_bytes,
                            offset: chunk.offset,
                            data: chunk.data,
                        },
                        None => BrokerMsg::Error {
                            code: ErrorCode::ResourceLimit,
                            message: String::from("image blob evicted or unknown"),
                        },
                    };
                    self.emit(ReactorEvent::SendMsg {
                        conn,
                        stream_id: stream_id.as_stream_id(),
                        attachment: Some(attachment),
                        req_id,
                        priority: OutboundPriority::Bulk,
                        msg,
                        permit: permit.clone(),
                    });
                }
                SessionEffect::SetPtyPaused(paused) => {
                    if let Some(sess) = self.sessions.get_mut(&session_id) {
                        sess.paused_by_stream = paused;
                        self.sync_pty_interest(session_id);
                    }
                }
            }
        }

        if write_ready {
            self.drain_pty_writes_for(session_id, sync_and_reap);
        }
        if views_dirty {
            self.try_publish_dirty_views(session_id);
        }
        if fg_dirty {
            self.flush_pending_fg_verify(session_id);
        }
    }

    pub(super) fn flush_all_pending_fg_verify(&mut self) {
        let pending_ids: Vec<SessionId> = self.fg_pending.keys().copied().collect();
        for sid in pending_ids {
            self.flush_pending_fg_verify(sid);
        }
    }

    pub(super) fn flush_pending_fg_verify(&mut self, session_id: SessionId) {
        if self.fg_inflight.contains(&session_id) {
            return;
        }
        let Some(items_map) = self.fg_pending.remove(&session_id) else {
            return;
        };
        let Some(sess) = self.sessions.get(&session_id) else {
            return;
        };
        let pgid = match sess.pty.as_ref() {
            #[cfg(unix)]
            Some(LivePty::Unix { master, .. }) => master.foreground_pgid(),
            _ => None,
        };
        // Dispatch all pending status verifications for `session_id` as one off-shard batch (one
        // `argv_for_pid` lookup per session, zero hot-shard sysinfo blocking, and zero dropped slots).
        if let Some(tx) = &self.fg_tx {
            let items: Vec<(String, u64, String)> =
                items_map.iter().map(|(id, (changed, app))| (id.clone(), *changed, app.clone())).collect();
            match tx.try_send(FgVerifyReq { session_id, pgid, items }) {
                Ok(()) => {
                    self.fg_inflight.insert(session_id);
                    return;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    // Retain in `fg_pending` and retry as soon as any in-flight verification completes;
                    // never degrade a spoofed `app=` to `Confirmed::Unknown` under transient backpressure.
                    self.fg_pending.insert(session_id, items_map);
                    return;
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
            }
        }
        // Fallback ONLY when `fg_tx` is absent or disconnected: resolve as `Confirmed::Unknown`
        // so `pending_verify` cannot deadlock a `WaitCondition::Status` waiter forever.
        let results: Vec<(String, u64, apack_proto::types::Confirmed)> = items_map
            .into_iter()
            .map(|(id, (changed, _))| (id, changed, apack_proto::types::Confirmed::Unknown))
            .collect();
        {
            let mut state = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
            state
                .queue
                .push_back((super::ShardCmd::ForegroundVerified { session_id, results, from_worker: false }, None));
            state.woken = true;
        }
        self.self_wake.wake();
    }

    pub(super) fn try_publish_dirty_views(&mut self, session_id: SessionId) {
        let (force_final, targets): (bool, Vec<(StreamKey, StreamAttachmentId)>) = match self.sessions.get(&session_id)
        {
            Some(sess) => {
                let force_final = !sess.machine.is_alive();
                let list = sess
                    .machine
                    .view_subscribers()
                    .into_iter()
                    .filter(|(_, att)| sess.view_slot_free.contains(att) || force_final)
                    .collect();
                (force_final, list)
            }
            None => return,
        };
        for (key, attachment_id) in targets {
            let mut emitted_frame: Option<Box<FrameUpdate>> = None;
            let published = match self.sessions.get_mut(&session_id) {
                Some(sess) => {
                    if force_final {
                        // Force a self-contained `full_sync` when publishing a final exit frame so
                        // replacing any unsent/stalled P1 frame never leaves an orphaned delta.
                        sess.machine.mark_view_full_sync(key);
                    }
                    sess.machine.try_publish_view(key, |frame| {
                        emitted_frame = Some(frame);
                        true
                    })
                }
                None => return,
            };
            if published && let Some(frame) = emitted_frame {
                if let Some(sess) = self.sessions.get_mut(&session_id) {
                    sess.view_slot_free.remove(&attachment_id);
                }
                self.emit(ReactorEvent::PublishFrame {
                    conn: key.0,
                    stream_id: key.1,
                    attachment_id,
                    session: session_id,
                    frame,
                    final_frame: force_final,
                });
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn handle_unix_poll_events(&mut self, events: &[PollEvent]) {
        for ev in events {
            let Some(&session_id) = self.token_to_session.get(&ev.token) else {
                continue;
            };
            if ev.writable {
                self.drain_pty_writes_for(session_id, true);
            }
            if ev.readable || ev.hangup {
                self.read_unix_pty_quantum(session_id);
            }
            if ev.hangup {
                self.try_reap_session(session_id);
            }
        }
    }

    #[cfg(unix)]
    pub(super) fn read_unix_pty_quantum(&mut self, session_id: SessionId) {
        let mut budget = PTY_READ_QUANTUM_BYTES;
        let mut buf = std::mem::take(&mut self.read_buf);
        while budget != 0 {
            let is_paused = self.sessions.get(&session_id).is_none_or(LiveSession::is_pty_paused);
            if is_paused {
                break;
            }
            let read_res = match self.sessions.get(&session_id).and_then(|s| s.pty.as_ref()) {
                Some(LivePty::Unix { master, eof: false, .. }) => {
                    let cap = budget.min(buf.len());
                    master.read_nonblocking(&mut buf[..cap])
                }
                _ => break,
            };
            match read_res {
                Ok(NonBlockingRead::Bytes(n)) => {
                    budget = budget.saturating_sub(n);
                    self.ingest_pty_bytes(session_id, &buf[..n]);
                }
                Ok(NonBlockingRead::WouldBlock) => break,
                Ok(NonBlockingRead::Eof) | Err(_) => {
                    if let Some(sess) = self.sessions.get_mut(&session_id)
                        && let Some(LivePty::Unix { token, eof, .. }) = sess.pty.as_mut()
                    {
                        *eof = true;
                        let tok = *token;
                        let _ = self.poller.deregister(tok);
                        self.token_to_session.remove(&tok);
                    }
                    self.read_buf = buf;
                    self.try_reap_session(session_id);
                    return;
                }
            }
        }
        self.read_buf = buf;
    }

    pub(crate) fn ingest_pty_bytes(&mut self, session_id: SessionId, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let now_ms = now_unix_ms();
        let wall = WallMs::from_millis(now_ms);
        let now = now_monotonic_ms();
        let mut effects = Vec::new();
        let mut has_raw_subs = false;
        let mut consumed = bytes.len();
        let mut dispatch: Option<(Load, ImageRefusal, Vec<u8>)> = None;

        if let Some(sess) = self.sessions.get_mut(&session_id) {
            if sess.pending_image.is_some() {
                sess.pending_image_tail.extend_from_slice(bytes);
                return;
            }
            sess.last_activity = Instant::now();
            sess.compressed_idle = false;
            has_raw_subs = !sess.raw_output_subs.is_empty();

            let prev_marks_seq = sess.machine.marks_last_seq();
            let mut wal_scanner_bytes: Option<Vec<u8>> = None;
            if let Some(mut scanner) = sess.image_scanner.take() {
                let mut rest = bytes;
                consumed = 0;
                let mut fed_bytes = if sess.wal.is_some() { Vec::with_capacity(bytes.len()) } else { Vec::new() };
                while !rest.is_empty() {
                    if !scanner.reading() {
                        let (n, entered_apc) = sess
                            .machine
                            .on_vt_feed_with(now, wall, &mut effects, |vt, evs| vt.feed_until_apc(rest, evs));
                        debug_assert!(n != 0 || entered_apc);
                        if sess.wal.is_some() {
                            fed_bytes.extend_from_slice(&rest[..n]);
                        }
                        rest = &rest[n..];
                        consumed = consumed.saturating_add(n);
                        if entered_apc {
                            scanner.begin();
                        }
                    } else {
                        let mut passthrough = Vec::new();
                        let (n, maybe_load) = scanner.read(rest, |b| passthrough.extend_from_slice(b));
                        debug_assert!(n != 0 || !scanner.reading());
                        rest = &rest[n..];
                        consumed = consumed.saturating_add(n);
                        if !passthrough.is_empty() {
                            if sess.wal.is_some() {
                                fed_bytes.extend_from_slice(&passthrough);
                            }
                            sess.machine.on_vt_feed_with(now, wall, &mut effects, |vt, evs| vt.feed(&passthrough, evs));
                        }
                        if let Some(load) = maybe_load
                            && let Some(refusal) = probe_named_image_load(&mut sess.machine, &load, &mut effects)
                        {
                            dispatch = Some((load, refusal, rest.to_vec()));
                            break;
                        }
                    }
                }
                sess.image_scanner = Some(scanner);
                if sess.wal.is_some() {
                    wal_scanner_bytes = Some(fed_bytes);
                }
            } else {
                sess.machine.on_pty_output(bytes, now, wall, &mut effects);
            }

            let committed = &bytes[..consumed];
            if !committed.is_empty() {
                let marks_seq = sess.machine.marks_last_seq();
                let marks_key = sess.machine.marks_key();
                let at_ground = sess.machine.vt().at_ground();
                super::replay::sync_wal_image_blobs(sess, &self.shared.image_store, &self.shared.wal_reaper);
                if let Some(wal) = &sess.wal {
                    let wal_chunk = wal_scanner_bytes.unwrap_or_else(|| committed.to_vec());
                    if !wal_chunk.is_empty() {
                        let _ = wal.write_output(now_ms, wal_chunk, at_ground);
                    }
                    if marks_seq != prev_marks_seq {
                        let _ = wal.write_marks(now_ms, marks_key, marks_seq);
                    }
                }
                super::replay::maybe_rollover_wal(
                    sess,
                    now_ms,
                    at_ground,
                    &self.shared.engine_abi_hash,
                    &self.shared.image_store,
                    &self.shared.wal_reaper,
                );
            }
        }

        let committed = &bytes[..consumed];
        self.total_pty_bytes = self.total_pty_bytes.saturating_add(committed.len() as u64);
        if has_raw_subs && !committed.is_empty() {
            let mut raw_event = None;
            let mut need_sync_interest = false;
            if let Some(sess) = self.sessions.get_mut(&session_id) {
                let len = committed.len();
                let sess_after = sess.raw_output_inflight.fetch_add(len, Ordering::AcqRel).saturating_add(len);
                let shard_after = self.raw_output_inflight.fetch_add(len, Ordering::AcqRel).saturating_add(len);
                if sess.terminating_since.is_none()
                    && super::protocol::should_pause_raw_inflight(sess_after, shard_after)
                {
                    sess.paused_by_raw_inflight = true;
                    need_sync_interest = true;
                }
                raw_event = Some(ReactorEvent::PublishRawOutput {
                    session: session_id,
                    bytes: committed.to_vec(),
                    session_inflight: Arc::clone(&sess.raw_output_inflight),
                    shard_inflight: Arc::clone(&self.raw_output_inflight),
                });
            }
            if need_sync_interest {
                self.sync_pty_interest(session_id);
            }
            if let Some(ev) = raw_event {
                self.emit(ev);
            }
        }
        if let Some((load, refusal, tail)) = dispatch {
            self.dispatch_image_load(session_id, load, refusal, &tail);
        }
        self.apply_effects(session_id, effects);
    }

    pub(super) fn drain_pty_writes_for(&mut self, session_id: SessionId, sync_and_reap: bool) {
        let Some(sess) = self.sessions.get_mut(&session_id) else {
            return;
        };
        #[cfg(unix)]
        {
            let mut hit_write_err = false;
            while sess.machine.pty_write_len() != 0 {
                sess.machine.peek_pty_write(PTY_WRITE_CHUNK_BYTES, &mut self.write_scratch);
                let written = match &sess.pty {
                    Some(LivePty::Unix { master, eof: false, write_broken: false, .. }) => {
                        master.write_nonblocking(&self.write_scratch)
                    }
                    _ => break,
                };
                match written {
                    Ok(0) => break, // EAGAIN -> arm writable interest below
                    Ok(n) => sess.machine.consume_pty_write(n),
                    Err(_) => {
                        if let Some(LivePty::Unix { write_broken, .. }) = sess.pty.as_mut() {
                            *write_broken = true;
                        }
                        hit_write_err = true;
                        break;
                    }
                }
            }
            if sync_and_reap {
                self.sync_pty_interest(session_id);
                if hit_write_err {
                    self.try_reap_session(session_id);
                }
            }
        }
        #[cfg(windows)]
        {
            let _ = sync_and_reap;
            drain_windows_pty_writes(sess);
        }
    }

    pub(super) fn sync_pty_interest(&mut self, session_id: SessionId) {
        #[cfg(unix)]
        {
            let Some(sess) = self.sessions.get(&session_id) else {
                return;
            };
            if let Some(LivePty::Unix { token, eof: false, write_broken, .. }) = &sess.pty {
                let interest = unix_pty_interest(sess.is_pty_paused(), *write_broken, sess.machine.pty_write_len());
                let _ = self.poller.modify(*token, interest);
            }
        }
        #[cfg(windows)]
        {
            if let Some(sess) = self.sessions.get(&session_id)
                && let Some(LivePty::Windows { bridge, .. }) = &sess.pty
            {
                bridge.set_paused(sess.is_pty_paused());
            }
        }
    }
}

/// Drains up to `budget` bytes of immediately available non-blocking Unix PTY output.
#[cfg(unix)]
pub(crate) fn drain_unix_pty_nonblocking(
    master: &PtyMaster,
    buf: &mut [u8],
    mut budget: usize,
    mut on_bytes: impl FnMut(&[u8]),
) {
    if buf.is_empty() {
        return;
    }
    while budget != 0 {
        let cap = budget.min(buf.len());
        match master.read_nonblocking(&mut buf[..cap]) {
            Ok(NonBlockingRead::Bytes(n)) => {
                budget = budget.saturating_sub(n);
                on_bytes(&buf[..n]);
            }
            _ => break,
        }
    }
}

/// Computes Unix PTY `Interest` flags from pause state, broken-write latch, and queued input length.
#[cfg(unix)]
pub(crate) const fn unix_pty_interest(paused: bool, write_broken: bool, pending_writes: usize) -> Interest {
    Interest { readable: !paused, writable: !write_broken && pending_writes != 0 }
}
