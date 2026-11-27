use apack_proto::messages::{BrokerMsg, ClientMsg};
use apack_proto::types::{
    BlobHash, BlockedKind, CellPos, CellPx, CellSpan, CommandOutcome, Confirmed, CursorShape, CursorState, ErrorCode,
    GridSize, ImagePlacement, KeyAction, KeyInput, MarksKey, NonZeroStreamId, PackedRow, PackedRun, PaletteOverrides,
    PasteOutcome, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy, SessionId, StatusState,
    SubFlags, UnknownReason, ViewModes, ViewportHint, VouchedExitCode, WaitCondition, WaitOutcome,
};
use apack_proto::{BULK_CHUNK_BYTES, MAX_COMMANDS, MAX_SEARCH_MATCHES, MAX_STATUSES};

use crate::lease::ConnId;
use crate::marks::{MarkEffect, MarksMode, MarksTracker};
use crate::session::{SessionEffect, SessionMachine};
use crate::status::StatusTable;
use crate::time::TimestampMs;
use crate::vt::{CompressStep, DirtyViewport, PasteEncode, VtEngine, VtEvent, VtGenerations, VtSnapshotMeta};
use crate::{MAX_WAITERS_PER_SESSION, SEARCH_COALESCE_MS, SYNC_HOLD_MAX_MS};

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
    first_active_line: u64,
    scrollback: u64,
    rows: Vec<String>,
    cursor: CursorState,
    modes: ViewModes,
    gens: VtGenerations,
    next_events: Vec<VtEvent>,
    dirty: DirtyViewport,
    wrapped_logical: Option<(u64, u64)>,
    last_search_range: core::cell::Cell<(u64, u64)>,
}

impl StubVt {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            size: GridSize::new(cols, rows).unwrap(),
            epoch: 1,
            first_active_line: 0,
            scrollback: 0,
            rows: vec![String::new(); usize::from(rows)],
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes::default(),
            gens: VtGenerations::default(),
            next_events: Vec::new(),
            dirty: DirtyViewport { scroll_delta: 0, dirty_rows: Vec::new(), full_dirty: true },
            wrapped_logical: None,
            last_search_range: core::cell::Cell::new((0, 0)),
        }
    }
}

impl VtEngine for StubVt {
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) {
        events.append(&mut self.next_events);
        let text = core::str::from_utf8(bytes).unwrap_or("");
        match text {
            "SCROLL" => {
                self.first_active_line += 1;
                self.scrollback += 1;
                self.rows.remove(0);
                self.rows.push(String::from("new-line"));
                self.dirty.scroll_delta += 1;
                self.dirty.dirty_rows.push(self.size.rows.get() - 1);
                self.gens.grid += 1;
            }
            "EVICT_SB" => {
                self.first_active_line += 1;
                // scrollback stays capped -> oldest_line advances by 1!
                self.gens.grid += 1;
            }
            "CLEAR_SB" => {
                self.scrollback = 0;
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
                events.push(VtEvent::Osc133 { payload: "D;0".into(), cursor_line: self.first_active_line });
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
            "SYNC_REARM" => {
                events.push(VtEvent::SyncOutput(false));
                events.push(VtEvent::SyncOutput(true));
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

    fn resize(&mut self, size: GridSize, _cell_px: CellPx, events: &mut Vec<VtEvent>) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.epoch += 1;
        events.push(VtEvent::SyncOutput(false));
        events.push(VtEvent::DeviceResponse(b"\x1b[48;10;20t".to_vec()));
        self.rows.resize(usize::from(size.rows.get()), String::new());
        self.dirty.full_dirty = true;
    }

    fn meta(&self) -> VtSnapshotMeta {
        VtSnapshotMeta {
            epoch: self.epoch,
            size: self.size,
            first_active_line: self.first_active_line,
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
                line: self.first_active_line + u64::from(r),
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
            if line >= self.first_active_line && (line - self.first_active_line) < self.rows.len() as u64 {
                self.extract_viewport_rows(&[(line - self.first_active_line) as u16], out);
            }
        }
    }

    fn word_at(&self, pos: CellPos, _delimiters: &str) -> Option<CellSpan> {
        Some(CellSpan { start: pos, end: pos })
    }

    fn line_at(&self, line: u64) -> Option<CellSpan> {
        if let Some((start, end)) = self.wrapped_logical
            && line >= start
            && line <= end
        {
            return Some(CellSpan { start: CellPos { line: start, col: 0 }, end: CellPos { line: end, col: 3 } });
        }
        Some(CellSpan { start: CellPos { line, col: 0 }, end: CellPos { line, col: 3 } })
    }

    fn text_of(&self, span: CellSpan, _max_bytes: usize) -> String {
        format!("{}:{}:{}:{}", span.start.line, span.start.col, span.end.line, span.end.col)
    }

    fn text_of_rect(&self, span: CellSpan, _max_bytes: usize) -> String {
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
        self.last_search_range.set((from_line, to_line));
        if needle == "__OOB_SPAN__" && from_line < to_line {
            // One span below from_line, one span at end_line == to_line, and one valid span inside [from_line, to_line)
            if from_line > 0 {
                out.push(CellSpan {
                    start: CellPos { line: from_line - 1, col: 0 },
                    end: CellPos { line: from_line, col: 0 },
                });
            }
            out.push(CellSpan { start: CellPos { line: from_line, col: 0 }, end: CellPos { line: to_line, col: 0 } });
            out.push(CellSpan { start: CellPos { line: from_line, col: 0 }, end: CellPos { line: from_line, col: 1 } });
            return false;
        }
        if needle == "__EXACT_LIMIT__" && from_line < to_line {
            for _ in 0..MAX_SEARCH_MATCHES {
                out.push(CellSpan {
                    start: CellPos { line: from_line, col: 0 },
                    end: CellPos { line: from_line, col: 0 },
                });
            }
            return false;
        }
        if needle == "__OVER_LIMIT__" && from_line < to_line {
            for _ in 0..=MAX_SEARCH_MATCHES {
                out.push(CellSpan {
                    start: CellPos { line: from_line, col: 0 },
                    end: CellPos { line: from_line, col: 0 },
                });
            }
            return false;
        }
        for line in from_line..to_line {
            if line >= self.first_active_line {
                let idx = (line - self.first_active_line) as usize;
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

    fn encode_pointer(&mut self, ev: &PointerInput, held_buttons: &mut u8, out: &mut Vec<u8>) {
        if ev.action == PointerAction::Press && ev.button < 3 {
            *held_buttons |= 1 << ev.button;
        }
        out.push(b'M');
        out.push(b'0' + *held_buttons);
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
    assert_eq!(keyed.refused(), 0);

    let k = key.as_str();
    // Unvouched marks, OSC 7 CWD, bad keys, leading-zero/zero/sign seqs are ignored/refused in Keyed mode
    keyed.on_osc133("A", 0, ts(1), &mut fx);
    keyed.on_osc633("C", 1, ts(2), &mut fx);
    keyed.on_osc633("D;0", 2, ts(3), &mut fx);
    keyed.on_cwd("/untrusted-osc7");
    assert_eq!(keyed.last_cwd(), None);

    keyed.on_osc633("V;S;1;ffffffffffffffffffffffffffffffff;cmd", 2, ts(4), &mut fx);
    keyed.on_osc633("V;S;1;short;cmd", 2, ts(5), &mut fx);
    keyed.on_osc633(&format!("V;S;0;{k};cmd"), 2, ts(6), &mut fx);
    keyed.on_osc633(&format!("V;S;01;{k};cmd"), 2, ts(7), &mut fx);
    keyed.on_osc633(&format!("V;S;+1;{k};cmd"), 2, ts(8), &mut fx);
    keyed.on_osc633(&format!("V;A;1;{k};extra"), 2, ts(9), &mut fx);
    keyed.on_osc633(&format!("V;X;1;{k};unknown"), 2, ts(9), &mut fx);
    assert!(fx.is_empty());
    assert!(keyed.commands().is_empty());
    assert!(!keyed.is_idle());
    assert_eq!(keyed.refused(), 7);

    // Valid vouched prompt arrival (seq=1) + replay rejection (seq=1 again!)
    keyed.on_osc633(&format!("V;A;1;{k}"), 10, ts(10), &mut fx);
    assert!(keyed.is_idle());
    assert_eq!(fx, vec![MarkEffect::PromptArrived]);
    fx.clear();
    keyed.on_osc633(&format!("V;A;1;{k}"), 10, ts(11), &mut fx);
    assert!(fx.is_empty());
    assert_eq!(keyed.refused(), 8);

    // Valid vouched Start (seq=2) with unescaping + replay rejection (seq=2 again!) + mismatched Finish (seq=99)
    keyed.on_osc633(&format!("V;S;2;{k};echo\\x20\\x61\\x42"), 11, ts(13), &mut fx);
    assert!(!keyed.is_idle());
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandStarted(r) if r.id == 2 && r.command.as_deref() == Some("echo aB") && r.prompt_line == Some(10)
    ));
    assert_eq!(keyed.find_command(2).and_then(|c| c.command.as_deref()), Some("echo aB"));
    assert!(keyed.find_command(999).is_none());
    fx.clear();
    keyed.on_osc633(&format!("V;S;2;{k};replay"), 12, ts(14), &mut fx);
    keyed.on_osc633(&format!("V;F;99;{k};0;/tmp"), 12, ts(15), &mut fx);
    keyed.on_osc633(&format!("V;F;2;{k};+0;/tmp"), 12, ts(16), &mut fx);
    keyed.on_osc633(&format!("V;F;2;{k};0"), 12, ts(17), &mut fx); // missing cwd semicolon
    assert!(fx.is_empty());
    assert_eq!(keyed.refused(), 12);

    // Valid vouched Finish (seq=2) with exit code -7 & unescaped CWD (all hex + invalid hex + \\ + trailing \)
    keyed.on_osc633(&format!("V;F;2;{k};-7;/tmp/a\\x3bb\\x3fc\\x3Fd\\x4Ae\\x3g\\xg3\\\\f\\"), 15, ts(20), &mut fx);
    assert!(keyed.is_idle());
    assert_eq!(keyed.last_cwd(), Some("/tmp/a;b?c?dJe\\x3g\\xg3\\f\\"));
    assert_eq!(fx[0], MarkEffect::CwdChanged("/tmp/a;b?c?dJe\\x3g\\xg3\\f\\".into()));
    assert!(matches!(
        &fx[1],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Exited(VouchedExitCode::from_vouched(-7)) && r.end_line == Some(15)
    ));
    assert_eq!(fx[2], MarkEffect::PromptArrived);

    // Finish without active command is refused
    fx.clear();
    keyed.on_osc633(&format!("V;F;2;{k};0;/tmp"), 15, ts(20), &mut fx);
    assert!(fx.is_empty());
    assert_eq!(keyed.refused(), 13);

    // Command 3: NoStatus (empty status field, empty cwd)
    fx.clear();
    keyed.on_osc633(&format!("V;S;3;{k};ls"), 16, ts(21), &mut fx);
    assert!(matches!(&fx[0], MarkEffect::CommandStarted(r) if r.id == 3));
    fx.clear();
    keyed.on_osc633(&format!("V;F;3;{k};;"), 17, ts(22), &mut fx);
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Unknown(UnknownReason::NoStatus)
    ));

    // Command 4 -> Superseded by Command 5 -> SessionEnded
    fx.clear();
    keyed.on_osc633(&format!("V;S;4;{k};sleep"), 18, ts(23), &mut fx);
    assert!(matches!(&fx[0], MarkEffect::CommandStarted(r) if r.id == 4));
    fx.clear();
    keyed.on_osc633(&format!("V;S;5;{k};pwd"), 19, ts(24), &mut fx);
    assert!(matches!(
        &fx[0],
        MarkEffect::CommandFinished(r) if r.outcome == CommandOutcome::Unknown(UnknownReason::Superseded)
    ));
    assert!(matches!(&fx[1], MarkEffect::CommandStarted(r) if r.id == 5));
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

    // Claimed mode: V;... is refused (never read as claim), OSC 7 updates cwd, E/P/A/C/D & MAX_COMMANDS ring cap
    let mut claimed = MarksTracker::new(MarksMode::Claimed);
    fx.clear();
    claimed.on_osc633(&format!("V;S;1;{k};nope"), 0, ts(1), &mut fx);
    assert!(fx.is_empty());
    assert_eq!(claimed.refused(), 1);
    claimed.on_cwd("/from-osc7");
    assert_eq!(claimed.last_cwd(), Some("/from-osc7"));
    claimed.on_osc633("P;Cwd=/from-633", 0, ts(2), &mut fx);
    assert_eq!(claimed.last_cwd(), Some("/from-633"));
    claimed.on_osc633("E;echo hi", 0, ts(3), &mut fx);
    claimed.on_osc633("A", 0, ts(4), &mut fx);
    assert!(claimed.is_idle());
    fx.clear();
    claimed.on_osc133("C", 1, ts(5), &mut fx);
    assert!(matches!(&fx[0], MarkEffect::CommandStarted(r) if r.id == 1 && r.command.as_deref() == Some("echo hi")));
    // Empty E; leaves pending_command None, and monotonic id advances 1 -> 2
    claimed.on_osc633("E;", 1, ts(5), &mut fx);
    fx.clear();
    claimed.on_osc133("C", 2, ts(6), &mut fx); // supersede in Claimed mode
    assert!(matches!(&fx[1], MarkEffect::CommandStarted(r) if r.id == 2 && r.command.is_none()));
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
        VtEvent::Osc133 { payload: "C".into(), cursor_line: 0 },
        VtEvent::ProgramStatus(crate::status::parse_osc7501("state=working;app=tool").unwrap()),
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
    assert_eq!(sess.drain_pty_write(32), b"abM1I");
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
    vt3.next_events = vec![
        VtEvent::Osc633 { payload: "D;0".into(), cursor_line: 0 },
        VtEvent::ProgramStatus(crate::status::parse_osc7501("state=done").unwrap()),
    ];
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
    vt_st.next_events = vec![VtEvent::ProgramStatus(crate::status::parse_osc7501("state=blocked;app=claude").unwrap())];
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
    //    Whereas grid-size change (new_epoch != prev_epoch) MUST clear mark line anchors AND immediately flush searches!
    let mut vt_rs = StubVt::new(40, 4);
    vt_rs.next_events = vec![
        VtEvent::Osc133 { payload: "C".into(), cursor_line: 0 },
        VtEvent::Osc133 { payload: "D;0".into(), cursor_line: 0 },
    ];
    let mut s_rs =
        SessionMachine::new(sid(15), vt_rs, ResizePolicy::ExplicitOverride, MarksMode::Claimed, CellPx::default());
    s_rs.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_rs.on_pty_output(b"NOOP", ts(10), &mut fx);
    // Single-line command output (start_line == end_line == 0): kills start_line > end_line -> >= and ==!
    // Also send a subsequent normal NOOP chunk (epoch unchanged) to prove marks are NOT wiped (kills epoch_changed != -> ==)!
    s_rs.on_pty_output(b"NOOP", ts(11), &mut fx);
    fx.clear();
    s_rs.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: Some(t) }, .. } if *req_id == RequestId::from_raw(1) && t == "0:0:0:39")));

    // Active search watch flushes immediately on epoch-bumping resize!
    s_rs.handle_stream_msg(
        k1,
        RequestId::from_raw(10),
        ClientMsg::SetSearch { query: "foo".into(), case_sensitive: true },
        &mut fx,
    );
    s_rs.on_pty_output(b"DIRTY_ROW2", ts(12), &mut fx); // schedules SearchFlush timer

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

    // Grid resize bumps epoch -> mark anchors cleared, SearchMatches flushed immediately, pending timer cancelled!
    fx.clear();
    s_rs.handle_stream_msg(
        k1,
        RequestId::from_raw(4),
        ClientMsg::ExplicitResize { size: GridSize::new(50, 5).unwrap(), cell_px: CellPx { width: 12, height: 24 } },
        &mut fx,
    );
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::SearchMatches { .. }, .. })));
    assert_eq!(s_rs.next_deadline(), None);
    fx.clear();
    s_rs.handle_stream_msg(k1, RequestId::from_raw(5), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. } if *req_id == RequestId::from_raw(5))));

    // 7. CommandOutput where start_line (2) > end_line (0) while end_line < total_lines (4) must return None!
    let mut vt_cmd = StubVt::new(40, 4);
    vt_cmd.cursor.row = 2;
    vt_cmd.next_events = vec![VtEvent::Osc133 { payload: "C".into(), cursor_line: 2 }];
    let mut s_cmd =
        SessionMachine::new(sid(16), vt_cmd, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_cmd.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_cmd.on_pty_output(b"NOOP", ts(10), &mut fx);
    s_cmd.on_pty_output(b"CURSOR_HOME", ts(11), &mut fx);
    fx.clear();
    s_cmd.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. } if *req_id == RequestId::from_raw(1))));

    // 8. Non-prompt mark (Osc133("C")) must NOT wake WaitCondition::Idle; PromptArrived (Osc133("A")) DOES wake it.
    let mut vt_cmd2 = StubVt::new(40, 4);
    vt_cmd2.next_events = vec![VtEvent::Osc133 { payload: "C".into(), cursor_line: 0 }];
    let mut s_idle =
        SessionMachine::new(sid(17), vt_cmd2, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    fx.clear();
    s_idle.register_wait(conn(1), RequestId::from_raw(1), WaitCondition::Idle, 500, ts(10), &mut fx);
    s_idle.on_pty_output(b"NOOP", ts(20), &mut fx);
    assert!(!fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Waited { .. }, .. })));
}

#[test]
fn session_security_hardening_and_audit_fixes() {
    let mut fx = Vec::new();
    let k1 = (conn(1), nz_stream(1));
    let k2 = (conn(2), nz_stream(1));

    // 1. Opposing-sign scrolls coalesced before publish degrade to full_sync; scrollback clear triggers publish
    let mut vt = StubVt::new(40, 4);
    vt.dirty.full_dirty = false;
    let mut sess = SessionMachine::new(sid(20), vt, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    assert!(sess.try_publish_view(k1, |_| true));
    sess.on_pty_output(b"SCROLL_ONLY", ts(1), &mut fx);
    sess.on_pty_output(b"SCROLL_DOWN_ONLY", ts(2), &mut fx);
    let mut full = false;
    assert!(sess.try_publish_view(k1, |f| {
        full = f.full_sync;
        true
    }));
    assert!(full);

    // Scrollback eviction + scrollback clear both trigger frame updates even without dirty viewport rows
    sess.on_pty_output(b"SCROLL", ts(3), &mut fx);
    assert!(sess.try_publish_view(k1, |_| true));
    sess.on_pty_output(b"EVICT_SB", ts(4), &mut fx);
    assert!(sess.try_publish_view(k1, |f| {
        assert_eq!(f.first_active_line, 2);
        assert_eq!(f.scrollback_lines, 1);
        true
    }));
    sess.on_pty_output(b"CLEAR_SB", ts(5), &mut fx);
    assert!(sess.try_publish_view(k1, |f| {
        assert_eq!(f.scrollback_lines, 0);
        true
    }));

    // 2. Unattached stream rejected (NoSuchStream) and non-VIEW ViewportHint rejected (Malformed)
    fx.clear();
    sess.handle_stream_msg(k2, RequestId::from_raw(1), ClientMsg::Commands, &mut fx);
    assert!(
        fx.iter().any(|e| matches!(
            e,
            SessionEffect::Send { msg: BrokerMsg::Error { code: ErrorCode::NoSuchStream, .. }, .. }
        ))
    );

    sess.attach_stream(k2, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    fx.clear();
    sess.handle_stream_msg(
        k2,
        RequestId::from_raw(2),
        ClientMsg::ViewportHint(ViewportHint {
            size: GridSize::new(30, 10).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    assert!(
        fx.iter()
            .any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Error { code: ErrorCode::Malformed, .. }, .. }))
    );

    // 3. Pause / Resume reference counting + automatic unpause on detach_stream
    fx.clear();
    sess.handle_stream_msg(k1, RequestId::from_raw(10), ClientMsg::Pause, &mut fx);
    assert!(fx.contains(&SessionEffect::SetPtyPaused(true)));
    fx.clear();
    sess.handle_stream_msg(k2, RequestId::from_raw(11), ClientMsg::Pause, &mut fx);
    assert!(!fx.contains(&SessionEffect::SetPtyPaused(true))); // already paused
    fx.clear();
    sess.handle_stream_msg(k1, RequestId::from_raw(12), ClientMsg::Resume, &mut fx);
    assert!(!fx.contains(&SessionEffect::SetPtyPaused(false))); // k2 still holds pause
    fx.clear();
    sess.detach_stream(k2, &mut fx);
    assert!(fx.contains(&SessionEffect::SetPtyPaused(false))); // last pauser detached -> unpaused!

    // 4. Progress normalization, VerifyForeground batch coalescing, and EpochChanged in stream order
    let mut vt_ev = StubVt::new(40, 4);
    vt_ev.next_events = vec![
        VtEvent::Progress { state: ProgressState::Indeterminate, percent: Some(80) },
        VtEvent::Progress { state: ProgressState::None, percent: Some(50) },
        VtEvent::Progress { state: ProgressState::Running, percent: Some(250) },
        VtEvent::ProgramStatus(crate::status::parse_osc7501("id=a;state=working;app=one").unwrap()),
        VtEvent::ProgramStatus(crate::status::parse_osc7501("id=a;state=done;app=two").unwrap()),
        VtEvent::EpochChanged,
    ];
    let mut s_ev =
        SessionMachine::new(sid(21), vt_ev, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_ev.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    fx.clear();
    s_ev.on_pty_output(b"NOOP", ts(10), &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { msg: BrokerMsg::Progress { state: ProgressState::Indeterminate, percent: None }, .. }
    )));
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { msg: BrokerMsg::Progress { state: ProgressState::None, percent: None }, .. }
    )));
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { msg: BrokerMsg::Progress { state: ProgressState::Running, percent: Some(100) }, .. }
    )));
    let verify_count = fx.iter().filter(|e| matches!(e, SessionEffect::VerifyForeground { .. })).count();
    assert_eq!(verify_count, 1);
    assert!(
        fx.iter()
            .any(|e| matches!(e, SessionEffect::VerifyForeground { id, changed: 2, app } if id == "a" && app == "two"))
    );

    // 5. DEC 2026 anti-starvation: 2026l + 2026h before any frame flushes CANNOT extend SYNC_HOLD_MAX_MS!
    //    And on_process_exit releases sync hold & clears pty_write_queue & rejects future writes!
    let mut vt_sync = StubVt::new(40, 4);
    vt_sync.next_events = vec![VtEvent::SyncOutput(true)];
    let mut s_sync = SessionMachine::with_pty_write_cap(
        sid(22),
        vt_sync,
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
        10,
    );
    s_sync.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_sync.on_pty_output(b"x", ts(1_000), &mut fx);
    assert_eq!(s_sync.next_deadline(), Some(ts(1_000 + SYNC_HOLD_MAX_MS)));
    // Attempt starvation at t=1900 by toggling 2026l + 2026h without publishing a frame
    s_sync.on_pty_output(b"SYNC_REARM", ts(1_900), &mut fx);
    assert_eq!(s_sync.next_deadline(), Some(ts(1_000 + SYNC_HOLD_MAX_MS))); // NOT pushed to 2900!
    // After expiry at t=2000, trying to re-arm at t=2050 BEFORE publishing still does NOT hold!
    s_sync.poll_timers(ts(2_000), &mut fx);
    assert!(!s_sync.is_sync_held());
    s_sync.on_pty_output(b"SYNC_REARM", ts(2_050), &mut fx);
    assert!(!s_sync.is_sync_held());
    assert!(s_sync.try_publish_view(k1, |_| true)); // publishing resets anti-starvation window!
    s_sync.on_pty_output(b"SYNC_REARM", ts(2_100), &mut fx);
    assert!(s_sync.is_sync_held());

    // DSR sub-quota leaves half the queue free for interactive user input!
    s_sync.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::Write { data: b"1234".to_vec() }, &mut fx);
    assert_eq!(s_sync.pty_write_len(), 4);
    // Queue cap is 10, DSR cap is 5; DSR of 6 bytes (> 5) is dropped, while user write of 8 bytes (<= 10) succeeds!
    let mut vt_dsr = StubVt::new(40, 4);
    vt_dsr.next_events = vec![VtEvent::DeviceResponse(b"123456".to_vec())];
    let mut s_dsr = SessionMachine::with_pty_write_cap(
        sid(23),
        vt_dsr,
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
        10,
    );
    s_dsr.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_dsr.on_pty_output(b"NOOP", ts(1), &mut fx);
    assert_eq!(s_dsr.pty_write_len(), 0); // 6 > 10/2 rejected!
    s_dsr.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::Write { data: b"12345678".to_vec() }, &mut fx);
    assert_eq!(s_dsr.pty_write_len(), 8); // user input still works!

    // on_process_exit releases sync hold, clears pty_write_queue, and rejects subsequent PTY writes
    fx.clear();
    s_sync.on_process_exit(Some(0), None, ts(2_200), &mut fx);
    assert!(!s_sync.is_sync_held());
    assert_eq!(s_sync.pty_write_len(), 0);
    assert!(fx.contains(&SessionEffect::ViewsDirty));
    fx.clear();
    s_sync.handle_stream_msg(k1, RequestId::from_raw(2), ClientMsg::Write { data: b"hi".to_vec() }, &mut fx);
    assert!(
        fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::Error { code: ErrorCode::Pty, .. }, .. }))
    );

    // 6. MAX_WAITERS_PER_SESSION cap returns ResourceLimit
    let mut s_lim = SessionMachine::new(
        sid(24),
        StubVt::new(40, 4),
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
    );
    for i in 0..MAX_WAITERS_PER_SESSION {
        s_lim.register_wait(conn(1), RequestId::from_raw(i as u32 + 1), WaitCondition::Exit, 500, ts(1), &mut fx);
    }
    fx.clear();
    s_lim.register_wait(conn(1), RequestId::from_raw(999), WaitCondition::Exit, 500, ts(1), &mut fx);
    assert!(fx.iter().any(|e| matches!(
        e,
        SessionEffect::Send { msg: BrokerMsg::Error { code: ErrorCode::ResourceLimit, .. }, .. }
    )));

    // 7. SearchWatch eviction delta, OOB span clamp on TextOf/TextOfRect, GetImageBlob max_bytes clamp,
    //    and SearchFlush timer cancelled when last search cleared or detached!
    let mut vt_s = StubVt::new(40, 4);
    vt_s.dirty.full_dirty = false;
    let mut s_s = SessionMachine::new(sid(25), vt_s, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_s.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        ClientMsg::SetSearch { query: "row".into(), case_sensitive: true },
        &mut fx,
    );
    // Evicting scrollback advances oldest_line -> schedules SearchFlush and emits eviction SearchDelta [0, 1)
    s_s.on_pty_output(b"SCROLL", ts(10), &mut fx);
    s_s.on_pty_output(b"EVICT_SB", ts(11), &mut fx);
    fx.clear();
    s_s.poll_timers(ts(10 + SEARCH_COALESCE_MS), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::SearchDelta { from_line: 0, to_line: 1, added, .. }, .. } if added.is_empty())));

    // Clearing search cancels pending SearchFlush timer
    s_s.on_pty_output(b"DIRTY_ROW2", ts(300), &mut fx);
    assert!(s_s.next_deadline().is_some());
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(2),
        ClientMsg::SetSearch { query: String::new(), case_sensitive: true },
        &mut fx,
    );
    assert_eq!(s_s.next_deadline(), None);

    // TextOf / TextOfRect with inverted/OOB spans return empty string; GetImageBlob clamps max_bytes
    let ep = s_s.vt().meta().epoch;
    fx.clear();
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(3),
        ClientMsg::TextOf {
            epoch: ep,
            span: CellSpan { start: CellPos { line: 999, col: 0 }, end: CellPos { line: 1000, col: 0 } },
        },
        &mut fx,
    );
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(4),
        ClientMsg::TextOfRect {
            epoch: ep,
            span: CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 0, col: 0 } },
        },
        &mut fx,
    );
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(5),
        ClientMsg::GetImageBlob { blob: BlobHash([2; 32]), offset: 0, max_bytes: u32::MAX },
        &mut fx,
    );
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(6),
        ClientMsg::GetImageBlob { blob: BlobHash([2; 32]), offset: 0, max_bytes: 0 },
        &mut fx,
    );
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::TextReply { text }, .. } if *req_id == RequestId::from_raw(3) && text.is_empty())));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::TextReply { text }, .. } if *req_id == RequestId::from_raw(4) && text.is_empty())));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::FetchImageBlob { req_id, max_bytes, .. } if *req_id == RequestId::from_raw(5) && *max_bytes == BULK_CHUNK_BYTES as u32)));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::FetchImageBlob { req_id, max_bytes, .. } if *req_id == RequestId::from_raw(6) && *max_bytes == 1)));

    // Detach cancels pending SearchFlush timer when last search watch leaves
    s_s.handle_stream_msg(
        k1,
        RequestId::from_raw(7),
        ClientMsg::SetSearch { query: "row".into(), case_sensitive: true },
        &mut fx,
    );
    s_s.on_pty_output(b"DIRTY_ROW2", ts(400), &mut fx);
    assert!(s_s.next_deadline().is_some());
    s_s.detach_stream(k1, &mut fx);
    assert_eq!(s_s.next_deadline(), None);
    assert!(!fx.contains(&SessionEffect::SetPtyPaused(false))); // non-paused detach must NOT emit SetPtyPaused(false)!

    // 8. Additional exact boundary checks killing remaining mutants:
    //    - Stream-ordered VtEvent::EpochChanged preserves post-event command anchor (kills !saw_epoch_event)
    let mut vt_ep = StubVt::new(40, 4);
    vt_ep.next_events = vec![
        VtEvent::Osc133 { payload: "C".into(), cursor_line: 0 },
        VtEvent::EpochChanged,
        VtEvent::Osc133 { payload: "C".into(), cursor_line: 1 },
        VtEvent::Osc133 { payload: "D;0".into(), cursor_line: 1 },
    ];
    let mut s_ep =
        SessionMachine::new(sid(26), vt_ep, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_ep.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_ep.on_pty_output(b"ALT", ts(1), &mut fx);
    fx.clear();
    s_ep.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    s_ep.handle_stream_msg(k1, RequestId::from_raw(2), ClientMsg::CommandOutput { command_id: 2 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. } if *req_id == RequestId::from_raw(1))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: Some(t) }, .. } if *req_id == RequestId::from_raw(2) && t == "1:0:1:39")));

    //    - SYNC_REARM at exact deadline (now == deadline) must NOT hold!
    let mut vt_sd = StubVt::new(40, 4);
    vt_sd.next_events = vec![VtEvent::SyncOutput(true)];
    let mut s_sd =
        SessionMachine::new(sid(27), vt_sd, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_sd.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);
    s_sd.on_pty_output(b"NOOP", ts(100), &mut fx);
    s_sd.poll_timers(ts(100 + SYNC_HOLD_MAX_MS), &mut fx);
    s_sd.on_pty_output(b"SYNC_REARM", ts(100 + SYNC_HOLD_MAX_MS), &mut fx);
    assert!(!s_sd.is_sync_held());

    //    - enqueue_device_response exact boundary (next == dsr_cap succeeds) & rejected when !alive!
    let mut vt_dr = StubVt::new(40, 4);
    vt_dr.next_events = vec![VtEvent::DeviceResponse(b"12345".to_vec())];
    let mut s_dr = SessionMachine::with_pty_write_cap(
        sid(28),
        vt_dr,
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
        10,
    );
    s_dr.on_pty_output(b"NOOP", ts(1), &mut fx);
    assert_eq!(s_dr.pty_write_len(), 5); // exact dsr_cap = 10/2 = 5 succeeds!
    s_dr.on_process_exit(Some(0), None, ts(2), &mut fx);
    assert_eq!(s_dr.pty_write_len(), 0);
    // Feed DeviceResponse after process exit -> must be ignored!
    let mut vt_dr2 = StubVt::new(40, 4);
    vt_dr2.next_events = vec![VtEvent::DeviceResponse(b"ok".to_vec())];
    let mut s_dr2 =
        SessionMachine::new(sid(29), vt_dr2, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_dr2.on_process_exit(Some(0), None, ts(1), &mut fx);
    s_dr2.on_pty_output(b"NOOP", ts(2), &mut fx);
    assert_eq!(s_dr2.pty_write_len(), 0);

    //    - CLEAR_SB alone (no dirty rows, oldest_line advances 0 -> 1) schedules SearchFlush!
    //    - Normal DIRTY_ROW2 flush emits EXACTLY 1 SearchDelta (no spurious [oldest, oldest) delta!)
    //    - Dirty range fully evicted before flush (clamped_from == clamped_to) emits NO second SearchDelta!
    //    - __OOB_SPAN__, __EXACT_LIMIT__, __OVER_LIMIT__ test SearchDelta filtering and MAX_SEARCH_MATCHES cap!
    let mut vt_sf = StubVt::new(40, 4);
    vt_sf.dirty.full_dirty = false;
    let mut s_sf =
        SessionMachine::new(sid(30), vt_sf, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_sf.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_sf.on_pty_output(b"SCROLL", ts(1), &mut fx); // first_active_line=1, scrollback=1, oldest=0
    s_sf.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        ClientMsg::SetSearch { query: "__OOB_SPAN__".into(), case_sensitive: true },
        &mut fx,
    );
    assert_eq!(s_sf.next_deadline(), None);
    // CLEAR_SB sets scrollback=0 -> oldest advances 0 -> 1 with NO dirty rows -> schedules SearchFlush!
    s_sf.on_pty_output(b"CLEAR_SB", ts(10), &mut fx);
    assert_eq!(s_sf.next_deadline(), Some(ts(10 + SEARCH_COALESCE_MS)));
    fx.clear();
    s_sf.poll_timers(ts(10 + SEARCH_COALESCE_MS), &mut fx);
    assert_eq!(
        fx.iter().filter(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::SearchDelta { .. }, .. })).count(),
        1
    );

    // DIRTY_ROW2 on __OOB_SPAN__: retains ONLY the 1 valid span (drops start < from and end == to), emits 1 delta!
    s_sf.on_pty_output(b"DIRTY_ROW2", ts(300), &mut fx);
    fx.clear();
    s_sf.poll_timers(ts(300 + SEARCH_COALESCE_MS), &mut fx);
    let deltas: Vec<_> = fx
        .iter()
        .filter_map(|e| match e {
            SessionEffect::Send { msg: BrokerMsg::SearchDelta { added, truncated, .. }, .. } => {
                Some((added.len(), *truncated))
            }
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec![(1, false)]);

    // __EXACT_LIMIT__ (len == MAX_SEARCH_MATCHES -> truncated == false) vs __OVER_LIMIT__ (len > MAX -> truncated == true)
    s_sf.handle_stream_msg(
        k1,
        RequestId::from_raw(2),
        ClientMsg::SetSearch { query: "__EXACT_LIMIT__".into(), case_sensitive: true },
        &mut fx,
    );
    s_sf.on_pty_output(b"DIRTY_ROW2", ts(600), &mut fx);
    fx.clear();
    s_sf.poll_timers(ts(600 + SEARCH_COALESCE_MS), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::SearchDelta { added, truncated: false, .. }, .. } if added.len() == MAX_SEARCH_MATCHES)));

    s_sf.handle_stream_msg(
        k1,
        RequestId::from_raw(3),
        ClientMsg::SetSearch { query: "__OVER_LIMIT__".into(), case_sensitive: true },
        &mut fx,
    );
    s_sf.on_pty_output(b"DIRTY_ROW2", ts(900), &mut fx);
    fx.clear();
    s_sf.poll_timers(ts(900 + SEARCH_COALESCE_MS), &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { msg: BrokerMsg::SearchDelta { added, truncated: true, .. }, .. } if added.len() == MAX_SEARCH_MATCHES)));

    // Dirty row 0 evicted before SearchFlush -> clamped_from == clamped_to -> only eviction delta emitted, no empty dirty delta!
    s_sf.on_pty_output(b"DIRTY_ROW0", ts(1200), &mut fx); // dirty_range = [1, 2)
    s_sf.on_pty_output(b"EVICT_SB", ts(1201), &mut fx); // first_active=2, scrollback=0, oldest=2!
    // Wait: EVICT_SB sets first_active=2 > 1 so dirty_range expands to [2, 6); instead use a fresh watch where only CLEAR_SB evicts:
    let mut vt_cl = StubVt::new(40, 4);
    vt_cl.dirty.full_dirty = false;
    let mut s_cl =
        SessionMachine::new(sid(31), vt_cl, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_cl.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_cl.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        ClientMsg::SetSearch { query: "row".into(), case_sensitive: true },
        &mut fx,
    );
    s_cl.on_pty_output(b"DIRTY_ROW0", ts(1), &mut fx); // dirty_range = [0, 1)
    s_cl.on_pty_output(b"SCROLL", ts(2), &mut fx); // first_active=1, scrollback=1, oldest=0
    // Flush first, then mark row 0 dirty at line 1, then advance oldest past line 1 via CLEAR_SB after another scroll:
    s_cl.poll_timers(ts(200), &mut fx);
    s_cl.on_pty_output(b"DIRTY_ROW0", ts(300), &mut fx); // dirty_range = [1, 2)
    // Make oldest jump to 2 without expanding dirty_range past 2: first_active=1, scrollback=1 -> SCROLL -> first_active=2, scrollback=2 -> wait, let's test clamp_span & extract_command_output < oldest directly!
    s_cl.on_pty_output(b"SCROLL", ts(301), &mut fx); // first_active=2, scrollback=2
    s_cl.on_pty_output(b"CLEAR_SB", ts(302), &mut fx); // oldest=2, max_line=5, cols=40 (max_col=39)!

    // Command started at line 0 before scrollback eviction -> start_line < oldest (0 < 2) -> None!
    // Command started at line 3 -> start_line > oldest (3 > 2) -> Some!
    let mut vt_co = StubVt::new(40, 4);
    vt_co.next_events = vec![
        VtEvent::Osc133 { payload: "C".into(), cursor_line: 0 },
        VtEvent::Osc133 { payload: "D;0".into(), cursor_line: 0 },
        VtEvent::Osc133 { payload: "C".into(), cursor_line: 3 },
        VtEvent::Osc133 { payload: "D;0".into(), cursor_line: 3 },
    ];
    let mut s_co =
        SessionMachine::new(sid(32), vt_co, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_co.attach_stream(k1, SubFlags::from_bits(SubFlags::EVENTS).unwrap(), None, &mut fx);
    s_co.on_pty_output(b"SCROLL", ts(1), &mut fx);
    s_co.on_pty_output(b"SCROLL", ts(2), &mut fx);
    s_co.on_pty_output(b"CLEAR_SB", ts(3), &mut fx); // oldest = 2, total_lines = 6
    fx.clear();
    s_co.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::CommandOutput { command_id: 1 }, &mut fx);
    s_co.handle_stream_msg(k1, RequestId::from_raw(2), ClientMsg::CommandOutput { command_id: 2 }, &mut fx);
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: None }, .. } if *req_id == RequestId::from_raw(1))));
    assert!(fx.iter().any(|e| matches!(e, SessionEffect::Send { req_id, msg: BrokerMsg::CommandOutputReply { text: Some(t) }, .. } if *req_id == RequestId::from_raw(2) && t == "3:0:3:39")));

    // 9. Exhaustive clamp_span boundary table (oldest = 2, max_line = 5, max_col = 39)
    let ep_cl = s_cl.vt().meta().epoch;
    let check_text_of = |s: &mut SessionMachine<StubVt>, span: CellSpan, expected: &str| {
        let mut local_fx = Vec::new();
        s.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::TextOf { epoch: ep_cl, span }, &mut local_fx);
        let got = local_fx
            .into_iter()
            .find_map(|e| match e {
                SessionEffect::Send { msg: BrokerMsg::TextReply { text }, .. } => Some(text),
                _ => None,
            })
            .expect("TextReply");
        assert_eq!(got, expected, "clamp_span mismatch for {span:?}");
    };
    // end.line < oldest (0 < 2, strictly less than oldest and != oldest) -> ""
    check_text_of(&mut s_cl, CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 0, col: 5 } }, "");
    // end.line == oldest (2 == 2) -> valid!
    check_text_of(
        &mut s_cl,
        CellSpan { start: CellPos { line: 2, col: 0 }, end: CellPos { line: 2, col: 0 } },
        "2:0:2:0",
    );
    // start.line > max_line (7 > 5) -> ""
    check_text_of(&mut s_cl, CellSpan { start: CellPos { line: 7, col: 0 }, end: CellPos { line: 8, col: 0 } }, "");
    // start.line == max_line (5 == 5) -> valid!
    check_text_of(
        &mut s_cl,
        CellSpan { start: CellPos { line: 5, col: 2 }, end: CellPos { line: 5, col: 5 } },
        "5:2:5:5",
    );
    // Straddling both oldest and max_line: start_col -> 0, end_col -> max_col (39)
    check_text_of(
        &mut s_cl,
        CellSpan { start: CellPos { line: 0, col: 10 }, end: CellPos { line: 10, col: 10 } },
        "2:0:5:39",
    );
    // Col > max_col clamped to 39
    check_text_of(
        &mut s_cl,
        CellSpan { start: CellPos { line: 3, col: 99 }, end: CellPos { line: 3, col: 99 } },
        "3:39:3:39",
    );
    // Inverted multi-line span (4..3) -> ""
    check_text_of(&mut s_cl, CellSpan { start: CellPos { line: 4, col: 0 }, end: CellPos { line: 3, col: 5 } }, "");
    // Inverted single-line span (3:10..3:5) -> ""
    check_text_of(&mut s_cl, CellSpan { start: CellPos { line: 3, col: 10 }, end: CellPos { line: 3, col: 5 } }, "");
    // Multi-line non-inverted span where start.col > end.col (3:10..4:5) -> valid!
    check_text_of(
        &mut s_cl,
        CellSpan { start: CellPos { line: 3, col: 10 }, end: CellPos { line: 4, col: 5 } },
        "3:10:4:5",
    );
}

#[test]
fn status_clear_per_stream_held_buttons_and_resize_side_effects() {
    use crate::status::StatusTable;
    use crate::vt::ProgramStatusUpdate;

    // 1. StatusTable Clear + ingest_update invariant enforcement (progress <= 100, non-empty optional strings)
    let mut table = StatusTable::new();
    let mut sfx = Vec::new();
    table.ingest("state=blocked;kind=permission;progress=50;app=claude", ts(10), &mut sfx);
    assert!(table.root().is_some());
    table.ingest("state=clear", ts(20), &mut sfx);
    assert!(table.root().is_none());

    table.ingest_update(
        ProgramStatusUpdate::Set {
            id: "sub".into(),
            state: StatusState::Working,
            kind: Some(BlockedKind::Auth), // stripped outside Blocked
            progress: Some(150),           // rejected > 100
            app: Some(String::new()),      // empty stripped
            title: Some("ok;with=sep".into()),
            message: Some("msg;state=done".into()),
        },
        ts(30),
        &mut sfx,
    );
    let sub = table.get("sub").unwrap();
    assert_eq!(sub.kind, None);
    assert_eq!(sub.progress, None);
    assert_eq!(sub.app, None);
    assert_eq!(sub.title.as_deref(), Some("ok;with=sep"));
    assert_eq!(sub.message.as_deref(), Some("msg;state=done"));
    table.ingest_update(ProgramStatusUpdate::Clear { id: "sub".into() }, ts(40), &mut sfx);
    assert!(table.get("sub").is_none());

    // 2. Per-stream held_buttons isolated across streams and cleared on blur & detach
    let mut sess = SessionMachine::new(
        sid(40),
        StubVt::new(40, 4),
        ResizePolicy::LatestActive,
        MarksMode::Claimed,
        CellPx::default(),
    );
    let k1 = (conn(1), NonZeroStreamId::new(1).unwrap());
    let k2 = (conn(2), NonZeroStreamId::new(1).unwrap());
    let mut fx = Vec::new();
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);
    sess.attach_stream(k2, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);

    let press0 = PointerInput {
        action: PointerAction::Press,
        button: 0,
        modifiers: 0,
        x_px: 0,
        y_px: 0,
        cell_px: CellPx::default(),
    };
    let move0 = PointerInput {
        action: PointerAction::Motion,
        button: 0,
        modifiers: 0,
        x_px: 8,
        y_px: 0,
        cell_px: CellPx::default(),
    };
    sess.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::Pointer(press0), &mut fx);
    assert_eq!(sess.drain_pty_write(64), b"M1");
    // Stream k2 still has held_buttons == 0!
    sess.handle_stream_msg(k2, RequestId::from_raw(2), ClientMsg::Pointer(move0), &mut fx);
    assert_eq!(sess.drain_pty_write(64), b"M0");
    // Blur on k1 clears k1's held_buttons!
    sess.handle_stream_msg(k1, RequestId::from_raw(3), ClientMsg::Focus { focused: false }, &mut fx);
    let _ = sess.drain_pty_write(64);
    sess.handle_stream_msg(k1, RequestId::from_raw(4), ClientMsg::Pointer(move0), &mut fx);
    assert_eq!(sess.drain_pty_write(64), b"M0");
    // Detach removes held_buttons entry cleanly
    sess.handle_stream_msg(k1, RequestId::from_raw(5), ClientMsg::Pointer(press0), &mut fx);
    let _ = sess.drain_pty_write(64);
    sess.detach_stream(k1, &mut fx);
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);
    sess.handle_stream_msg(k1, RequestId::from_raw(6), ClientMsg::Pointer(move0), &mut fx);
    assert_eq!(sess.drain_pty_write(64), b"M0");

    // 3. Resize drains VtEvent::SyncOutput(false) and VtEvent::DeviceResponse immediately!
    let mut vt_rs = StubVt::new(40, 4);
    vt_rs.next_events = vec![VtEvent::SyncOutput(true)];
    let mut s_rs =
        SessionMachine::new(sid(41), vt_rs, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    s_rs.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);
    s_rs.on_pty_output(b"HOLD", ts(100), &mut fx);
    assert!(s_rs.is_sync_held());
    fx.clear();
    s_rs.handle_stream_msg(
        k1,
        RequestId::from_raw(7),
        ClientMsg::ViewportHint(ViewportHint {
            size: GridSize::new(50, 10).unwrap(),
            cell_px: CellPx::default(),
            visible: true,
            focused: true,
        }),
        &mut fx,
    );
    assert!(!s_rs.is_sync_held(), "resize emitting SyncOutput(false) must release sync hold");
    assert_eq!(s_rs.drain_pty_write(64), b"\x1b[48;10;20t");
}

#[test]
fn session_machine_runtime_accessors_and_nonblocking_pty_peek_consume() {
    let key = MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap();
    let cell_px = CellPx { width: 9, height: 18 };
    let mut sess =
        SessionMachine::new(sid(50), StubVt::new(40, 4), ResizePolicy::LatestActive, MarksMode::Keyed(key), cell_px);
    assert_eq!(sess.current_cell_px(), cell_px);
    assert_eq!(sess.marks_key(), Some(key));
    assert_eq!(sess.marks_last_seq(), 0);
    assert_eq!(sess.marks_refused(), 0);
    assert_eq!(sess.cwd(), None);
    assert!(sess.commands().is_empty());

    // Claimed session returns None for marks_key
    let claimed =
        SessionMachine::new(sid(51), StubVt::new(40, 4), ResizePolicy::LatestActive, MarksMode::Claimed, cell_px);
    assert_eq!(claimed.marks_key(), None);

    // Feed a vouched start + finish + a forged mark via vt_mut
    let mut fx = Vec::new();
    sess.vt_mut().next_events = vec![
        VtEvent::Osc633 { payload: format!("V;S;1;{};echo hi", key.as_str()), cursor_line: 0 },
        VtEvent::Osc633 { payload: format!("V;F;1;{};0;/home/me", key.as_str()), cursor_line: 1 },
        VtEvent::Osc633 { payload: String::from("V;S;2;ffffffffffffffffffffffffffffffff;bad"), cursor_line: 1 },
    ];
    sess.on_pty_output(b"run", ts(10), &mut fx);
    assert_eq!(sess.marks_last_seq(), 1);
    assert_eq!(sess.marks_refused(), 1);
    assert_eq!(sess.cwd(), Some("/home/me"));
    assert_eq!(sess.commands().len(), 1);

    // mark_all_views_full_sync forces a clean subscriber to emit a full_sync frame
    let k1 = (conn(1), nz_stream(1));
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);
    assert!(sess.try_publish_view(k1, |_| true));
    assert!(!sess.try_publish_view(k1, |_| true));
    sess.mark_all_views_full_sync();
    let mut got_full = false;
    assert!(sess.try_publish_view(k1, |f| {
        got_full = f.full_sync;
        true
    }));
    assert!(got_full);

    // peek_pty_write + consume_pty_write handle partial writes and VecDeque wraparound cleanly
    sess.handle_stream_msg(k1, RequestId::from_raw(1), ClientMsg::Write { data: b"abcdefgh".to_vec() }, &mut fx);
    let mut buf = Vec::new();
    sess.peek_pty_write(3, &mut buf);
    assert_eq!(buf, b"abc");
    sess.consume_pty_write(3);
    // Push more to exercise VecDeque wraparound across head + tail slices
    sess.handle_stream_msg(k1, RequestId::from_raw(2), ClientMsg::Write { data: b"ijkl".to_vec() }, &mut fx);
    sess.peek_pty_write(20, &mut buf);
    assert_eq!(buf, b"defghijkl");
    sess.consume_pty_write(9);
    assert_eq!(sess.pty_write_len(), 0);

    // sync_vt_geometry updates cached current_size & current_cell_px after external VT replacement
    let new_sz = GridSize::new(100, 30).unwrap();
    let new_px = CellPx { width: 10, height: 20 };
    let mut evs = Vec::new();
    sess.vt_mut().resize(new_sz, new_px, &mut evs);
    sess.sync_vt_geometry(new_px);
    assert_eq!(sess.current_size(), new_sz);
    assert_eq!(sess.current_cell_px(), new_px);
}

#[test]
fn review_mutants_rows_budget_clamp_rect_and_logical_line_expansion() {
    use crate::session::{clamp_rect_span, estimate_packed_row_bytes, truncate_rows_to_budget};

    // 1. estimate_packed_row_bytes & truncate_rows_to_budget exact boundaries
    let r0 = PackedRow {
        line: 0,
        flags: 0,
        text: "hello-world".into(),
        runs: Vec::new(),
        styles: Vec::new(),
        links: Vec::new(),
        pieces: Vec::new(),
    };
    let sz0 = estimate_packed_row_bytes(&r0);
    assert!(sz0 > 10);
    // Keep at least 1 row even when row 0 > budget; keep 2 rows when exact == budget; drop 3rd when +1 > budget
    let mut rows = vec![r0.clone(), r0.clone(), r0.clone()];
    truncate_rows_to_budget(&mut rows, 1);
    assert_eq!(rows.len(), 1);
    let mut rows2 = vec![r0.clone(), r0.clone(), r0.clone()];
    truncate_rows_to_budget(&mut rows2, 32 + sz0 * 2);
    assert_eq!(rows2.len(), 2);
    let mut rows3 = vec![r0.clone(), r0.clone(), r0.clone()];
    truncate_rows_to_budget(&mut rows3, 32 + sz0 * 2 - 1);
    assert_eq!(rows3.len(), 1);

    // 2. clamp_rect_span boundaries (cols=10 => max_col=9, total_lines=4 => max_line=3)
    let meta = VtSnapshotMeta {
        epoch: 1,
        size: GridSize::new(10, 4).unwrap(),
        first_active_line: 0,
        scrollback_lines: 0,
        cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
        modes: ViewModes::default(),
        images_gen: 0,
    };
    // Exact max_col boundary (9) is valid; max_col + 1 (10) returns None
    let at_edge =
        clamp_rect_span(CellSpan { start: CellPos { line: 1, col: 9 }, end: CellPos { line: 2, col: 9 } }, &meta);
    assert_eq!(at_edge, Some(CellSpan { start: CellPos { line: 1, col: 9 }, end: CellPos { line: 2, col: 9 } }));
    let past_edge =
        clamp_rect_span(CellSpan { start: CellPos { line: 1, col: 10 }, end: CellPos { line: 2, col: 12 } }, &meta);
    assert_eq!(past_edge, None);
    // Reversed columns normalized and clamped to max_col; reversed lines -> None
    let swapped =
        clamp_rect_span(CellSpan { start: CellPos { line: 1, col: 15 }, end: CellPos { line: 1, col: 3 } }, &meta);
    assert_eq!(swapped, Some(CellSpan { start: CellPos { line: 1, col: 3 }, end: CellPos { line: 1, col: 9 } }));
    assert_eq!(
        clamp_rect_span(CellSpan { start: CellPos { line: 3, col: 0 }, end: CellPos { line: 1, col: 5 } }, &meta),
        None
    );

    // 3. Soft-wrapped logical line expansion in SearchDelta & text waiters
    let mut vt = StubVt::new(10, 4);
    vt.wrapped_logical = Some((1, 2));
    let mut s = SessionMachine::new(sid(40), vt, ResizePolicy::LatestActive, MarksMode::Claimed, CellPx::default());
    let k1 = (ConnId::new(1).unwrap(), NonZeroStreamId::new(1).unwrap());
    let mut fx = Vec::new();
    s.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW).unwrap(), None, &mut fx);
    s.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        ClientMsg::SetSearch { query: "foo".into(), case_sensitive: false },
        &mut fx,
    );
    // Dirtying only row 1 must expand the search delta range to [1, 3) (covering wrapped row 2)!
    s.vt_mut().cursor.row = 1;
    s.vt_mut().dirty = DirtyViewport { scroll_delta: 0, dirty_rows: vec![1], full_dirty: false };
    s.on_pty_output(b"NOOP", ts(10), &mut fx);
    fx.clear();
    s.poll_timers(ts(200), &mut fx);
    assert_eq!(s.vt().last_search_range.get(), (1, 3));
    // Dirtying only row 2 must expand backward to [1, 3) as well!
    s.vt_mut().cursor.row = 2;
    s.vt_mut().dirty = DirtyViewport { scroll_delta: 0, dirty_rows: vec![2], full_dirty: false };
    s.on_pty_output(b"NOOP", ts(300), &mut fx);
    fx.clear();
    s.poll_timers(ts(500), &mut fx);
    assert_eq!(s.vt().last_search_range.get(), (1, 3));
    // Empty range (lo == hi) must return (lo, lo) untouched without underflowing `hi - 1`
    assert_eq!(s.expand_to_logical_lines(2, 2, 0, 4), (2, 2));
    assert_eq!(s.expand_to_logical_lines(1, 2, 0, 4), (1, 3));
    assert_eq!(s.expand_to_logical_lines(2, 3, 0, 4), (1, 3));

    // 4. budget_frame_rows keeps at least 1 row even when budget < row0, keeps exact-boundary rows,
    //    and returns deferred viewport row offsets when over budget!
    let make_row = |line: u64, n: usize| PackedRow {
        line,
        flags: 0,
        text: "a".repeat(n),
        runs: Vec::new(),
        styles: Vec::new(),
        links: Vec::new(),
        pieces: Vec::new(),
    };
    let r10 = make_row(10, 40);
    let r11 = make_row(11, 40);
    let r13 = make_row(13, 40);
    let exact_two =
        32 + crate::session::estimate_packed_row_bytes(&r10) + crate::session::estimate_packed_row_bytes(&r11);
    let mut f_rows_tiny = vec![r10.clone(), r11.clone()];
    assert_eq!(crate::session::budget_frame_rows(&mut f_rows_tiny, 10, 1), vec![1]);
    assert_eq!(f_rows_tiny.len(), 1);
    let mut f_rows_exact = vec![r10.clone(), r11.clone(), r13.clone()];
    assert_eq!(crate::session::budget_frame_rows(&mut f_rows_exact, 10, exact_two), vec![3]);
    assert_eq!(f_rows_exact.len(), 2);
    let mut f_rows_over = vec![r10, r11, r13];
    assert_eq!(crate::session::budget_frame_rows(&mut f_rows_over, 10, exact_two - 1), vec![1, 3]);
    assert_eq!(f_rows_over.len(), 1);
}
