use apack_proto::messages::{BrokerMsg, ClientMsg};
use apack_proto::types::{
    BlobHash, BlockedKind, CellPos, CellPx, CellSpan, CommandOutcome, Confirmed, CursorShape, CursorState, ErrorCode,
    GridSize, ImagePlacement, KeyAction, KeyInput, MarksKey, NonZeroStreamId, PackedRow, PackedRun, PaletteOverrides,
    PasteOutcome, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy, SessionId, StatusState,
    SubFlags, UnknownReason, ViewModes, ViewportHint, VouchedExitCode, WaitCondition, WaitOutcome,
};
use apack_proto::{MAX_COMMANDS, MAX_STATUSES};

use crate::lease::ConnId;
use crate::marks::{MarkEffect, MarksMode, MarksTracker};
use crate::session::{SessionEffect, SessionMachine};
use crate::status::StatusTable;
use crate::time::TimestampMs;
use crate::vt::{CompressStep, DirtyViewport, PasteEncode, VtEngine, VtEvent, VtGenerations, VtSnapshotMeta};
use crate::{SEARCH_COALESCE_MS, SYNC_HOLD_MAX_MS};

fn ts(ms: u64) -> TimestampMs {
    TimestampMs::from_millis(ms)
}

fn sid(n: u64) -> SessionId {
    SessionId::new(n).unwrap()
}

fn conn(n: u64) -> ConnId {
    ConnId::new(n).unwrap()
}

fn nz_stream(n: u32) -> NonZeroStreamId {
    NonZeroStreamId::new(n).unwrap()
}

fn sample_key() -> MarksKey {
    MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap()
}

/// Scripted `VtEngine` for exercising every `SessionMachine` code path inside `apack-core`.
struct StubVt {
    size: GridSize,
    epoch: u64,
    scrollback: u64,
    rows: Vec<String>,
    cursor: CursorState,
    modes: ViewModes,
    gens: VtGenerations,
    next_events: Vec<VtEvent>,
    dirty: DirtyViewport,
}

impl StubVt {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            size: GridSize::new(cols, rows).unwrap(),
            epoch: 1,
            scrollback: 0,
            rows: vec![String::new(); usize::from(rows)],
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes::default(),
            gens: VtGenerations::default(),
            next_events: Vec::new(),
            dirty: DirtyViewport { scroll_delta: 0, dirty_rows: Vec::new(), full_dirty: true },
        }
    }
}

impl VtEngine for StubVt {
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) {
        events.append(&mut self.next_events);
        let text = core::str::from_utf8(bytes).unwrap_or("");
        match text {
            "SCROLL" => {
                self.scrollback += 1;
                self.rows.remove(0);
                self.rows.push(String::from("new-line"));
                self.dirty.scroll_delta += 1;
                self.dirty.dirty_rows.push(self.size.rows.get() - 1);
                self.gens.grid += 1;
            }
            "SCROLL_DOWN" => {
                self.rows.pop();
                self.rows.insert(0, String::from("top-line"));
                self.dirty.scroll_delta -= 1;
                self.dirty.dirty_rows.push(0);
                self.gens.grid += 1;
            }
            "SCROLL_ONLY" => {
                self.dirty.scroll_delta += 1;
            }
            "SCROLL_DOWN_ONLY" => {
                self.dirty.scroll_delta -= 1;
            }
            "FULL_DIRTY" => {
                self.dirty.full_dirty = true;
            }
            "DIRTY_ROW0" => {
                self.rows[0] = String::from("row-zero");
                self.dirty.dirty_rows.push(0);
            }
            "DIRTY_ROW2" => {
                self.rows[2] = String::from("row-two");
                self.dirty.dirty_rows.push(2);
            }
            "DIRTY_ROW3" => {
                self.rows[3] = String::from("row-three");
                self.dirty.dirty_rows.push(3);
            }
            "DIRTY_OOB" => {
                self.dirty.dirty_rows.push(self.size.rows.get());
            }
            "BIG_SCROLL" => {
                self.dirty.scroll_delta += i32::from(self.size.rows.get());
            }
            "CURSOR" => {
                self.cursor.col += 1;
            }
            "CURSOR_HOME" => {
                self.cursor.row = 0;
                events.push(VtEvent::Osc133("D;0".into()));
            }
            "MODES" => {
                self.modes.mouse = !self.modes.mouse;
                self.gens.modes += 1;
            }
            "PALETTE" => {
                self.gens.palette += 1;
            }
            "IMAGES" => {
                self.gens.images += 1;
            }
            "ALT" => {
                self.epoch += 1;
                self.modes.alt_screen = !self.modes.alt_screen;
                self.gens.modes += 1;
                self.dirty.full_dirty = true;
            }
            "NOOP" => {}
            _ if !text.is_empty() => {
                self.rows[0] = String::from(text);
                self.dirty.dirty_rows.push(0);
                self.gens.grid += 1;
            }
            _ => {}
        }
    }

    fn resize(&mut self, size: GridSize, _cell_px: CellPx) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.epoch += 1;
        self.rows.resize(usize::from(size.rows.get()), String::new());
        self.dirty.full_dirty = true;
    }

    fn meta(&self) -> VtSnapshotMeta {
        VtSnapshotMeta {
            epoch: self.epoch,
            size: self.size,
            scrollback_lines: self.scrollback,
            cursor: self.cursor,
            modes: self.modes,
            images_gen: self.gens.images,
        }
    }

    fn generations(&self) -> VtGenerations {
        self.gens
    }

    fn palette_overrides(&self) -> PaletteOverrides {
        PaletteOverrides::default()
    }

    fn take_dirty_viewport(&mut self) -> DirtyViewport {
        core::mem::take(&mut self.dirty)
    }

    fn extract_viewport_rows(&self, rows: &[u16], out: &mut Vec<PackedRow>) {
        for &r in rows {
            assert!(usize::from(r) < self.rows.len(), "out-of-bounds viewport row {r} >= {}", self.rows.len());
            let t = &self.rows[usize::from(r)];
            let runs = if t.is_empty() {
                vec![]
            } else {
                vec![PackedRun {
                    col_start: 0,
                    col_len: t.len() as u16,
                    byte_offset: 0,
                    byte_len: t.len() as u32,
                    style_idx: 0,
                    link_idx: 0,
                    flags: 0,
                }]
            };
            out.push(PackedRow {
                line: self.scrollback + u64::from(r),
                flags: 0,
                text: t.clone(),
                runs,
                styles: vec![],
                links: vec![],
                pieces: vec![],
            });
        }
    }

    fn extract_history_rows(&self, from_line: u64, count: u32, out: &mut Vec<PackedRow>) {
        for i in 0..u64::from(count) {
            let line = from_line + i;
            if line >= self.scrollback && (line - self.scrollback) < self.rows.len() as u64 {
                self.extract_viewport_rows(&[(line - self.scrollback) as u16], out);
            }
        }
    }

    fn word_at(&self, pos: CellPos, _delimiters: &str) -> Option<CellSpan> {
        Some(CellSpan { start: pos, end: pos })
    }

    fn line_at(&self, line: u64) -> Option<CellSpan> {
        Some(CellSpan { start: CellPos { line, col: 0 }, end: CellPos { line, col: 3 } })
    }

    fn text_of(&self, span: CellSpan) -> String {
        format!("{}:{}:{}:{}", span.start.line, span.start.col, span.end.line, span.end.col)
    }

    fn text_of_rect(&self, span: CellSpan) -> String {
        format!("rect:{}:{}", span.start.col, span.end.col)
    }

    fn search_lines(
        &self,
        from_line: u64,
        to_line: u64,
        needle: &str,
        _case_sensitive: bool,
        limit: usize,
        out: &mut Vec<CellSpan>,
    ) -> bool {
        for line in from_line..to_line {
            if line >= self.scrollback {
                let idx = (line - self.scrollback) as usize;
                if let Some(text) = self.rows.get(idx)
                    && let Some(pos) = text.find(needle)
                {
                    if out.len() >= limit {
                        return true;
                    }
                    out.push(CellSpan {
                        start: CellPos { line, col: pos as u16 },
                        end: CellPos { line, col: (pos + needle.len() - 1) as u16 },
                    });
                }
            }
        }
        false
    }

    fn images(&self) -> Vec<ImagePlacement> {
        Vec::new()
    }

    fn encode_key(&mut self, key: &KeyInput, out: &mut Vec<u8>) {
        out.extend_from_slice(key.text.as_bytes());
    }

    fn encode_pointer(&mut self, _ev: &PointerInput, out: &mut Vec<u8>) {
        out.extend_from_slice(b"M");
    }

    fn encode_focus(&mut self, focused: bool, out: &mut Vec<u8>) {
        out.extend_from_slice(if focused { b"I" } else { b"O" });
    }

    fn encode_paste(&mut self, text: &str, allow_unsafe: bool) -> PasteEncode {
        if !allow_unsafe && text.contains('\n') {
            PasteEncode::Unsafe
        } else {
            PasteEncode::Bytes(text.as_bytes().to_vec())
        }
    }

    fn set_cursor_defaults(&mut self, shape: CursorShape, blinking: bool) {
        self.cursor.shape = shape;
        self.cursor.blinking = blinking;
    }

    fn compress_step(&mut self, _max_pages: usize) -> CompressStep {
        CompressStep::Done
    }

    fn scrollback_bytes(&self) -> u64 {
        self.scrollback * 100
    }
}

#[test]
fn marks_tracker_keyed_immunity_supersede_exit_ring_and_unescape() {
    let key = sample_key();
    let mut keyed = MarksTracker::new(MarksMode::Keyed(key));
    let mut fx = Vec::new();
    assert!(!keyed.is_idle());
    assert_eq!(keyed.last_cwd(), None);

    // Unvouched marks and bad keys are completely ignored in Keyed mode
    keyed.on_osc133("A", 0, ts(1), &mut fx);
    keyed.on_osc633("C", 1, ts(2), &mut fx);
    keyed.on_osc633("D;0", 2, ts(3), &mut fx);
    keyed.on_osc633("V;ffffffffffffffffffffffffffffffff;D;0", 2, ts(4), &mut fx);
    keyed.on_osc633("V;short;D;0", 2, ts(5), &mut fx);
    assert!(fx.is_empty());
    assert!(keyed.commands().is_empty());
    assert!(!keyed.is_idle());

    let k = key.as_str();
    keyed.on_osc633(&format!("V;{k};A"), 10, ts(10), &mut fx);
    assert!(keyed.is_idle());
    assert_eq!(fx, vec![MarkEffect::PromptArrived]);
    fx.clear();

    // Test lowercase hex (\x3b = ';', \x3f = '?'), uppercase hex (\x3F = '?', \x4A = 'J'), invalid hex (\x3g, \xg3), \\, and trailing backslash
    keyed.on_osc633(&format!("V;{k};P;Cwd=/tmp/a\\x3bb\\x3fc\\x3Fd\\x4Ae\\x3g\\xg3\\\\f\\"), 10, ts(11), &mut fx);
    assert_eq!(keyed.last_cwd(), Some("/tmp/a;b?c?dJe\\x3g\\xg3\\f\\"));
    assert_eq!(fx, vec![MarkEffect::CwdChanged("/tmp/a;b?c?dJe\\x3g\\xg3\\f\\".into())]);
    fx.clear();

    keyed.on_osc633(&format!("V;{k};E;echo\\x20\\x61\\x42"), 10, ts(12), &mut fx);
    keyed.on_osc633(&format!("V;{k};C"), 11, ts(13), &mut fx);
    assert!(!keyed.is_idle());
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandStarted(r) if r.id == 1 && r.command.as_deref() == Some("echo aB") && r.prompt_line == Some(10)
    ));
    fx.clear();

    keyed.on_osc633(&format!("V;{k};D;42"), 15, ts(20), &mut fx);
    assert!(keyed.is_idle());
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Exited(VouchedExitCode::from_vouched(42)) && r.end_line == Some(15)
    ));
    assert_eq!(fx[1], MarkEffect::PromptArrived);

    // Command 2: verify monotonic IDs (1, 2, 3) and NoStatus
    fx.clear();
    keyed.on_osc633(&format!("V;{k};C"), 16, ts(21), &mut fx);
    assert!(matches!(&fx[0], MarkEffect::CommandStarted(r) if r.id == 2));
    fx.clear();
    keyed.on_osc633(&format!("V;{k};D"), 17, ts(22), &mut fx);
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Unknown(UnknownReason::NoStatus)
    ));

    // Command 3 -> Superseded by Command 4 -> SessionEnded
    fx.clear();
    keyed.on_osc633(&format!("V;{k};C"), 18, ts(23), &mut fx);
    assert!(matches!(&fx[0], MarkEffect::CommandStarted(r) if r.id == 3));
    fx.clear();
    keyed.on_osc633(&format!("V;{k};C"), 19, ts(24), &mut fx);
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Unknown(UnknownReason::Superseded)
    ));
    assert!(matches!(&fx[1], MarkEffect::CommandStarted(r) if r.id == 4));
    fx.clear();
    keyed.on_epoch_change();
    assert!(
        keyed.commands().iter().all(|c| c.prompt_line.is_none() && c.output_line.is_none() && c.end_line.is_none())
    );
    keyed.on_session_exit(ts(30), &mut fx);
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Unknown(UnknownReason::SessionEnded)
    ));

    // Claimed mode + MAX_COMMANDS ring cap
    let mut claimed = MarksTracker::new(MarksMode::Claimed);
    for i in 0..=(MAX_COMMANDS as u64 + 2) {
        fx.clear();
        claimed.on_osc133("C", i, ts(i), &mut fx);
        claimed.on_osc133("D;7", i + 1, ts(i + 1), &mut fx);
    }
    assert_eq!(claimed.commands().len(), MAX_COMMANDS);
    assert_eq!(claimed.commands().last().unwrap().outcome, CommandOutcome::Claimed(Some(7)));

    fx.clear();
    claimed.on_osc133("C", 999, ts(999), &mut fx);
    fx.clear();
    claimed.on_session_exit(ts(1000), &mut fx);
    assert!(matches!(&fx[0], MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Claimed(None)));
}

#[test]
fn status_table_parsing_lru_eviction_and_async_confirmation() {
    let mut table = StatusTable::default();
    assert!(table.is_empty());
    assert_eq!(table.len(), 0);
    assert_eq!(table.root(), None);
    let mut fx = Vec::new();

    // Malformed / missing state ignored
    table.ingest("id=x;message=hi", ts(1), &mut fx);
    table.ingest("state=bogus", ts(2), &mut fx);
    assert!(table.is_empty());
    assert!(fx.is_empty());

    // Root Blocked status with kind, progress, app, title, message
    table.ingest("state=blocked;kind=permission;progress=50;app=claude;title=Run;message=Allow?", ts(10), &mut fx);
    assert!(!table.is_empty());
    assert_eq!(table.len(), 1);
    let root = table.root().unwrap();
    assert_eq!(root.state, StatusState::Blocked);
    assert_eq!(root.kind, Some(BlockedKind::Permission));
    assert_eq!(root.progress, Some(50));
    assert_eq!(root.app.as_deref(), Some("claude"));
    assert_eq!(root.title.as_deref(), Some("Run"));
    assert_eq!(root.message.as_deref(), Some("Allow?"));
    assert_eq!(root.since_ms, 10);
    assert_eq!(root.confirmed, Confirmed::Unknown);
    assert_eq!(fx.len(), 2);
    let rev = root.changed;
    assert_eq!(rev, 1);

    // Stale revision ignored; matching revision updates Confirmed
    fx.clear();
    table.apply_confirmation("", rev + 99, Confirmed::Matched, &mut fx);
    assert!(fx.is_empty());
    table.apply_confirmation("", rev, Confirmed::Matched, &mut fx);
    assert_eq!(table.root().unwrap().confirmed, Confirmed::Matched);
    assert_eq!(fx.len(), 1);

    // Empty app/title/message stay None; question & error variants parsed
    fx.clear();
    table.ingest("state=blocked;kind=question;app=;title=;message=;progress=101", ts(25), &mut fx);
    let root = table.root().unwrap();
    assert_eq!(root.changed, 2);
    assert_eq!(root.since_ms, 10);
    assert_eq!(root.kind, Some(BlockedKind::Question));
    assert_eq!(root.app, None);
    assert_eq!(root.title, None);
    assert_eq!(root.message, None);
    assert_eq!(root.progress, None);

    table.ingest("state=blocked;kind=auth", ts(26), &mut fx);
    assert_eq!(table.root().unwrap().kind, Some(BlockedKind::Auth));

    table.ingest("state=error;kind=permission", ts(30), &mut fx);
    assert_eq!(table.root().unwrap().state, StatusState::Error);
    assert_eq!(table.root().unwrap().since_ms, 30);
    assert_eq!(table.root().unwrap().kind, None);

    table.ingest("state=working", ts(32), &mut fx);
    assert_eq!(table.root().unwrap().state, StatusState::Working);

    // Odd-byte overlong UTF-8 lands mid-char at byte 512 ("a" + 300*"é" -> byte 511 is valid boundary!)
    let mut long_msg = String::from("a");
    long_msg.push_str(&"é".repeat(300));
    table.ingest(&format!("id=long;state=done;message={long_msg}"), ts(35), &mut fx);
    assert_eq!(table.get("long").unwrap().message.as_ref().unwrap().len(), 511);

    // Exceeding MAX_STATUSES evicts oldest non-root entry while preserving root ""
    for i in 0..(MAX_STATUSES + 5) {
        table.ingest(&format!("id=sub-{i};state=idle"), ts(100 + i as u64), &mut fx);
    }
    assert_eq!(table.len(), MAX_STATUSES);
    assert!(table.root().is_some());
    assert!(table.get("long").is_none());
}

#[test]
fn session_machine_view_dirty_tracking_and_individual_publish_triggers() {
    let vt = StubVt::new(40, 4);
    let mut sess = SessionMachine::new(sid(1), vt, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    let k1 = (conn(1), nz_stream(1));
    let mut fx = Vec::new();

    // No views attached -> on_pty_output does NOT emit ViewsDirty
    sess.on_pty_output(b"NOOP", ts(1), &mut fx);
    assert!(!fx.contains(&SessionEffect::ViewsDirty));

    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    assert!(fx.contains(&SessionEffect::ViewsDirty));

    // Initial full sync
    let mut frames = Vec::new();
    assert!(!sess.try_publish_view(k1, |_| false)); // rejected -> keeps dirty
    assert!(sess.try_publish_view(k1, |f| {
        frames.push(f);
        true
    }));
    assert!(frames.last().unwrap().full_sync);
    assert!(!sess.try_publish_view(k1, |_| true)); // completely clean

    // 1. Dirty row + scroll_up shifts dirty row index from 2 -> 1 and exposes row 3
    sess.on_pty_output(b"DIRTY_ROW2", ts(10), &mut fx);
    sess.on_pty_output(b"SCROLL", ts(11), &mut fx);
    assert!(sess.try_publish_view(k1, |f| {
        assert!(!f.full_sync);
        assert_eq!(f.scroll_delta, 1);
        let lines: Vec<u64> = f.rows.iter().map(|r| r.line).collect();
        // scrollback is now 1, row 2 shifted to viewport row 1 (abs 2), new bottom row is viewport row 3 (abs 4)
        assert_eq!(lines, vec![2, 4]);
        true
    }));

    // 2. Scroll down shifts dirty row 0 -> 1 and exposes row 0; OOB dirty row ignored
    sess.on_pty_output(b"hello", ts(12), &mut fx); // row 0 dirty
    sess.on_pty_output(b"SCROLL_DOWN", ts(13), &mut fx); // shifts row 0 -> row 1, marks row 0
    sess.on_pty_output(b"DIRTY_OOB", ts(14), &mut fx); // row 4 == rows -> ignored
    assert!(sess.try_publish_view(k1, |f| {
        assert!(!f.full_sync);
        assert_eq!(f.scroll_delta, -1);
        let lines: Vec<u64> = f.rows.iter().map(|r| r.line).collect();
        assert_eq!(lines, vec![1, 2]); // abs lines scrollback(1)+0 and scrollback(1)+1
        true
    }));

    // 3. Scroll >= rows triggers full_sync
    sess.on_pty_output(b"BIG_SCROLL", ts(15), &mut fx);
    assert!(sess.try_publish_view(k1, |f| {
        assert!(f.full_sync);
        assert_eq!(f.scroll_delta, 0);
        true
    }));

    // 4. Each individual metadata change triggers a frame when rows are clean:
    for (cmd, check_palette) in [
        (&b"SCROLL_ONLY"[..], false),
        (&b"CURSOR"[..], false),
        (&b"MODES"[..], false),
        (&b"PALETTE"[..], true),
        (&b"IMAGES"[..], false),
        (&b"ALT"[..], true),
    ] {
        sess.on_pty_output(cmd, ts(20), &mut fx);
        assert!(
            sess.try_publish_view(k1, |f| {
                assert_eq!(f.palette.is_some(), check_palette);
                true
            }),
            "expected frame publish after {:?}",
            core::str::from_utf8(cmd)
        );
        assert!(!sess.try_publish_view(k1, |_| true));
    }
}

#[test]
fn session_machine_viewport_policies_resizes_and_pty_queue() {
    let k1 = (conn(1), nz_stream(1));
    let k2 = (conn(2), nz_stream(2));
    let mut fx = Vec::new();

    // LatestActive: picks most recently focused visible view, ignoring hidden views
    let mut s = SessionMachine::with_pty_write_cap(
        sid(1),
        StubVt::new(40, 10),
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx { width: 8, height: 16 },
        8,
    );
    s.attach_stream(
        k1,
        SubFlags::from_bits(SubFlags::VIEW).unwrap(),
        Some(ViewportHint {
            size: GridSize::new(80, 24).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    assert_eq!(s.current_size(), GridSize::new(80, 24).unwrap());
    // Attach k2 focused -> switches to k2 (100x30)
    s.attach_stream(
        k2,
        SubFlags::from_bits(SubFlags::VIEW).unwrap(),
        Some(ViewportHint {
            size: GridSize::new(100, 30).unwrap(),
            cell_px: CellPx { width: 9, height: 18 },
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    assert_eq!(s.current_size(), GridSize::new(100, 30).unwrap());
    // k1 updates ViewportHint (focused=true) -> bumps viewport_seq and wins!
    fx.clear();
    s.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        ClientMsg::ViewportHint(ViewportHint {
            size: GridSize::new(90, 25).unwrap(),
            cell_px: CellPx { width: 8, height: 16 },
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    assert_eq!(s.current_size(), GridSize::new(90, 25).unwrap());
    assert!(
        fx.iter()
            .any(|e| matches!(e, SessionEffect::ResizePty { size, .. } if *size == GridSize::new(90, 25).unwrap()))
    );

    // Same-size ExplicitResize with different cell_px still resizes; identical (size, cell_px) is a no-op
    fx.clear();
    s.handle_stream_msg(
        k1,
        RequestId::from_raw(2),
        ClientMsg::ExplicitResize { size: GridSize::new(90, 25).unwrap(), cell_px: CellPx { width: 10, height: 20 } },
        &mut fx,
    );
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::ResizePty { .. })));
    fx.clear();
    s.handle_stream_msg(
        k1,
        RequestId::from_raw(3),
        ClientMsg::ExplicitResize { size: GridSize::new(90, 25).unwrap(), cell_px: CellPx { width: 10, height: 20 } },
        &mut fx,
    );
    assert!(!fx.iter().any(|e| matches!(e, SessionEffect::ResizePty { .. })));

    // PTY write queue exact boundary (8 bytes fits; 9th byte returns ErrorCode::Pty)
    fx.clear();
    s.handle_stream_msg(k1, RequestId::from_raw(4), ClientMsg::Write { data: b"12345678".to_vec() }, &mut fx);
    assert_eq!(s.pty_write_len(), 8);
    assert!(!fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Error { .. }, .. })));
    s.handle_stream_msg(k1, RequestId::from_raw(5), ClientMsg::Write { data: b"x".to_vec() }, &mut fx);
    assert!(
        fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Error { code: ErrorCode::Pty, .. }, .. }))
    );
    assert_eq!(s.drain_pty_write(8), b"12345678");
    assert_eq!(s.pty_write_len(), 0);

    // Paste unsafe gate vs confirmed paste
    fx.clear();
    s.handle_stream_msg(
        k1,
        RequestId::from_raw(6),
        ClientMsg::Paste { text: "a\nb".into(), allow_unsafe: false },
        &mut fx,
    );
    assert!(
        fx.iter()
            .any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Pasted { outcome: PasteOutcome::Unsafe }, .. }))
    );
    fx.clear();
    s.handle_stream_msg(
        k1,
        RequestId::from_raw(7),
        ClientMsg::Paste { text: "a\nb".into(), allow_unsafe: true },
        &mut fx,
    );
    assert!(
        fx.iter()
            .any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Pasted { outcome: PasteOutcome::Pasted }, .. }))
    );
    assert_eq!(s.drain_pty_write(8), b"a\nb");

    // MinFit policy + ExplicitOverride policy
    let mut min_s =
        SessionMachine::new(sid(2), StubVt::new(80, 24), ResizePolicy::MinFit, MarksMode::Claimed, CellPx::default());
    min_s.attach_stream(
        k1,
        SubFlags::from_bits(SubFlags::VIEW).unwrap(),
        Some(ViewportHint {
            size: GridSize::new(70, 20).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    min_s.attach_stream(
        k2,
        SubFlags::from_bits(SubFlags::VIEW).unwrap(),
        Some(ViewportHint {
            size: GridSize::new(50, 30).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: false,
        }),
        &mut fx,
    );
    assert_eq!(min_s.current_size(), GridSize::new(50, 20).unwrap());

    let mut exp_s = SessionMachine::new(
        sid(3),
        StubVt::new(80, 24),
        ResizePolicy::ExplicitOverride,
        MarksMode::Claimed,
        CellPx::default(),
    );
    exp_s.attach_stream(
        k1,
        SubFlags::from_bits(SubFlags::VIEW).unwrap(),
        Some(ViewportHint {
            size: GridSize::new(50, 15).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    assert_eq!(exp_s.current_size(), GridSize::new(80, 24).unwrap());
}

#[test]
fn session_machine_marks_search_waiters_command_output_and_exit() {
    let mut vt = StubVt::new(40, 4);
    vt.next_events = vec![
        VtEvent::Title("t1".into()),
        VtEvent::Bell,
        VtEvent::Clipboard { selection: "c".into(), data: "clip".into() },
        VtEvent::Notification { title: "n".into(), body: "b".into() },
        VtEvent::Progress { state: ProgressState::Running, percent: Some(40) },
        VtEvent::Cwd("/work".into()),
        VtEvent::DeviceResponse(b"\x1b[1;1R".to_vec()),
        VtEvent::Osc133("C".into()),
        VtEvent::Osc7501("state=working;app=tool".into()),
    ];
    let mut sess = SessionMachine::new(sid(1), vt, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    assert_eq!(sess.id(), sid(1));
    assert!(sess.is_alive());

    let k1 = (conn(1), nz_stream(1));
    let mut fx = Vec::new();
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);

    // Initial PTY output emits all VT events + CommandStarted (id=1) + VerifyForeground + DeviceResponse
    fx.clear();
    sess.on_pty_output(b"hello needle", ts(100), &mut fx);
    assert_eq!(sess.title(), "t1");
    assert_eq!(sess.drain_pty_write(32), b"\x1b[1;1R");
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::VerifyForeground { app, .. } if app == "tool")));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::CommandStarted(_), .. })));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Bell, .. })));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Clipboard { .. }, .. })));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Notification { .. }, .. })));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Progress { .. }, .. })));

    // Register 2 waiters (tests next_wait_id += 1 vs *= 1): Text waiter wakes reactively on dirty output,
    // and Idle waiter stays pending until OSC 133;D arrives!
    fx.clear();
    sess.register_wait(conn(1), RequestId::from_raw(10), WaitCondition::Text("new-line".into()), 500, ts(110), &mut fx);
    sess.register_wait(conn(1), RequestId::from_raw(11), WaitCondition::Idle, 500, ts(110), &mut fx);
    sess.register_wait(
        conn(1),
        RequestId::from_raw(12),
        WaitCondition::Status(vec![StatusState::Done]),
        500,
        ts(110),
        &mut fx,
    );
    sess.register_wait(conn(1), RequestId::from_raw(13), WaitCondition::Text("never".into()), 20, ts(110), &mut fx);

    // CommandOutput for active command id=1 returns Some("0:0:3:39"); unknown id=99 returns None
    fx.clear();
    sess.handle_stream_msg(k1, RequestId::from_raw(40), ClientMsg::Commands, &mut fx);
    sess.handle_stream_msg(k1, RequestId::from_raw(41), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    sess.handle_stream_msg(k1, RequestId::from_raw(42), ClientMsg::CommandOutput { command_id: 99 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::CommandsReply { commands }, .. }
        if *req_id == RequestId::from_raw(40) && commands.len() == 1
    )));
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: Some(t) }, .. }
        if *req_id == RequestId::from_raw(41) && t == "0:0:3:39"
    )));
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. }
        if *req_id == RequestId::from_raw(42)
    )));

    // Wait 13 times out at ts(130); other 3 remain pending
    fx.clear();
    sess.poll_timers(ts(130), &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Timeout }, .. }
        if *req_id == RequestId::from_raw(13)
    )));

    // SetSearch + SCROLL wakes Text waiter (req 10) reactively AND schedules 150ms SearchDelta flush
    fx.clear();
    sess.handle_stream_msg(
        k1,
        RequestId::from_raw(50),
        ClientMsg::SetSearch { query: "new-line".into(), case_sensitive: true },
        &mut fx,
    );
    fx.clear();
    sess.on_pty_output(b"SCROLL", ts(200), &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. }
        if *req_id == RequestId::from_raw(10)
    )));
    sess.poll_timers(ts(200 + SEARCH_COALESCE_MS), &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { msg: BrokerMsg::SearchDelta { from_line: 0, to_line: 5, added, .. }, .. }
        if added.len() == 1
    )));

    // Epoch change via ALT marks search for full refresh on next SearchFlush
    fx.clear();
    sess.on_pty_output(b"ALT", ts(400), &mut fx);
    sess.poll_timers(ts(400 + SEARCH_COALESCE_MS), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::SearchMatches { .. }, .. })));

    // Stream RPCs (WordAt, LineAt, TextOf, TextOfRect, FetchRows, Images, GetImageBlob, Key, Text, Pointer, Focus, SetCursorDefaults)
    let cur_epoch = sess.vt().meta().epoch;
    let stale_epoch = cur_epoch + 99;
    fx.clear();
    for (req, msg) in [
        (60, ClientMsg::FetchRows { epoch: stale_epoch, from_line: 0, count: 1 }),
        (61, ClientMsg::FetchRows { epoch: cur_epoch, from_line: 1, count: 2 }),
        (62, ClientMsg::WordAt { epoch: stale_epoch, pos: CellPos { line: 0, col: 0 } }),
        (63, ClientMsg::WordAt { epoch: cur_epoch, pos: CellPos { line: 1, col: 2 } }),
        (64, ClientMsg::LineAt { epoch: stale_epoch, line: 0 }),
        (65, ClientMsg::LineAt { epoch: cur_epoch, line: 1 }),
        (
            66,
            ClientMsg::TextOf {
                epoch: stale_epoch,
                span: CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 0, col: 1 } },
            },
        ),
        (
            67,
            ClientMsg::TextOf {
                epoch: cur_epoch,
                span: CellSpan { start: CellPos { line: 1, col: 0 }, end: CellPos { line: 1, col: 1 } },
            },
        ),
        (
            68,
            ClientMsg::TextOfRect {
                epoch: stale_epoch,
                span: CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 0, col: 1 } },
            },
        ),
        (
            69,
            ClientMsg::TextOfRect {
                epoch: cur_epoch,
                span: CellSpan { start: CellPos { line: 1, col: 0 }, end: CellPos { line: 1, col: 2 } },
            },
        ),
        (70, ClientMsg::Images),
        (71, ClientMsg::GetImageBlob { blob: BlobHash([1; 32]), offset: 0, max_bytes: 1024 }),
        (
            72,
            ClientMsg::Key(KeyInput {
                code: "KeyA".into(),
                action: KeyAction::Press,
                modifiers: 0,
                text: "a".into(),
                unshifted_codepoint: 0,
                composed: false,
            }),
        ),
        (73, ClientMsg::Text { text: "b".into() }),
        (
            74,
            ClientMsg::Pointer(PointerInput {
                action: PointerAction::Press,
                button: 0,
                modifiers: 0,
                x_px: 0,
                y_px: 0,
                cell_px: CellPx::default(),
            }),
        ),
        (75, ClientMsg::Focus { focused: true }),
        (76, ClientMsg::SetCursorDefaults { shape: CursorShape::Underline, blinking: true }),
    ] {
        sess.handle_stream_msg(k1, RequestId::from_raw(req), msg, &mut fx);
    }
    assert_eq!(sess.drain_pty_write(32), b"abMI");
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Error { code: ErrorCode::StaleEpoch, .. }, .. } if *req_id == RequestId::from_raw(60))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Rows { .. }, .. } if *req_id == RequestId::from_raw(61))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Error { code: ErrorCode::StaleEpoch, .. }, .. } if *req_id == RequestId::from_raw(62))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::SpanReply { span: Some(_) }, .. } if *req_id == RequestId::from_raw(63))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Error { code: ErrorCode::StaleEpoch, .. }, .. } if *req_id == RequestId::from_raw(64))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::SpanReply { span: Some(_) }, .. } if *req_id == RequestId::from_raw(65))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Error { code: ErrorCode::StaleEpoch, .. }, .. } if *req_id == RequestId::from_raw(66))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::TextReply { text }, .. } if *req_id == RequestId::from_raw(67) && text == "1:0:1:1")));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Error { code: ErrorCode::StaleEpoch, .. }, .. } if *req_id == RequestId::from_raw(68))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::TextReply { text }, .. } if *req_id == RequestId::from_raw(69) && text == "rect:0:2")));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::ImagesReply { .. }, .. } if *req_id == RequestId::from_raw(70))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::FetchImageBlob { .. })));
    assert!(fx.iter().any(
        |e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Ok, .. } if *req_id == RequestId::from_raw(76))
    ));

    // Process exit: wakes WaitCondition::Exit with Met, remaining non-Exit waiters (Idle=11, Status=12) with Exited,
    // sets is_alive = false, and calling on_process_exit a second time is a no-op!
    fx.clear();
    sess.register_wait(conn(1), RequestId::from_raw(14), WaitCondition::Exit, 500, ts(450), &mut fx);
    sess.on_process_exit(Some(0), None, ts(460), &mut fx);
    assert!(!sess.is_alive());
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Exited }, .. } if *req_id == RequestId::from_raw(11))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Exited }, .. } if *req_id == RequestId::from_raw(12))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. } if *req_id == RequestId::from_raw(14))));
    fx.clear();
    sess.on_process_exit(Some(1), None, ts(465), &mut fx);
    assert!(fx.is_empty());

    // After exit: WaitCondition::Exit resolves Met immediately; WaitCondition::Idle resolves Exited immediately
    sess.register_wait(conn(1), RequestId::from_raw(15), WaitCondition::Exit, 500, ts(470), &mut fx);
    sess.register_wait(conn(1), RequestId::from_raw(16), WaitCondition::Idle, 500, ts(470), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. } if *req_id == RequestId::from_raw(15))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Exited }, .. } if *req_id == RequestId::from_raw(16))));

    // DEC 2026 sync hold + Status waiter waking reactively + Idle waiter waking reactively on OSC 633;D
    let mut vt2 = StubVt::new(40, 4);
    vt2.next_events = vec![VtEvent::SyncOutput(true)];
    let mut s2 = SessionMachine::new(sid(2), vt2, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s2.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    fx.clear();
    s2.on_pty_output(b"x", ts(500), &mut fx);
    assert!(s2.is_sync_held());
    assert!(!fx.contains(&SessionEffect::ViewsDirty)); // suppressed while sync-held!
    assert!(!s2.try_publish_view(k1, |_| true));
    assert_eq!(s2.next_deadline(), Some(ts(500 + SYNC_HOLD_MAX_MS)));
    s2.poll_timers(ts(500 + SYNC_HOLD_MAX_MS), &mut fx);
    assert!(!s2.is_sync_held());
    assert!(fx.contains(&SessionEffect::ViewsDirty));

    // Reactive Idle + Status waiters
    fx.clear();
    s2.register_wait(conn(1), RequestId::from_raw(80), WaitCondition::Idle, 500, ts(2_000), &mut fx);
    s2.register_wait(
        conn(1),
        RequestId::from_raw(81),
        WaitCondition::Status(vec![StatusState::Done]),
        500,
        ts(2_000),
        &mut fx,
    );
    s2.register_wait(conn(2), RequestId::from_raw(82), WaitCondition::Exit, 500, ts(2_000), &mut fx);
    s2.cancel_waiters_for_conn(conn(2));

    let mut vt3 = StubVt::new(40, 4);
    vt3.next_events = vec![VtEvent::Osc633("D;0".into()), VtEvent::Osc7501("state=done".into())];
    let mut s3 = SessionMachine::new(sid(3), vt3, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s3.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    s3.register_wait(conn(1), RequestId::from_raw(90), WaitCondition::Idle, 500, ts(2_000), &mut fx);
    s3.register_wait(
        conn(1),
        RequestId::from_raw(91),
        WaitCondition::Status(vec![StatusState::Done]),
        500,
        ts(2_000),
        &mut fx,
    );
    fx.clear();
    s3.on_pty_output(b"done", ts(2_010), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. } if *req_id == RequestId::from_raw(90))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. } if *req_id == RequestId::from_raw(91))));

    // Detach cleans up views
    sess.detach_stream(k1, &mut fx);
    assert!(sess.view_subscribers().is_empty());
}

#[test]
fn session_edge_cases_and_boundary_invariants() {
    let mut fx = Vec::new();

    // 1. ViewSubscriber::apply_dirty boundaries:
    //    - same epoch + dirty.full_dirty -> forces full_sync
    //    - dirty row 0 shifted up by +1 -> dropped (moved == -1)
    //    - dirty row 3 (rows - 1) shifted down by -1 -> dropped (moved == rows)
    //    - out-of-bounds dirty row r == rows -> NOT emitted in frame
    let mut vt = StubVt::new(40, 4);
    vt.dirty.full_dirty = false;
    let mut sess = SessionMachine::new(sid(10), vt, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    let k1 = (conn(1), nz_stream(1));
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    assert!(sess.try_publish_view(k1, |_| true)); // initial full sync establishes last_epoch = Some(1)

    // Same epoch + full_dirty=true -> full_sync frame!
    sess.on_pty_output(b"FULL_DIRTY", ts(1), &mut fx);
    let mut full_seen = false;
    assert!(sess.try_publish_view(k1, |f| {
        full_seen = f.full_sync;
        true
    }));
    assert!(full_seen);

    // Dirty row 0 + scroll +1 -> row 0 shifts to -1 and is dropped (no rows in frame, only scroll_delta = +1)
    sess.on_pty_output(b"DIRTY_ROW0", ts(2), &mut fx);
    sess.on_pty_output(b"SCROLL_ONLY", ts(3), &mut fx);
    let mut frame_rows = Vec::new();
    let mut frame_delta = 0;
    assert!(sess.try_publish_view(k1, |f| {
        frame_rows = f.rows.iter().map(|r| r.line).collect();
        frame_delta = f.scroll_delta;
        true
    }));
    assert_eq!(frame_delta, 1);
    assert!(frame_rows.is_empty());

    // Dirty row 3 (rows - 1) + scroll -1 -> row 3 shifts to 4 (== rows) and is dropped!
    sess.on_pty_output(b"DIRTY_ROW3", ts(4), &mut fx);
    sess.on_pty_output(b"SCROLL_DOWN_ONLY", ts(5), &mut fx);
    assert!(sess.try_publish_view(k1, |f| {
        frame_rows = f.rows.iter().map(|r| r.line).collect();
        frame_delta = f.scroll_delta;
        true
    }));
    assert_eq!(frame_delta, -1);
    assert!(frame_rows.is_empty());

    // Dirty OOB row (r == rows) must be ignored -> frame must NOT even publish!
    sess.on_pty_output(b"DIRTY_OOB", ts(6), &mut fx);
    assert!(!sess.try_publish_view(k1, |_| true));

    // 2. attach_stream viewport_seq monotonicity: attaching 2 views with initial_viewport under LatestActive
    //    must choose the SECOND view's size (kills viewport_seq += 1 -> *= 1).
    let mut s_vp = SessionMachine::new(
        sid(11),
        StubVt::new(40, 10),
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
    );
    let hint1 = ViewportHint {
        size: GridSize::new(50, 15).unwrap(),
        cell_px: CellPx::default(),
        visible: true,
        focused: false,
    };
    let hint2 = ViewportHint {
        size: GridSize::new(70, 22).unwrap(),
        cell_px: CellPx::default(),
        visible: true,
        focused: false,
    };
    s_vp.attach_stream((conn(2), nz_stream(1)), SubFlags::from_bits(SubFlags::VIEW).unwrap(), Some(hint2), &mut fx);
    s_vp.attach_stream((conn(1), nz_stream(1)), SubFlags::from_bits(SubFlags::VIEW).unwrap(), Some(hint1), &mut fx);
    assert_eq!(s_vp.current_size(), GridSize::new(50, 15).unwrap());

    // 3. cancel_waiters_for_conn cancels ONLY the disconnected conn's waiters, keeping other conns' waiters live!
    let mut s_wait = SessionMachine::new(
        sid(12),
        StubVt::new(40, 4),
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
    );
    fx.clear();
    s_wait.register_wait(conn(1), RequestId::from_raw(1), WaitCondition::Exit, 50, ts(10), &mut fx);
    s_wait.register_wait(conn(2), RequestId::from_raw(2), WaitCondition::Exit, 50, ts(10), &mut fx);
    s_wait.cancel_waiters_for_conn(conn(1));
    s_wait.poll_timers(ts(100), &mut fx);
    assert!(!fx.iter().any(|e| matches!(e, SessionEffect::Send { conn: c, .. } if *c == conn(1))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { conn: c, req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Timeout }, .. } if *c == conn(2) && *req_id == RequestId::from_raw(2))));

    // 4. apply_status_confirmation propagates Confirmed::Matched via ProgramStatus broadcast!
    let mut vt_st = StubVt::new(40, 4);
    vt_st.next_events = vec![VtEvent::Osc7501("state=blocked;app=claude".into())];
    let mut s_st =
        SessionMachine::new(sid(13), vt_st, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_st.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    fx.clear();
    s_st.on_pty_output(b"NOOP", ts(10), &mut fx);
    s_st.apply_status_confirmation("", 1, Confirmed::Matched, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::ProgramStatus(r), .. } if r.confirmed == Confirmed::Matched)));

    // 5. SearchWatch coalesce & compute_dirty_abs_range:
    //    - NOOP chunk (scrollback unchanged, dirty_rows empty) must NOT schedule SearchFlush timer!
    //    - DIRTY_ROW2 (abs = 2 > 0, scrollback unchanged, full_dirty = false) MUST match WaitCondition::Text AND
    //      schedule SearchFlush once; a second dirty chunk before deadline must NOT postpone the deadline!
    let mut vt_srch = StubVt::new(40, 4);
    vt_srch.dirty.full_dirty = false;
    let mut s_srch =
        SessionMachine::new(sid(14), vt_srch, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_srch.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    fx.clear();
    s_srch.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        ClientMsg::SetSearch { query: "row-two".into(), case_sensitive: true },
        &mut fx,
    );
    assert_eq!(s_srch.next_deadline(), None);
    s_srch.on_pty_output(b"NOOP", ts(10), &mut fx);
    assert_eq!(s_srch.next_deadline(), None); // kills > -> >= and < -> <= in compute_dirty_abs_range and && -> ||!

    // Register WaitCondition::Text for "row-two" on row 2 (abs = 2): kills abs + 1 -> abs - 1 / abs * 1!
    fx.clear();
    s_srch.register_wait(conn(1), RequestId::from_raw(2), WaitCondition::Text("row-two".into()), 500, ts(20), &mut fx);
    s_srch.on_pty_output(b"DIRTY_ROW2", ts(20), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::Waited { outcome: WaitOutcome::Met }, .. } if *req_id == RequestId::from_raw(2))));
    assert_eq!(s_srch.next_deadline(), Some(ts(20 + SEARCH_COALESCE_MS)));
    // Second dirty chunk before SearchFlush deadline must preserve the original coalesce deadline!
    s_srch.on_pty_output(b"DIRTY_ROW3", ts(100), &mut fx);
    assert_eq!(s_srch.next_deadline(), Some(ts(20 + SEARCH_COALESCE_MS)));

    // 6. apply_resize: cell_px-only change (new_epoch == prev_epoch) must NOT clear mark line anchors!
    //    Whereas grid-size change (new_epoch != prev_epoch) MUST clear mark line anchors!
    let mut vt_rs = StubVt::new(40, 4);
    vt_rs.next_events = vec![VtEvent::Osc133("C".into()), VtEvent::Osc133("D;0".into())];
    let mut s_rs =
        SessionMachine::new(sid(15), vt_rs, ResizePolicy::ExplicitOverride, MarksMode::Claimed, CellPx::default());
    s_rs.on_pty_output(b"NOOP", ts(10), &mut fx);
    // Single-line command output (start_line == end_line == 0): kills start_line > end_line -> >= and ==!
    // Also send a subsequent normal NOOP chunk (epoch unchanged) to prove marks are NOT wiped (kills epoch_changed != -> ==)!
    s_rs.on_pty_output(b"NOOP", ts(11), &mut fx);
    fx.clear();
    s_rs.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: Some(t) }, .. } if *req_id == RequestId::from_raw(1) && t == "0:0:0:39")));

    // CellPx-only resize keeps same epoch -> mark anchors survive!
    s_rs.handle_stream_msg(
        k1,
        RequestId::from_raw(2),
        ClientMsg::ExplicitResize { size: GridSize::new(40, 4).unwrap(), cell_px: CellPx { width: 12, height: 24 } },
        &mut fx,
    );
    fx.clear();
    s_rs.handle_stream_msg(k1, RequestId::from_raw(3), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: Some(_) }, .. } if *req_id == RequestId::from_raw(3))));

    // Grid resize bumps epoch -> mark anchors cleared!
    s_rs.handle_stream_msg(
        k1,
        RequestId::from_raw(4),
        ClientMsg::ExplicitResize { size: GridSize::new(50, 5).unwrap(), cell_px: CellPx { width: 12, height: 24 } },
        &mut fx,
    );
    fx.clear();
    s_rs.handle_stream_msg(k1, RequestId::from_raw(5), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. } if *req_id == RequestId::from_raw(5))));

    // 7. CommandOutput where start_line (2) > end_line (0) while end_line < total_lines (4) must return None (kills || -> &&)!
    let mut vt_cmd = StubVt::new(40, 4);
    vt_cmd.cursor.row = 2;
    vt_cmd.next_events = vec![VtEvent::Osc133("C".into())];
    let mut s_cmd =
        SessionMachine::new(sid(16), vt_cmd, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_cmd.on_pty_output(b"NOOP", ts(10), &mut fx);
    s_cmd.on_pty_output(b"CURSOR_HOME", ts(11), &mut fx);
    fx.clear();
    s_cmd.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. } if *req_id == RequestId::from_raw(1))));

    // 8. Non-prompt mark (Osc133("C")) must NOT wake WaitCondition::Idle; PromptArrived (Osc133("A")) DOES wake it.
    let mut vt_cmd2 = StubVt::new(40, 4);
    vt_cmd2.next_events = vec![VtEvent::Osc133("C".into())];
    let mut s_idle =
        SessionMachine::new(sid(17), vt_cmd2, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    fx.clear();
    s_idle.register_wait(conn(1), RequestId::from_raw(1), WaitCondition::Idle, 500, ts(10), &mut fx);
    s_idle.on_pty_output(b"NOOP", ts(20), &mut fx);
    assert!(!fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Waited { .. }, .. })));
}
