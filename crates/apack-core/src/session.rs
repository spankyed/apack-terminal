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
    CellPx, CursorState, ErrorCode, GridSize, NonZeroStreamId, PackedRow, PasteOutcome, RequestId, ResizePolicy,
    SessionId, StreamId, SubFlags, ViewModes, ViewportHint, WaitCondition, WaitOutcome,
};
use apack_proto::{MAX_ROWS_PER_FETCH, MAX_SEARCH_MATCHES};

use crate::flow::OutboundPriority;
use crate::lease::ConnId;
use crate::marks::{MarkEffect, MarksMode, MarksTracker};
use crate::status::{StatusEffect, StatusTable};
use crate::time::{TimerId, TimerWheel, TimestampMs};
use crate::vt::{DirtyViewport, PasteEncode, VtEngine, VtEvent};
use crate::{DEFAULT_WORD_DELIMITERS, MAX_PTY_WRITE_BYTES, SEARCH_COALESCE_MS, SYNC_HOLD_MAX_MS};

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
    needs_full_refresh: bool,
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
    search_flush_timer: Option<TimerId>,
    viewport_seq: u64,
    viewports: BTreeMap<StreamKey, (ViewportHint, u64)>,
    subscribers: BTreeMap<StreamKey, SubFlags>,
    views: BTreeMap<StreamKey, ViewSubscriber>,
    searches: BTreeMap<StreamKey, SearchWatch>,
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
            search_flush_timer: None,
            viewport_seq: 0,
            viewports: BTreeMap::new(),
            subscribers: BTreeMap::new(),
            views: BTreeMap::new(),
            searches: BTreeMap::new(),
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

    /// Returns the current arbitrated grid size.
    pub fn current_size(&self) -> GridSize {
        self.current_size
    }

    /// Returns the earliest pending timer deadline for this session (DEC 2026 hold, search flush, or wait timeout).
    pub fn next_deadline(&self) -> Option<TimestampMs> {
        self.timers.next_deadline()
    }

    /// Returns queued bytes waiting to be written to the PTY stdin.
    pub fn pty_write_len(&self) -> usize {
        self.pty_write_queue.len()
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

    /// Detaches a stream, cleaning up its view state, viewport hint, and active search watch.
    pub fn detach_stream(&mut self, key: StreamKey, effects: &mut Vec<SessionEffect>) {
        self.subscribers.remove(&key);
        self.views.remove(&key);
        self.searches.remove(&key);
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
        let before_meta = self.vt.meta();
        let mut vt_events = Vec::new();
        self.vt.feed(bytes, &mut vt_events);

        let after_meta = self.vt.meta();
        let dirty = self.vt.take_dirty_viewport();
        let epoch_changed = after_meta.epoch != before_meta.epoch;
        if epoch_changed {
            self.marks.on_epoch_change();
        }

        // Propagate viewport dirty bitsets to every attached SUB_VIEW subscriber.
        for view in self.views.values_mut() {
            view.apply_dirty(after_meta.epoch, after_meta.size.rows.get(), &dirty);
        }

        // Process VT side-effect events.
        let cursor_abs_line = after_meta.scrollback_lines.saturating_add(u64::from(after_meta.cursor.row));
        for ev in vt_events {
            match ev {
                VtEvent::Title(title) => {
                    self.title = title.clone();
                    self.broadcast_event(BrokerMsg::Title { title }, effects);
                }
                VtEvent::Bell => {
                    self.broadcast_event(BrokerMsg::Bell, effects);
                }
                VtEvent::Clipboard { selection, data } => {
                    self.broadcast_event(BrokerMsg::Clipboard { selection, data }, effects);
                }
                VtEvent::Notification { title, body } => {
                    self.broadcast_event(BrokerMsg::Notification { title, body }, effects);
                }
                VtEvent::Progress { state, percent } => {
                    self.broadcast_event(BrokerMsg::Progress { state, percent }, effects);
                }
                VtEvent::Cwd(cwd) => {
                    self.broadcast_event(BrokerMsg::CwdChanged { cwd }, effects);
                }
                VtEvent::SyncOutput(true) => {
                    if self.sync_hold.is_none() {
                        let timer = self.timers.schedule(now.saturating_add(SYNC_HOLD_MAX_MS), SessionTimer::SyncHold);
                        self.sync_hold = Some(timer);
                    }
                }
                VtEvent::SyncOutput(false) => {
                    if let Some(timer) = self.sync_hold.take() {
                        self.timers.cancel(timer);
                    }
                }
                VtEvent::Osc133(payload) => {
                    let mut mark_effects = Vec::new();
                    self.marks.on_osc133(&payload, cursor_abs_line, now, &mut mark_effects);
                    self.apply_mark_effects(mark_effects, effects);
                }
                VtEvent::Osc633(payload) => {
                    let mut mark_effects = Vec::new();
                    self.marks.on_osc633(&payload, cursor_abs_line, now, &mut mark_effects);
                    self.apply_mark_effects(mark_effects, effects);
                }
                VtEvent::Osc7501(payload) => {
                    let mut status_effects = Vec::new();
                    self.statuses.ingest(&payload, now, &mut status_effects);
                    self.apply_status_effects(status_effects, effects);
                }
                VtEvent::DeviceResponse(reply) => {
                    let _ = self.enqueue_pty_bytes(&reply, effects);
                }
            }
        }

        // Compute the absolute dirty line range for incremental SearchWatch & WaitCondition::Text.
        let total_lines = after_meta.scrollback_lines.saturating_add(u64::from(after_meta.size.rows.get()));
        let dirty_abs_range = compute_dirty_abs_range(&before_meta, &after_meta, &dirty, total_lines);

        if !self.searches.is_empty() {
            let mut need_timer = false;
            for watch in self.searches.values_mut() {
                if watch.epoch != after_meta.epoch {
                    watch.epoch = after_meta.epoch;
                    watch.needs_full_refresh = true;
                    watch.dirty_range = None;
                    need_timer = true;
                } else if let Some((from, to)) = dirty_abs_range {
                    watch.dirty_range = Some(match watch.dirty_range {
                        None => (from, to),
                        Some((a, b)) => (a.min(from), b.max(to)),
                    });
                    need_timer = true;
                }
            }
            if need_timer && self.search_flush_timer.is_none() {
                let tid = self.timers.schedule(now.saturating_add(SEARCH_COALESCE_MS), SessionTimer::SearchFlush);
                self.search_flush_timer = Some(tid);
            }
        }

        // Evaluate reactive WaitCondition::Text waiters over only the newly dirty range (or full on epoch change).
        let text_scan = if epoch_changed { Some((0, total_lines)) } else { dirty_abs_range };
        if let Some((from, to)) = text_scan {
            self.check_text_waiters(from, to, effects);
        }

        self.notify_views_dirty(effects);
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
        self.apply_status_effects(status_effects, effects);
    }

    /// Handles session process termination: finalizes running command, notifies `EVENTS` subscribers,
    /// and resolves every pending waiter immediately.
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
        match msg {
            ClientMsg::ViewportHint(hint) => {
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
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write queue full", effects);
                }
            }
            ClientMsg::Text { text } => {
                if self.enqueue_pty_bytes(text.as_bytes(), effects).is_err() {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write queue full", effects);
                }
            }
            ClientMsg::Pointer(input) => {
                let mut buf = Vec::new();
                self.vt.encode_pointer(&input, &mut buf);
                if self.enqueue_pty_bytes(&buf, effects).is_err() {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write queue full", effects);
                }
            }
            ClientMsg::Focus { focused } => {
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
                        send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write queue full", effects);
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
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::Pty, "PTY write queue full", effects);
                }
            }
            ClientMsg::FetchRows { epoch, from_line, count } => {
                let cur_epoch = self.vt.meta().epoch;
                if epoch != cur_epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let mut rows = Vec::new();
                self.vt.extract_history_rows(from_line, count.min(MAX_ROWS_PER_FETCH), &mut rows);
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
                if epoch != self.vt.meta().epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let text = self.vt.text_of(span);
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::TextReply { text },
                });
            }
            ClientMsg::TextOfRect { epoch, span } => {
                if epoch != self.vt.meta().epoch {
                    send_stream_error(conn, wire_stream, req_id, ErrorCode::StaleEpoch, "stale epoch", effects);
                    return;
                }
                let text = self.vt.text_of_rect(span);
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
                    effects.push(SessionEffect::Send {
                        conn,
                        stream_id: wire_stream,
                        req_id,
                        priority: OutboundPriority::Bulk,
                        msg: BrokerMsg::SearchMatches { epoch: meta.epoch, matches: Vec::new(), truncated: false },
                    });
                    return;
                }
                let total_lines = meta.scrollback_lines.saturating_add(u64::from(meta.size.rows.get()));
                let mut matches = Vec::new();
                let truncated =
                    self.vt.search_lines(0, total_lines, &query, case_sensitive, MAX_SEARCH_MATCHES, &mut matches);
                self.searches.insert(
                    key,
                    SearchWatch {
                        query,
                        case_sensitive,
                        epoch: meta.epoch,
                        needs_full_refresh: false,
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
                let placements = self.vt.images();
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: wire_stream,
                    req_id,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::ImagesReply { images_gen: meta.images_gen, placements },
                });
            }
            ClientMsg::GetImageBlob { blob, offset, max_bytes } => {
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
            && sub.last_cursor == Some(meta.cursor)
            && sub.last_modes == Some(meta.modes)
            && !palette_changed
            && sub.last_images_gen == meta.images_gen;
        if nothing_changed {
            return false;
        }

        let row_indices: Vec<u16> =
            if full_sync { (0..meta.size.rows.get()).collect() } else { sub.dirty_rows.iter().copied().collect() };
        let mut rows: Vec<PackedRow> = Vec::new();
        self.vt.extract_viewport_rows(&row_indices, &mut rows);
        let palette = if palette_changed { Some(self.vt.palette_overrides()) } else { None };
        let frame = Box::new(FrameUpdate {
            seq: 0,
            epoch: meta.epoch,
            size: meta.size,
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
            sub.last_palette_gen = gens.palette;
            sub.last_images_gen = meta.images_gen;
            sub.last_cursor = Some(meta.cursor);
            sub.last_modes = Some(meta.modes);
            sub.needs_full_sync = false;
            sub.dirty_rows.clear();
            sub.scroll_delta = 0;
        }
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
        self.vt.resize(size, cell_px);
        let new_epoch = self.vt.meta().epoch;
        let _ = self.vt.take_dirty_viewport();
        if new_epoch != prev_epoch {
            self.marks.on_epoch_change();
            for watch in self.searches.values_mut() {
                watch.epoch = new_epoch;
                watch.needs_full_refresh = true;
                watch.dirty_range = None;
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

    fn apply_status_effects(&mut self, status_effects: Vec<StatusEffect>, effects: &mut Vec<SessionEffect>) {
        let mut root_state = None;
        for se in status_effects {
            match se {
                StatusEffect::Updated(rec) => {
                    if rec.id.is_empty() {
                        root_state = Some(rec.state);
                    }
                    self.broadcast_event(BrokerMsg::ProgramStatus(Box::new(rec)), effects);
                }
                StatusEffect::VerifyForeground { id, changed, app } => {
                    effects.push(SessionEffect::VerifyForeground { id, changed, app });
                }
            }
        }
        if let Some(state) = root_state {
            self.wake_waiters(|c| matches!(c, WaitCondition::Status(states) if states.contains(&state)), effects);
        }
    }

    fn flush_searches(&mut self, effects: &mut Vec<SessionEffect>) {
        let meta = self.vt.meta();
        let total_lines = meta.scrollback_lines.saturating_add(u64::from(meta.size.rows.get()));
        for (&(conn, stream_id), watch) in &mut self.searches {
            if watch.needs_full_refresh || watch.epoch != meta.epoch {
                watch.epoch = meta.epoch;
                watch.needs_full_refresh = false;
                watch.dirty_range = None;
                let mut matches = Vec::new();
                let truncated = self.vt.search_lines(
                    0,
                    total_lines,
                    &watch.query,
                    watch.case_sensitive,
                    MAX_SEARCH_MATCHES,
                    &mut matches,
                );
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: stream_id.as_stream_id(),
                    req_id: RequestId::PUSH,
                    priority: OutboundPriority::Bulk,
                    msg: BrokerMsg::SearchMatches { epoch: meta.epoch, matches, truncated },
                });
            } else if let Some((from, to)) = watch.dirty_range.take() {
                let clamped_to = to.min(total_lines);
                let clamped_from = from.min(clamped_to);
                let mut added = Vec::new();
                let truncated = self.vt.search_lines(
                    clamped_from,
                    clamped_to,
                    &watch.query,
                    watch.case_sensitive,
                    MAX_SEARCH_MATCHES,
                    &mut added,
                );
                effects.push(SessionEffect::Send {
                    conn,
                    stream_id: stream_id.as_stream_id(),
                    req_id: RequestId::PUSH,
                    priority: OutboundPriority::Event,
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
                let total = meta.scrollback_lines.saturating_add(u64::from(meta.size.rows.get()));
                let mut found = Vec::new();
                let _ = self.vt.search_lines(0, total, needle, true, 1, &mut found);
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
        let cmd = self.marks.commands().into_iter().find(|c| c.id == command_id)?;
        let start_line = cmd.output_line?;
        let meta = self.vt.meta();
        let total_lines = meta.scrollback_lines.saturating_add(u64::from(meta.size.rows.get()));
        let end_line = cmd.end_line.unwrap_or_else(|| total_lines.saturating_sub(1));
        if start_line > end_line || end_line >= total_lines {
            return None;
        }
        let span = apack_proto::types::CellSpan {
            start: apack_proto::types::CellPos { line: start_line, col: 0 },
            end: apack_proto::types::CellPos { line: end_line, col: meta.size.cols.get() - 1 },
        };
        Some(self.vt.text_of(span))
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

fn compute_dirty_abs_range(
    before: &crate::vt::VtSnapshotMeta,
    after: &crate::vt::VtSnapshotMeta,
    dirty: &DirtyViewport,
    total_lines: u64,
) -> Option<(u64, u64)> {
    if dirty.full_dirty {
        return Some((0, total_lines));
    }
    let mut range: Option<(u64, u64)> = if after.scrollback_lines > before.scrollback_lines {
        Some((before.scrollback_lines, total_lines))
    } else {
        None
    };
    for &r in &dirty.dirty_rows {
        let abs = after.scrollback_lines.saturating_add(u64::from(r));
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
