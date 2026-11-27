//! Pure sans-IO [`SessionMachine<V>`] orchestrating one terminal session over any [`VtEngine`].
//!
//! Owns the VT engine exclusively (zero locks), shell markers, OSC 7501 status table, declarative
//! `ViewportHint` arbitration, DEC 2026 synchronized-output holds, incremental $O(\Delta\text{lines})$
//! `SearchWatch`, reactive event-driven `WaiterSet`, and bounded PTY stdin write buffering.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use apack_proto::messages::{BrokerMsg, ClientMsg, FrameUpdate};
use apack_proto::types::{
    CellPos, CellPx, CellSpan, CursorState, ErrorCode, GridSize, NonZeroStreamId, PackedRow, PasteOutcome,
    ProgressState, RequestId, ResizePolicy, SessionId, StreamId, SubFlags, ViewModes, ViewportHint, WaitCondition,
    WaitOutcome,
};
use apack_proto::{BULK_CHUNK_BYTES, MAX_ROWS_PER_FETCH, MAX_SEARCH_MATCHES, MAX_STRING_BYTES};

use crate::flow::OutboundPriority;
use crate::lease::ConnId;
use crate::marks::{MarkEffect, MarksMode, MarksTracker};
use crate::status::{StatusEffect, StatusTable};
use crate::time::{TimerId, TimerWheel, TimestampMs};
use crate::vt::{DirtyViewport, PasteEncode, VtEngine, VtEvent, VtSnapshotMeta};
use crate::{
    DEFAULT_WORD_DELIMITERS, MAX_EVENT_STRING_BYTES, MAX_IMAGE_PLACEMENTS, MAX_PTY_WRITE_BYTES,
    MAX_WAITERS_PER_SESSION, SEARCH_COALESCE_MS, SYNC_HOLD_MAX_MS, truncate_utf8,
};

/// Identifies one attached multiplexed stream across connections.
pub type StreamKey = (ConnId, NonZeroStreamId);

/// Outbound side effect emitted by [`SessionMachine`] for the shard / transport reactor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEffect {
    /// Reply or push message destined for one connection stream.
    Send {
        /// Target connection.
        conn: ConnId,
        /// Target wire stream (`StreamId::CONTROL` for control replies like `Wait`).
        stream_id: StreamId,
        /// Correlation ID (`RequestId::PUSH` for unsolicited events).
        req_id: RequestId,
        /// Outbound priority tier.
        priority: OutboundPriority,
        /// Message payload.
        msg: BrokerMsg,
    },
    /// PTY dimensions changed due to `ViewportHint` arbitration or `ExplicitResize`.
    ResizePty {
        /// New grid dimensions.
        size: GridSize,
        /// New cell pixel dimensions.
        cell_px: CellPx,
    },
    /// New bytes were queued in `pty_write_queue` and are ready to drain to the PTY fd.
    PtyWriteReady,
    /// One or more `SUB_VIEW` subscribers have dirty screen state ready to publish.
    ViewsDirty,
    /// Dispatch an async foreground-process verification check to the Blocking Pool.
    VerifyForeground {
        /// OSC 7501 status record ID.
        id: String,
        /// Monotonic revision to match on completion.
        changed: u64,
        /// Claimed application name.
        app: String,
    },
    /// Request the broker's global `ImageStore` to serve a blob chunk for `GetImageBlob`.
    FetchImageBlob {
        /// Requesting connection.
        conn: ConnId,
        /// Requesting stream.
        stream_id: NonZeroStreamId,
        /// Request correlation ID.
        req_id: RequestId,
        /// Content hash.
        blob: apack_proto::types::BlobHash,
        /// Starting byte offset.
        offset: u32,
        /// Maximum chunk bytes requested.
        max_bytes: u32,
    },
    /// Toggle manual PTY read pause requested via stream `Pause`/`Resume`.
    SetPtyPaused(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionTimer {
    SyncHold,
    SearchFlush,
    WaitTimeout,
}

#[derive(Debug)]
struct ViewSubscriber {
    last_epoch: Option<u64>,
    last_first_active_line: u64,
    last_scrollback_lines: u64,
    last_palette_gen: u64,
    last_images_gen: u64,
    last_cursor: Option<CursorState>,
    last_modes: Option<ViewModes>,
    needs_full_sync: bool,
    dirty_rows: BTreeSet<u16>,
    scroll_delta: i32,
}

impl ViewSubscriber {
    fn new() -> Self {
        Self {
            last_epoch: None,
            last_first_active_line: 0,
            last_scrollback_lines: 0,
            last_palette_gen: 0,
            last_images_gen: 0,
            last_cursor: None,
            last_modes: None,
            needs_full_sync: true,
            dirty_rows: BTreeSet::new(),
            scroll_delta: 0,
        }
    }

    fn apply_dirty(&mut self, epoch: u64, rows: u16, dirty: &DirtyViewport) {
        if self.last_epoch != Some(epoch) || dirty.full_dirty {
            self.needs_full_sync = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            return;
        }
        if self.needs_full_sync {
            return;
        }
        // Opposing-sign scrolls coalesced before publish can cancel out `scroll_delta` while
        // losing track of blank rows exposed at both edges; degrade safely to `full_sync`.
        if self.scroll_delta != 0
            && dirty.scroll_delta != 0
            && self.scroll_delta.signum() != dirty.scroll_delta.signum()
        {
            self.needs_full_sync = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            return;
        }
        let next_scroll = self.scroll_delta.saturating_add(dirty.scroll_delta);
        if next_scroll.unsigned_abs() >= u32::from(rows) {
            self.needs_full_sync = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            return;
        }
        if dirty.scroll_delta != 0 {
            let mut shifted = BTreeSet::new();
            for &r in &self.dirty_rows {
                let moved = i32::from(r) - dirty.scroll_delta;
                if moved >= 0 && moved < i32::from(rows) {
                    shifted.insert(moved as u16);
                }
            }
            self.dirty_rows = shifted;
        }
        self.scroll_delta = next_scroll;
        for &r in &dirty.dirty_rows {
            if r < rows {
                self.dirty_rows.insert(r);
            }
        }
    }
}

#[derive(Debug)]
struct SearchWatch {
    query: String,
    case_sensitive: bool,
    epoch: u64,
    last_oldest_line: u64,
    needs_full_refresh: bool,
    was_truncated: bool,
    dirty_range: Option<(u64, u64)>,
}

#[derive(Debug)]
struct Waiter {
    conn: ConnId,
    req_id: RequestId,
    condition: WaitCondition,
    timer: TimerId,
}

/// Pure sans-IO session state machine generic over any [`VtEngine`].
#[derive(Debug)]
pub struct SessionMachine<V: VtEngine> {
    id: SessionId,
    vt: V,
    alive: bool,
    word_delimiters: String,
    resize_policy: ResizePolicy,
    current_size: GridSize,
    current_cell_px: CellPx,
    title: String,
    marks: MarksTracker,
    statuses: StatusTable,
    timers: TimerWheel<SessionTimer>,
    sync_hold: Option<TimerId>,
    sync_hold_started: Option<TimestampMs>,
    search_flush_timer: Option<TimerId>,
    viewport_seq: u64,
    viewports: BTreeMap<StreamKey, (ViewportHint, u64)>,
    subscribers: BTreeMap<StreamKey, SubFlags>,
    views: BTreeMap<StreamKey, ViewSubscriber>,
    held_buttons: BTreeMap<StreamKey, u8>,
    searches: BTreeMap<StreamKey, SearchWatch>,
    manual_pauses: BTreeSet<StreamKey>,
    waiters: Vec<Waiter>,
    pty_write_queue: VecDeque<u8>,
    max_pty_write_bytes: usize,
}

impl<V: VtEngine> SessionMachine<V> {
    /// Creates a new session state machine owning `vt`.
    pub fn new(
        id: SessionId,
        vt: V,
        resize_policy: ResizePolicy,
        marks_mode: MarksMode,
        initial_cell_px: CellPx,
    ) -> Self {
        Self::with_pty_write_cap(id, vt, resize_policy, marks_mode, initial_cell_px, MAX_PTY_WRITE_BYTES)
    }

    /// Creates a new session state machine with a custom PTY write queue cap (useful in tests/sim).
    pub fn with_pty_write_cap(
        id: SessionId,
        vt: V,
        resize_policy: ResizePolicy,
        marks_mode: MarksMode,
        initial_cell_px: CellPx,
        max_pty_write_bytes: usize,
    ) -> Self {
        let current_size = vt.meta().size;
        Self {
            id,
            vt,
            alive: true,
            word_delimiters: String::from(DEFAULT_WORD_DELIMITERS),
            resize_policy,
            current_size,
            current_cell_px: initial_cell_px,
            title: String::new(),
            marks: MarksTracker::new(marks_mode),
            statuses: StatusTable::new(),
            timers: TimerWheel::new(),
            sync_hold: None,
            sync_hold_started: None,
            search_flush_timer: None,
            viewport_seq: 0,
            viewports: BTreeMap::new(),
            subscribers: BTreeMap::new(),
            views: BTreeMap::new(),
            held_buttons: BTreeMap::new(),
            searches: BTreeMap::new(),
            manual_pauses: BTreeSet::new(),
            waiters: Vec::new(),
            pty_write_queue: VecDeque::new(),
            max_pty_write_bytes,
        }
    }

    /// Returns this session's ID.
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Borrows the underlying VT engine (read-only, e.g. for simulator invariant oracles).
    pub fn vt(&self) -> &V {
        &self.vt
    }

    /// Mutably borrows the underlying VT engine (e.g. for cooperative compression or WAL restore).
    pub fn vt_mut(&mut self) -> &mut V {
        &mut self.vt
    }

    /// Returns `true` while the session process has not exited.
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Returns `true` when DEC 2026 synchronized output is actively holding view frames.
    pub fn is_sync_held(&self) -> bool {
        self.sync_hold.is_some()
    }

    /// Returns the current terminal title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the last working directory reported by shell integration or OSC 7.
    pub fn cwd(&self) -> Option<&str> {
        self.marks.last_cwd()
    }

    /// Returns the active shell marker vouching key, if in keyed mode.
    pub fn marks_key(&self) -> Option<apack_proto::types::MarksKey> {
        match self.marks.mode() {
            MarksMode::Keyed(k) => Some(k),
            MarksMode::Claimed => None,
        }
    }

    /// Returns the last accepted monotonic vouched mark sequence number.
    pub fn marks_last_seq(&self) -> u64 {
        self.marks.last_seq()
    }

    /// Returns the count of refused forged or replayed vouched marks.
    pub fn marks_refused(&self) -> u64 {
        self.marks.refused()
    }

    /// Returns a snapshot of the bounded shell command history ring.
    pub fn commands(&self) -> Vec<apack_proto::types::CommandRecord> {
        self.marks.commands()
    }

    /// Returns the current arbitrated grid size.
    pub fn current_size(&self) -> GridSize {
        self.current_size
    }

    /// Returns the current arbitrated cell pixel dimensions.
    pub fn current_cell_px(&self) -> CellPx {
        self.current_cell_px
    }

    /// Synchronizes cached grid/cell geometry after external VT replacement (e.g. `Seek` / restore).
    pub fn sync_vt_geometry(&mut self, cell_px: CellPx) {
        self.current_size = self.vt.meta().size;
        self.current_cell_px = cell_px;
    }

    /// Marks every attached `SUB_VIEW` subscriber as needing a fresh full-sync frame (e.g. after `Seek`).
    pub fn mark_all_views_full_sync(&mut self) {
        for view in self.views.values_mut() {
            view.needs_full_sync = true;
            view.dirty_rows.clear();
            view.scroll_delta = 0;
        }
    }

    /// Returns the earliest pending timer deadline for this session (DEC 2026 hold, search flush, or wait timeout).
    pub fn next_deadline(&self) -> Option<TimestampMs> {
        self.timers.next_deadline()
    }

    /// Returns queued bytes waiting to be written to the PTY stdin.
    pub fn pty_write_len(&self) -> usize {
        self.pty_write_queue.len()
    }

    /// Copies up to `max_bytes` from the front of the PTY stdin queue without removing them.
    pub fn peek_pty_write(&self, max_bytes: usize, out: &mut Vec<u8>) {
        out.clear();
        out.extend(self.pty_write_queue.iter().take(max_bytes).copied());
    }

    /// Removes `count` bytes from the front of the PTY stdin queue after a successful non-blocking write.
    pub fn consume_pty_write(&mut self, count: usize) {
        let drop_n = count.min(self.pty_write_queue.len());
        self.pty_write_queue.drain(..drop_n);
    }

    /// Drains up to `max_bytes` from the bounded PTY stdin write queue.
    pub fn drain_pty_write(&mut self, max_bytes: usize) -> Vec<u8> {
        let take = max_bytes.min(self.pty_write_queue.len());
        self.pty_write_queue.drain(..take).collect()
    }

    /// Attaches a stream to this session, recording its subscription flags and optional initial viewport hint.
    pub fn attach_stream(
        &mut self,
        key: StreamKey,
        flags: SubFlags,
        initial_viewport: Option<ViewportHint>,
        effects: &mut Vec<SessionEffect>,
    ) {
        self.subscribers.insert(key, flags);
        if flags.contains(SubFlags::VIEW) {
            self.views.insert(key, ViewSubscriber::new());
            if let Some(hint) = initial_viewport {
                self.viewport_seq += 1;
                self.viewports.insert(key, (hint, self.viewport_seq));
                self.recompute_viewport_geometry(effects);
            }
            self.notify_views_dirty(effects);
        }
    }

    /// Detaches a stream, cleaning up its view state, viewport hint, manual pause, and active search watch.
    pub fn detach_stream(&mut self, key: StreamKey, effects: &mut Vec<SessionEffect>) {
        self.subscribers.remove(&key);
        self.views.remove(&key);
        if self.views.is_empty() {
            if let Some(tid) = self.sync_hold.take() {
                self.timers.cancel(tid);
            }
            self.sync_hold_started = None;
        }
        self.held_buttons.remove(&key);
        self.searches.remove(&key);
        if self.searches.is_empty()
            && let Some(tid) = self.search_flush_timer.take()
        {
            self.timers.cancel(tid);
        }
        if self.manual_pauses.remove(&key) && self.manual_pauses.is_empty() {
            effects.push(SessionEffect::SetPtyPaused(false));
        }
        if self.viewports.remove(&key).is_some() {
            self.recompute_viewport_geometry(effects);
        }
    }

    /// Removes all waiters owned by `conn` when `conn` disconnects.
    pub fn cancel_waiters_for_conn(&mut self, conn: ConnId) {
        let mut kept = Vec::new();
        for w in self.waiters.drain(..) {
            if w.conn == conn {
                self.timers.cancel(w.timer);
            } else {
                kept.push(w);
            }
        }
        self.waiters = kept;
    }

    /// Registers a reactive, event-driven `Wait` request (zero polling).
    pub fn register_wait(
        &mut self,
        conn: ConnId,
        req_id: RequestId,
        condition: WaitCondition,
        timeout_ms: u32,
        now: TimestampMs,
        effects: &mut Vec<SessionEffect>,
    ) {
        if self.is_wait_met(&condition) {
            effects.push(SessionEffect::Send {
                conn,
                stream_id: StreamId::CONTROL,
                req_id,
                priority: OutboundPriority::Control,
                msg: BrokerMsg::Waited { outcome: WaitOutcome::Met },
            });
            return;
        }
        if !self.alive {
            effects.push(SessionEffect::Send {
                conn,
                stream_id: StreamId::CONTROL,
                req_id,
                priority: OutboundPriority::Control,
                msg: BrokerMsg::Waited { outcome: WaitOutcome::Exited },
            });
            return;
        }
        if self.waiters.len() >= MAX_WAITERS_PER_SESSION {
            send_stream_error(
                conn,
                StreamId::CONTROL,
                req_id,
                ErrorCode::ResourceLimit,
                "too many concurrent waiters on session",
                effects,
            );
            return;
        }
        let deadline = now.saturating_add(u64::from(timeout_ms));
        let timer = self.timers.schedule(deadline, SessionTimer::WaitTimeout);
        self.waiters.push(Waiter { conn, req_id, condition, timer });
    }

    /// Feeds raw PTY output bytes into the VT engine, dispatching side effects, marks, status,
    /// incremental search dirtying, reactive waiters, and view dirty bitsets.
    pub fn on_pty_output(&mut self, bytes: &[u8], now: TimestampMs, effects: &mut Vec<SessionEffect>) {
        if bytes.is_empty() {
            return;
        }
        self.on_vt_feed_with(now, effects, |vt, vt_events| vt.feed(bytes, vt_events));
    }

    /// Runs a custom VT feed closure (such as `feed_until_apc` or inline Kitty body chunks) and
    /// applies the exact same dirty-viewport, event, mark, search, and wait transitions as
    /// [`Self::on_pty_output`].
    pub fn on_vt_feed_with<R>(
        &mut self,
        now: TimestampMs,
        effects: &mut Vec<SessionEffect>,
        feed: impl FnOnce(&mut V, &mut Vec<VtEvent>) -> R,
    ) -> R {
        let before_meta = self.vt.meta();
        let mut vt_events = Vec::new();
        let result = feed(&mut self.vt, &mut vt_events);

        let after_meta = self.vt.meta();
        let dirty = self.vt.take_dirty_viewport();
        let epoch_changed = after_meta.epoch != before_meta.epoch;

        // Propagate viewport dirty bitsets to every attached SUB_VIEW subscriber.
        for view in self.views.values_mut() {
            view.apply_dirty(after_meta.epoch, after_meta.size.rows.get(), &dirty);
        }

        // Process VT side-effect events in stream order, coalescing VerifyForeground per status ID.
        let mut saw_epoch_event = false;
        let mut pending_verify: BTreeMap<String, (u64, String)> = BTreeMap::new();
        for ev in vt_events {
            match ev {
                VtEvent::Title(raw) => {
                    let title = truncate_utf8(&raw, MAX_EVENT_STRING_BYTES);
                    self.title = title.clone();
                    self.broadcast_event(BrokerMsg::Title { title }, effects);
                }
                VtEvent::Bell => {
                    self.broadcast_event(BrokerMsg::Bell, effects);
                }
                VtEvent::Clipboard { selection, data } => {
                    let selection = truncate_utf8(&selection, MAX_EVENT_STRING_BYTES);
                    let data = truncate_utf8(&data, MAX_STRING_BYTES);
                    self.broadcast_event(BrokerMsg::Clipboard { selection, data }, effects);
                }
                VtEvent::Notification { title, body } => {
                    let title = truncate_utf8(&title, MAX_EVENT_STRING_BYTES);
                    let body = truncate_utf8(&body, MAX_EVENT_STRING_BYTES);
                    self.broadcast_event(BrokerMsg::Notification { title, body }, effects);
                }
                VtEvent::Progress { state, percent } => {
                    let percent = normalize_progress(state, percent);
                    self.broadcast_event(BrokerMsg::Progress { state, percent }, effects);
                }
                VtEvent::Cwd(raw) => {
                    if let Some(cwd) = self.marks.on_cwd(&raw) {
                        self.broadcast_event(BrokerMsg::CwdChanged { cwd }, effects);
                    }
                }
                VtEvent::SyncOutput(true) => {
                    if !self.views.is_empty() {
                        let start = *self.sync_hold_started.get_or_insert(now);
                        let deadline = start.saturating_add(SYNC_HOLD_MAX_MS);
                        if self.sync_hold.is_none() && now < deadline {
                            let timer = self.timers.schedule(deadline, SessionTimer::SyncHold);
                            self.sync_hold = Some(timer);
                        }
                    }
                }
                VtEvent::SyncOutput(false) => {
                    if let Some(timer) = self.sync_hold.take() {
                        self.timers.cancel(timer);
                    }
                }
                VtEvent::EpochChanged => {
                    saw_epoch_event = true;
                    self.marks.on_epoch_change();
                }
                VtEvent::Osc133 { payload, cursor_line } => {
                    let mut mark_effects = Vec::new();
                    self.marks.on_osc133(&payload, cursor_line, now, &mut mark_effects);
                    self.apply_mark_effects(mark_effects, effects);
                }
                VtEvent::Osc633 { payload, cursor_line } => {
                    let mut mark_effects = Vec::new();
                    self.marks.on_osc633(&payload, cursor_line, now, &mut mark_effects);
                    self.apply_mark_effects(mark_effects, effects);
                }
                VtEvent::ProgramStatus(update) => {
                    let mut status_effects = Vec::new();
                    self.statuses.ingest_update(update, now, &mut status_effects);
                    self.apply_status_effects(status_effects, Some(&mut pending_verify), effects);
                }
                VtEvent::DeviceResponse(reply) => {
                    let _ = self.enqueue_device_response(&reply, effects);
                }
            }
        }
        if epoch_changed && !saw_epoch_event {
            self.marks.on_epoch_change();
        }
        for (id, (changed, app)) in pending_verify {
            effects.push(SessionEffect::VerifyForeground { id, changed, app });
        }

        // Compute the absolute dirty line range for incremental SearchWatch & WaitCondition::Text.
        let oldest_line = after_meta.oldest_line();
        let total_lines = after_meta.total_lines();
        let dirty_abs_range = compute_dirty_abs_range(&before_meta, &after_meta, &dirty);

        if !self.searches.is_empty() {
            let mut need_timer = false;
            for watch in self.searches.values_mut() {
                if watch.epoch != after_meta.epoch {
                    watch.epoch = after_meta.epoch;
                    watch.last_oldest_line = oldest_line;
                    watch.needs_full_refresh = true;
                    watch.dirty_range = None;
                    need_timer = true;
                } else {
                    if oldest_line > watch.last_oldest_line {
                        if watch.was_truncated {
                            watch.needs_full_refresh = true;
                        }
                        need_timer = true;
                    }
                    if let Some((from, to)) = dirty_abs_range {
                        if watch.was_truncated {
                            watch.needs_full_refresh = true;
                        } else {
                            watch.dirty_range = Some(match watch.dirty_range {
                                None => (from, to),
                                Some((a, b)) => (a.min(from), b.max(to)),
                            });
                        }
                        need_timer = true;
                    }
                }
            }
            if need_timer && self.search_flush_timer.is_none() {
                let tid = self.timers.schedule(now.saturating_add(SEARCH_COALESCE_MS), SessionTimer::SearchFlush);
                self.search_flush_timer = Some(tid);
            }
        }

        // Evaluate reactive WaitCondition::Text waiters over the enclosing logical lines (or full on epoch change).
        let text_scan = if epoch_changed {
            Some((oldest_line, total_lines))
        } else {
            dirty_abs_range.map(|(from, to)| self.expand_to_logical_lines(from, to, oldest_line, total_lines))
        };
        if let Some((from, to)) = text_scan {
            self.check_text_waiters(from, to, effects);
        }

        self.notify_views_dirty(effects);
        result
    }

    /// Enqueues a terminal-generated reply (such as a Kitty graphics refusal response) into the
    /// bounded PTY write queue, emitting [`SessionEffect::PtyWriteReady`] when accepted.
    pub fn enqueue_pty_reply(&mut self, reply: &[u8], effects: &mut Vec<SessionEffect>) {
        let _ = self.enqueue_device_response(reply, effects);
    }

    /// Applies an asynchronous OSC 7501 foreground-process verification result.
    pub fn apply_status_confirmation(
        &mut self,
        id: &str,
        changed: u64,
        confirmed: apack_proto::types::Confirmed,
        effects: &mut Vec<SessionEffect>,
    ) {
        let mut status_effects = Vec::new();
        self.statuses.apply_confirmation(id, changed, confirmed, &mut status_effects);
        self.apply_status_effects(status_effects, None, effects);
    }

    /// Handles session process termination: releases any sync hold, clears pending stdin bytes,
    /// finalizes running command, notifies `EVENTS` subscribers, and resolves every pending waiter.
    pub fn on_process_exit(
        &mut self,
        exit_code: Option<i32>,
        signal: Option<String>,
        now: TimestampMs,
        effects: &mut Vec<SessionEffect>,
    ) {
        if !self.alive {
            return;
        }
        self.alive = false;
        self.pty_write_queue.clear();
        if let Some(tid) = self.sync_hold.take() {
            self.timers.cancel(tid);
        }
        self.sync_hold_started = None;

        let mut mark_effects = Vec::new();
        self.marks.on_session_exit(now, &mut mark_effects);
        self.apply_mark_effects(mark_effects, effects);

        self.broadcast_event(BrokerMsg::Exited { exit_code, signal }, effects);

        for w in self.waiters.drain(..) {
            self.timers.cancel(w.timer);
            let outcome = match w.condition {
                WaitCondition::Exit => WaitOutcome::Met,
                _ => WaitOutcome::Exited,
            };
            effects.push(SessionEffect::Send {
                conn: w.conn,
                stream_id: StreamId::CONTROL,
                req_id: w.req_id,
                priority: OutboundPriority::Control,
                msg: BrokerMsg::Waited { outcome },
            });
        }
        self.notify_views_dirty(effects);
    }

    /// Fires all expired session timers (`SyncHold`, `SearchFlush`, `WaitTimeout`) at `now`.
    pub fn poll_timers(&mut self, now: TimestampMs, effects: &mut Vec<SessionEffect>) {
        let mut expired = Vec::new();
        self.timers.poll_expired(now, &mut expired);
        for (tid, timer) in expired {
            match timer {
                SessionTimer::SyncHold => {
                    if self.sync_hold == Some(tid) {
                        self.sync_hold = None;
                        self.notify_views_dirty(effects);
                    }
                }
                SessionTimer::SearchFlush => {
                    if self.search_flush_timer == Some(tid) {
                        self.search_flush_timer = None;
                        self.flush_searches(effects);
                    }
                }
                SessionTimer::WaitTimeout => {
                    if let Some(pos) = self.waiters.iter().position(|w| w.timer == tid) {
                        let w = self.waiters.remove(pos);
                        effects.push(SessionEffect::Send {
                            conn: w.conn,
                            stream_id: StreamId::CONTROL,
                            req_id: w.req_id,
                            priority: OutboundPriority::Control,
                            msg: BrokerMsg::Waited { outcome: WaitOutcome::Timeout },
                        });
                    }
                }
            }
        }
    }

    /// Handles a per-session `ClientMsg` routed by `BrokerCore::DispatchToSession`.
    pub fn handle_stream_msg(
        &mut self,
        key: StreamKey,
        req_id: RequestId,
        msg: ClientMsg,
        effects: &mut Vec<SessionEffect>,
    ) {
        let (conn, stream_id) = key;
        let wire_stream = stream_id.as_stream_id();
        let Some(&flags) = self.subscribers.get(&key) else {
            send_stream_error(conn, wire_stream, req_id, ErrorCode::NoSuchStream, "stream not attached", effects);
            return;
        };
        match msg {
            ClientMsg::ViewportHint(hint) => {
                if !flags.contains(SubFlags::VIEW) {
                    send_stream_error(
                        conn,
                        wire_stream,
                        req_id,
                        ErrorCode::Malformed,
                        "ViewportHint requires SUB_VIEW",
                        effects,
                    );
                    return;
                }
                self.viewport_seq += 1;
                self.viewports.insert(key, (hint, self.viewport_seq));
                self.recompute_viewport_geometry(effects);
            }
            ClientMsg::ExplicitResize { size, cell_px } => {
                self.apply_resize(size, cell_px, effects);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::Ok,
                });
            }
            ClientMsg::SetCursorDefaults { shape, blinking } => {
                self.vt.set_cursor_defaults(shape, blinking);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::Ok,
                });
                self.notify_views_dirty(effects);
            }
            ClientMsg::Key(input) => {
                let mut buf = Vec::new();
                self.vt.encode_key(&input, &mut buf);
                if self.enqueue_pty_bytes(&buf, effects).is_err() {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write rejected", effects);
                }
            }
            ClientMsg::Text { text } => {
                if self.enqueue_pty_bytes(text.as_bytes(), effects).is_err() {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write rejected", effects);
                }
            }
            ClientMsg::Pointer(input) => {
                let held = self.held_buttons.entry(key).or_insert(0);
                let mut buf = Vec::new();
                self.vt.encode_pointer(&input, held, &mut buf);
                if self.enqueue_pty_bytes(&buf, effects).is_err() {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write rejected", effects);
                }
            }
            ClientMsg::Focus { focused } => {
                if !focused {
                    self.held_buttons.remove(&key);
                }
                let mut buf = Vec::new();
                self.vt.encode_focus(focused, &mut buf);
                let _ = self.enqueue_pty_bytes(&buf, effects);
            }
            ClientMsg::Paste { text, allow_unsafe } => match self.vt.encode_paste(&text, allow_unsafe) {
                PasteEncode::Unsafe => {
                    effects.push(SessionEffect::Send {
                        conn,
                        stream_id: wire_stream,
                        req_id,
                        priority: OutboundPriority::Control,
                        msg: BrokerMsg::Pasted { outcome: PasteOutcome::Unsafe },
                    });
                }
                PasteEncode::Bytes(bytes) => {
                    if self.enqueue_pty_bytes(&bytes, effects).is_err() {
                        send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write rejected", effects);
                    } else {
                        effects.push(SessionEffect::Send {
                            conn,
                            stream_id: wire_stream,
                            req_id,
                            priority: OutboundPriority::Control,
                            msg: BrokerMsg::Pasted { outcome: PasteOutcome::Pasted },
                        });
                    }
                }
            },
            ClientMsg::Write { data } => {
                if self.enqueue_pty_bytes(&data, effects).is_err() {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write rejected", effects);
                }
            }
            ClientMsg::Pause => {
                let was_paused = !self.manual_pauses.is_empty();
                self.manual_pauses.insert(key);
                if !was_paused {
                    effects.push(SessionEffect::SetPtyPaused(true));
                }
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::Ok,
                });
            }
            ClientMsg::Resume => {
                if self.manual_pauses.remove(&key) && self.manual_pauses.is_empty() {
                    effects.push(SessionEffect::SetPtyPaused(false));
                }
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::Ok,
                });
            }
            ClientMsg::FetchRows { epoch, from_line, count } => {
                let cur_epoch = self.vt.meta().epoch;
                if epoch != cur_epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let mut rows = Vec::new();
                self.vt.extract_history_rows(from_line, count.min(MAX_ROWS_PER_FETCH), &mut rows);
                // Enforce the wire frame payload cap so heavily styled/hyperlinked rows never
                // exceed `MAX_FRAME_PAYLOAD` when serialized into `BrokerMsg::Rows`.
                truncate_rows_to_budget(&mut rows, apack_proto::MAX_FRAME_PAYLOAD.saturating_sub(4_096));
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::Rows { epoch: cur_epoch, rows },
                });
            }
            ClientMsg::WordAt { epoch, pos } => {
                if epoch != self.vt.meta().epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let span = self.vt.word_at(pos, &self.word_delimiters);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::SpanReply { span },
                });
            }
            ClientMsg::LineAt { epoch, line } => {
                if epoch != self.vt.meta().epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let span = self.vt.line_at(line);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::SpanReply { span },
                });
            }
            ClientMsg::TextOf { epoch, span } => {
                let meta = self.vt.meta();
                if epoch != meta.epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let text = match clamp_span(span, &meta) {
                    Some(clamped) => truncate_utf8(&self.vt.text_of(clamped, MAX_STRING_BYTES), MAX_STRING_BYTES),
                    None => String::new(),
                };
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::TextReply { text },
                });
            }
            ClientMsg::TextOfRect { epoch, span } => {
                let meta = self.vt.meta();
                if epoch != meta.epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let text = match clamp_rect_span(span, &meta) {
                    Some(clamped) => truncate_utf8(&self.vt.text_of_rect(clamped, MAX_STRING_BYTES), MAX_STRING_BYTES),
                    None => String::new(),
                };
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::TextReply { text },
                });
            }
            ClientMsg::SetSearch { query, case_sensitive } => {
                let meta = self.vt.meta();
                if query.is_empty() {
                    self.searches.remove(&key);
                    if self.searches.is_empty()
                        && let Some(tid) = self.search_flush_timer.take()
                    {
                        self.timers.cancel(tid);
                    }
                    effects.push(SessionEffect::Send {
                        conn,
                        stream_id: wire_stream,
                        req_id,
                        priority: OutboundPriority::Bulk,
                        msg: BrokerMsg::SearchMatches { epoch: meta.epoch, matches: Vec::new(), truncated: false },
                    });
                    return;
                }
                let oldest = meta.oldest_line();
                let total_lines = meta.total_lines();
                let mut matches = Vec::new();
                let truncated =
                    self.vt.search_lines(oldest, total_lines, &query, case_sensitive, MAX_SEARCH_MATCHES, &mut matches);
                matches.truncate(MAX_SEARCH_MATCHES);
                self.searches.insert(
                    key,
                    SearchWatch {
                        query,
                        case_sensitive,
                        epoch: meta.epoch,
                        last_oldest_line: oldest,
                        needs_full_refresh: false,
                        was_truncated: truncated,
                        dirty_range: None,
                    },
                );
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::SearchMatches { epoch: meta.epoch, matches, truncated },
                });
            }
            ClientMsg::Commands => {
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::CommandsReply { commands: self.marks.commands() },
                });
            }
            ClientMsg::CommandOutput { command_id } => {
                let text = self.extract_command_output(command_id);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::CommandOutputReply { text },
                });
            }
            ClientMsg::Images => {
                let meta = self.vt.meta();
                let mut placements = self.vt.images();
                placements.truncate(MAX_IMAGE_PLACEMENTS);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::ImagesReply { images_gen: meta.images_gen, placements },
                });
            }
            ClientMsg::GetImageBlob { blob, offset, max_bytes } => {
                let max_bytes = max_bytes.clamp(1, BULK_CHUNK_BYTES as u32);
                effects.push(SessionEffect::FetchImageBlob { conn, stream_id, req_id, blob, offset, max_bytes });
            }
            _ => {}
        }
    }

    /// Atomically builds a differential (or full-sync) [`FrameUpdate`] for `key` and invokes `publish`.
    ///
    /// If `publish` returns `false` (subscriber out of credit or frame already pending), all dirty
    /// tracking for `key` is left completely untouched so no visual update is ever dropped.
    /// Returns `true` iff a frame was published and committed.
    pub fn try_publish_view(&mut self, key: StreamKey, publish: impl FnOnce(Box<FrameUpdate>) -> bool) -> bool {
        if self.is_sync_held() {
            return false;
        }
        let Some(sub) = self.views.get(&key) else {
            return false;
        };
        let meta = self.vt.meta();
        let gens = self.vt.generations();
        let full_sync = sub.needs_full_sync || sub.last_epoch != Some(meta.epoch);
        let palette_changed = full_sync || sub.last_palette_gen != gens.palette;
        let nothing_changed = !full_sync
            && sub.dirty_rows.is_empty()
            && sub.scroll_delta == 0
            && sub.last_first_active_line == meta.first_active_line
            && sub.last_scrollback_lines == meta.scrollback_lines
            && sub.last_cursor == Some(meta.cursor)
            && sub.last_modes == Some(meta.modes)
            && !palette_changed
            && sub.last_images_gen == meta.images_gen;
        if nothing_changed {
            self.sync_hold_started = None;
            return false;
        }

        let row_indices: Vec<u16> =
            if full_sync { (0..meta.size.rows.get()).collect() } else { sub.dirty_rows.iter().copied().collect() };
        let mut rows: Vec<PackedRow> = Vec::new();
        self.vt.extract_viewport_rows(&row_indices, &mut rows);
        let deferred_rows =
            budget_frame_rows(&mut rows, meta.first_active_line, apack_proto::MAX_FRAME_PAYLOAD.saturating_sub(16_384));
        let palette = if palette_changed { Some(self.vt.palette_overrides()) } else { None };
        let frame = Box::new(FrameUpdate {
            seq: 0,
            epoch: meta.epoch,
            size: meta.size,
            first_active_line: meta.first_active_line,
            scrollback_lines: meta.scrollback_lines,
            scroll_delta: if full_sync { 0 } else { sub.scroll_delta },
            cursor: meta.cursor,
            modes: meta.modes,
            palette,
            rows,
            images_gen: meta.images_gen,
            full_sync,
        });

        if !publish(frame) {
            return false;
        }

        // Commit only after `publish` accepted the frame!
        if let Some(sub) = self.views.get_mut(&key) {
            sub.last_epoch = Some(meta.epoch);
            sub.last_first_active_line = meta.first_active_line;
            sub.last_scrollback_lines = meta.scrollback_lines;
            sub.last_palette_gen = gens.palette;
            sub.last_images_gen = meta.images_gen;
            sub.last_cursor = Some(meta.cursor);
            sub.last_modes = Some(meta.modes);
            sub.needs_full_sync = false;
            sub.dirty_rows.clear();
            for row_idx in deferred_rows {
                sub.dirty_rows.insert(row_idx);
            }
            sub.scroll_delta = 0;
        }
        // A view actually flushed outside a sync hold -> reset anti-starvation clock for future holds.
        self.sync_hold_started = None;
        true
    }

    /// Returns all currently attached `SUB_VIEW` stream keys.
    pub fn view_subscribers(&self) -> Vec<StreamKey> {
        self.views.keys().copied().collect()
    }

    fn notify_views_dirty(&self, effects: &mut Vec<SessionEffect>) {
        if !self.views.is_empty() && !self.is_sync_held() {
            effects.push(SessionEffect::ViewsDirty);
        }
    }

    fn enqueue_pty_bytes(&mut self, bytes: &[u8], effects: &mut Vec<SessionEffect>) -> Result<(), ()> {
        if !self.alive {
            return Err(());
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let next = self.pty_write_queue.len().checked_add(bytes.len()).ok_or(())?;
        if next > self.max_pty_write_bytes {
            return Err(());
        }
        self.pty_write_queue.extend(bytes);
        effects.push(SessionEffect::PtyWriteReady);
        Ok(())
    }

    fn enqueue_device_response(&mut self, bytes: &[u8], effects: &mut Vec<SessionEffect>) -> Result<(), ()> {
        if !self.alive || bytes.is_empty() {
            return Ok(());
        }
        let dsr_cap = self.max_pty_write_bytes / 2;
        let next = self.pty_write_queue.len().checked_add(bytes.len()).ok_or(())?;
        if next > dsr_cap {
            return Err(());
        }
        self.pty_write_queue.extend(bytes);
        effects.push(SessionEffect::PtyWriteReady);
        Ok(())
    }

    fn recompute_viewport_geometry(&mut self, effects: &mut Vec<SessionEffect>) {
        if self.resize_policy == ResizePolicy::ExplicitOverride {
            return;
        }
        let visible: Vec<&(ViewportHint, u64)> = self.viewports.values().filter(|(h, _)| h.visible).collect();
        if visible.is_empty() {
            return;
        }
        let (target_size, target_cell_px) = match self.resize_policy {
            ResizePolicy::LatestActive => {
                let best = visible
                    .iter()
                    .copied()
                    .filter(|(h, _)| h.focused)
                    .max_by_key(|(_, seq)| *seq)
                    .or_else(|| visible.iter().copied().max_by_key(|(_, seq)| *seq))
                    .expect("visible non-empty");
                (best.0.size, best.0.cell_px)
            }
            ResizePolicy::MinFit => {
                let min_cols = visible.iter().map(|(h, _)| h.size.cols.get()).min().expect("non-empty");
                let min_rows = visible.iter().map(|(h, _)| h.size.rows.get()).min().expect("non-empty");
                let latest_px = visible.iter().copied().max_by_key(|(_, seq)| *seq).expect("non-empty").0.cell_px;
                (GridSize::new(min_cols, min_rows).expect("valid min grid"), latest_px)
            }
            ResizePolicy::ExplicitOverride => return,
        };
        self.apply_resize(target_size, target_cell_px, effects);
    }

    fn apply_resize(&mut self, size: GridSize, cell_px: CellPx, effects: &mut Vec<SessionEffect>) {
        if size == self.current_size && cell_px == self.current_cell_px {
            return;
        }
        let prev_epoch = self.vt.meta().epoch;
        self.current_size = size;
        self.current_cell_px = cell_px;
        let mut vt_events = Vec::new();
        self.vt.resize(size, cell_px, &mut vt_events);
        for ev in vt_events {
            match ev {
                VtEvent::SyncOutput(false) => {
                    if let Some(tid) = self.sync_hold.take() {
                        self.timers.cancel(tid);
                    }
                    self.sync_hold_started = None;
                }
                VtEvent::DeviceResponse(reply) => {
                    let _ = self.enqueue_device_response(&reply, effects);
                }
                _ => {}
            }
        }
        let new_epoch = self.vt.meta().epoch;
        let _ = self.vt.take_dirty_viewport();
        if new_epoch != prev_epoch {
            self.marks.on_epoch_change();
            if !self.searches.is_empty() {
                if let Some(tid) = self.search_flush_timer.take() {
                    self.timers.cancel(tid);
                }
                for watch in self.searches.values_mut() {
                    watch.epoch = new_epoch;
                    watch.needs_full_refresh = true;
                    watch.dirty_range = None;
                }
                self.flush_searches(effects);
            }
        }
        for view in self.views.values_mut() {
            view.needs_full_sync = true;
            view.dirty_rows.clear();
            view.scroll_delta = 0;
        }
        effects.push(SessionEffect::ResizePty { size, cell_px });
        self.notify_views_dirty(effects);
    }

    fn apply_mark_effects(&mut self, mark_effects: Vec<MarkEffect>, effects: &mut Vec<SessionEffect>) {
        let mut idle_triggered = false;
        for me in mark_effects {
            match me {
                MarkEffect::CommandStarted(rec) => {
                    self.broadcast_event(BrokerMsg::CommandStarted(Box::new(rec)), effects);
                }
                MarkEffect::CommandFinished(rec) => {
                    self.broadcast_event(BrokerMsg::CommandFinished(Box::new(rec)), effects);
                }
                MarkEffect::PromptArrived => {
                    idle_triggered = true;
                }
                MarkEffect::CwdChanged(cwd) => {
                    self.broadcast_event(BrokerMsg::CwdChanged { cwd }, effects);
                }
            }
        }
        if idle_triggered {
            self.wake_waiters(|c| matches!(c, WaitCondition::Idle), effects);
        }
    }

    fn apply_status_effects(
        &mut self,
        status_effects: Vec<StatusEffect>,
        mut coalesce_verify: Option<&mut BTreeMap<String, (u64, String)>>,
        effects: &mut Vec<SessionEffect>,
    ) {
        let mut root_state = None;
        for se in status_effects {
            match se {
                StatusEffect::Updated(rec) => {
                    if rec.id.is_empty() {
                        root_state = Some(rec.state);
                    }
                    if rec.app.is_none()
                        && let Some(map) = coalesce_verify.as_mut()
                    {
                        map.remove(&rec.id);
                    }
                    self.broadcast_event(BrokerMsg::ProgramStatus(Box::new(rec)), effects);
                }
                StatusEffect::Cleared { id } => {
                    if let Some(map) = coalesce_verify.as_mut() {
                        map.remove(&id);
                    }
                    self.broadcast_event(BrokerMsg::ProgramStatusCleared { id }, effects);
                }
                StatusEffect::VerifyForeground { id, changed, app } => {
                    if let Some(map) = coalesce_verify.as_mut() {
                        map.insert(id, (changed, app));
                    } else {
                        effects.push(SessionEffect::VerifyForeground { id, changed, app });
                    }
                }
            }
        }
        if let Some(state) = root_state {
            self.wake_waiters(|c| matches!(c, WaitCondition::Status(states) if states.contains(&state)), effects);
        }
    }

    pub(crate) fn expand_to_logical_lines(&self, from: u64, to: u64, oldest: u64, total_lines: u64) -> (u64, u64) {
        let mut lo = from.max(oldest).min(total_lines);
        let mut hi = to.max(lo).min(total_lines);
        if lo < hi {
            if let Some(span) = self.vt.line_at(lo) {
                lo = lo.min(span.start.line).max(oldest);
            }
            if let Some(span) = self.vt.line_at(hi - 1) {
                hi = hi.max(span.end.line.saturating_add(1)).min(total_lines);
            }
        }
        (lo, hi)
    }

    fn flush_searches(&mut self, effects: &mut Vec<SessionEffect>) {
        let meta = self.vt.meta();
        let oldest = meta.oldest_line();
        let total_lines = meta.total_lines();
        let mut expanded_ranges: Vec<(StreamKey, u64, u64)> = Vec::new();
        for (&key, watch) in &self.searches {
            if !watch.needs_full_refresh
                && watch.epoch == meta.epoch
                && let Some((from, to)) = watch.dirty_range
            {
                let (lo, hi) = self.expand_to_logical_lines(from, to, oldest, total_lines);
                expanded_ranges.push((key, lo, hi));
            }
        }
        for (&(conn, stream_id), watch) in &mut self.searches {
            if watch.needs_full_refresh || watch.epoch != meta.epoch {
                watch.epoch = meta.epoch;
                watch.last_oldest_line = oldest;
                watch.needs_full_refresh = false;
                watch.dirty_range = None;
                let mut matches = Vec::new();
                let truncated = self.vt.search_lines(
                    oldest,
                    total_lines,
                    &watch.query,
                    watch.case_sensitive,
                    MAX_SEARCH_MATCHES,
                    &mut matches,
                );
                matches.truncate(MAX_SEARCH_MATCHES);
                watch.was_truncated = truncated;
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: stream_id.as_stream_id(),
                    req_id: RequestId::PUSH,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::SearchMatches { epoch: meta.epoch, matches, truncated },
                });
                continue;
            }
            if oldest > watch.last_oldest_line {
                let evicted_from = watch.last_oldest_line;
                watch.last_oldest_line = oldest;
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: stream_id.as_stream_id(),
                    req_id: RequestId::PUSH,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::SearchDelta {
                        epoch: meta.epoch,
                        from_line: evicted_from,
                        to_line: oldest,
                        added: Vec::new(),
                        truncated: false,
                    },
                });
            }
            if watch.dirty_range.take().is_some() {
                let (clamped_from, clamped_to) = expanded_ranges
                    .iter()
                    .find(|(k, _, _)| *k == (conn, stream_id))
                    .map_or((oldest, oldest), |&(_, lo, hi)| (lo, hi));
                let mut added = Vec::new();
                let mut truncated = self.vt.search_lines(
                    clamped_from,
                    clamped_to,
                    &watch.query,
                    watch.case_sensitive,
                    MAX_SEARCH_MATCHES,
                    &mut added,
                );
                // Enforce apack-proto SearchDelta wire invariant strictly.
                added.retain(|s| s.start.line >= clamped_from && s.end.line < clamped_to);
                if added.len() > MAX_SEARCH_MATCHES {
                    added.truncate(MAX_SEARCH_MATCHES);
                    truncated = true;
                }
                if truncated {
                    watch.was_truncated = true;
                }
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: stream_id.as_stream_id(),
                    req_id: RequestId::PUSH,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::SearchDelta {
                        epoch: meta.epoch,
                        from_line: clamped_from,
                        to_line: clamped_to,
                        added,
                        truncated,
                    },
                });
            }
        }
    }

    fn is_wait_met(&self, cond: &WaitCondition) -> bool {
        match cond {
            WaitCondition::Idle => self.marks.is_idle(),
            WaitCondition::Text(needle) => {
                let meta = self.vt.meta();
                let mut found = Vec::new();
                let _ = self.vt.search_lines(meta.oldest_line(), meta.total_lines(), needle, true, 1, &mut found);
                !found.is_empty()
            }
            WaitCondition::Status(states) => self.statuses.root().is_some_and(|r| states.contains(&r.state)),
            WaitCondition::Exit => !self.alive,
        }
    }

    fn check_text_waiters(&mut self, from_line: u64, to_line: u64, effects: &mut Vec<SessionEffect>) {
        let mut kept = Vec::new();
        for w in self.waiters.drain(..) {
            let matched = match &w.condition {
                WaitCondition::Text(needle) => {
                    let mut found = Vec::new();
                    let _ = self.vt.search_lines(from_line, to_line, needle, true, 1, &mut found);
                    !found.is_empty()
                }
                _ => false,
            };
            if matched {
                self.timers.cancel(w.timer);
                effects.push(SessionEffect::Send {
                    conn: w.conn,
                    stream_id: StreamId::CONTROL,
                    req_id: w.req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::Waited { outcome: WaitOutcome::Met },
                });
            } else {
                kept.push(w);
            }
        }
        self.waiters = kept;
    }

    fn wake_waiters(&mut self, predicate: impl Fn(&WaitCondition) -> bool, effects: &mut Vec<SessionEffect>) {
        let mut kept = Vec::new();
        for w in self.waiters.drain(..) {
            if predicate(&w.condition) {
                self.timers.cancel(w.timer);
                effects.push(SessionEffect::Send {
                    conn: w.conn,
                    stream_id: StreamId::CONTROL,
                    req_id: w.req_id,
                    priority: OutboundPriority::Control,
                    msg: BrokerMsg::Waited { outcome: WaitOutcome::Met },
                });
            } else {
                kept.push(w);
            }
        }
        self.waiters = kept;
    }

    fn extract_command_output(&self, command_id: u64) -> Option<String> {
        let cmd = self.marks.find_command(command_id)?;
        let start_line = cmd.output_line?;
        let meta = self.vt.meta();
        let oldest = meta.oldest_line();
        let total_lines = meta.total_lines();
        let end_line = cmd.end_line.unwrap_or_else(|| total_lines.saturating_sub(1));
        if start_line < oldest || start_line > end_line || end_line >= total_lines {
            return None;
        }
        let span = CellSpan {
            start: CellPos { line: start_line, col: 0 },
            end: CellPos { line: end_line, col: meta.size.cols.get() - 1 },
        };
        Some(truncate_utf8(&self.vt.text_of(span, MAX_STRING_BYTES), MAX_STRING_BYTES))
    }

    fn broadcast_event(&self, msg: BrokerMsg, effects: &mut Vec<SessionEffect>) {
        for (&(conn, stream_id), &flags) in &self.subscribers {
            if flags.contains(SubFlags::EVENTS) {
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: stream_id.as_stream_id(),
                    req_id: RequestId::PUSH,
                    priority: OutboundPriority::Event,
                    msg: msg.clone(),
                });
            }
        }
    }
}

fn normalize_progress(state: ProgressState, percent: Option<u8>) -> Option<u8> {
    match state {
        ProgressState::None | ProgressState::Indeterminate => None,
        ProgressState::Running | ProgressState::Error | ProgressState::Paused => percent.map(|p| p.min(100)),
    }
}

pub(crate) fn clamp_rect_span(span: CellSpan, meta: &VtSnapshotMeta) -> Option<CellSpan> {
    let oldest = meta.oldest_line();
    let max_line = meta.total_lines().checked_sub(1)?;
    let max_col = meta.size.cols.get() - 1;
    let start_line = span.start.line.max(oldest);
    let end_line = span.end.line.min(max_line);
    let lo_col = span.start.col.min(span.end.col);
    let hi_col = span.start.col.max(span.end.col).min(max_col);
    if start_line > end_line || lo_col > max_col {
        return None;
    }
    Some(CellSpan { start: CellPos { line: start_line, col: lo_col }, end: CellPos { line: end_line, col: hi_col } })
}

pub(crate) fn estimate_packed_row_bytes(row: &PackedRow) -> usize {
    let mut w = apack_proto::codec::Writer::new();
    row.encode(&mut w);
    w.as_slice().len()
}

pub(crate) fn budget_frame_rows(rows: &mut Vec<PackedRow>, first_active_line: u64, max_rows_bytes: usize) -> Vec<u16> {
    let mut used_bytes = 32usize;
    let mut keep_count = 0usize;
    for row in rows.iter() {
        let row_bytes = estimate_packed_row_bytes(row);
        if keep_count > 0 && used_bytes.saturating_add(row_bytes) > max_rows_bytes {
            break;
        }
        used_bytes = used_bytes.saturating_add(row_bytes);
        keep_count += 1;
    }
    let deferred: Vec<u16> = rows[keep_count..]
        .iter()
        .filter_map(|r| r.line.checked_sub(first_active_line).and_then(|d| u16::try_from(d).ok()))
        .collect();
    rows.truncate(keep_count);
    deferred
}

pub(crate) fn truncate_rows_to_budget(rows: &mut Vec<PackedRow>, max_rows_bytes: usize) {
    let mut used_bytes = 32usize;
    let mut keep_count = 0usize;
    for row in rows.iter() {
        let row_bytes = estimate_packed_row_bytes(row);
        if keep_count > 0 && used_bytes.saturating_add(row_bytes) > max_rows_bytes {
            break;
        }
        used_bytes = used_bytes.saturating_add(row_bytes);
        keep_count += 1;
    }
    rows.truncate(keep_count);
}

fn clamp_span(span: CellSpan, meta: &VtSnapshotMeta) -> Option<CellSpan> {
    let oldest = meta.oldest_line();
    let max_line = meta.total_lines().checked_sub(1)?;
    let max_col = meta.size.cols.get() - 1;
    let start_line = span.start.line.max(oldest);
    let end_line = span.end.line.min(max_line);
    let start_col = if start_line == span.start.line { span.start.col.min(max_col) } else { 0 };
    let end_col = if end_line == span.end.line { span.end.col.min(max_col) } else { max_col };
    if start_line > end_line || (start_line == end_line && start_col > end_col) {
        return None;
    }
    Some(CellSpan {
        start: CellPos { line: start_line, col: start_col },
        end: CellPos { line: end_line, col: end_col },
    })
}

fn compute_dirty_abs_range(
    before: &VtSnapshotMeta,
    after: &VtSnapshotMeta,
    dirty: &DirtyViewport,
) -> Option<(u64, u64)> {
    let oldest = after.oldest_line();
    let total_lines = after.total_lines();
    if dirty.full_dirty {
        return Some((oldest, total_lines));
    }
    let mut range: Option<(u64, u64)> = None;
    if after.first_active_line > before.first_active_line {
        let from = before.first_active_line.max(oldest);
        range = Some((from, total_lines));
    }
    if dirty.scroll_delta != 0 {
        let from = after.first_active_line;
        range = Some(match range {
            None => (from, total_lines),
            Some((lo, hi)) => (lo.min(from), hi.max(total_lines)),
        });
    }
    let rows = after.size.rows.get();
    for &r in &dirty.dirty_rows {
        if r >= rows {
            continue;
        }
        let abs = after.first_active_line.saturating_add(u64::from(r));
        let end = abs.saturating_add(1);
        range = Some(match range {
            None => (abs, end),
            Some((lo, hi)) => (lo.min(abs), hi.max(end)),
        });
    }
    range
}

fn send_stream_error(
    conn: ConnId,
    stream_id: StreamId,
    req_id: RequestId,
    code: ErrorCode,
    message: &str,
    effects: &mut Vec<SessionEffect>,
) {
    effects.push(SessionEffect::Send {
        conn,
        stream_id,
        req_id,
        priority: OutboundPriority::Control,
        msg: BrokerMsg::Error { code, message: String::from(message) },
    });
}
