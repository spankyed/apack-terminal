//! Pure-Rust deterministic [`VtEngine`] implementation for unit tests and chaos simulation.
//!
//! Supports streaming byte-by-byte escape parsing across arbitrary chunk boundaries (OSC 0/2 title,
//! OSC 7 cwd, OSC 133/633 shell marks, OSC 7501 program status, CSI `?2026h/l` DEC 2026 sync
//! output, CSI `?1049h/l` alt screen, CSI `6n` DSR reply, BEL, CR, LF scrolling, and scrollback).

use std::collections::BTreeSet;

use apack_core::vt::{CompressStep, DirtyViewport, PasteEncode, VtEngine, VtEvent, VtGenerations, VtSnapshotMeta};
use apack_proto::types::{
    CellPos, CellPx, CellSpan, CursorShape, CursorState, GridSize, ImagePlacement, KeyInput, PackedRow, PackedRun,
    PaletteOverrides, PointerInput, ViewModes,
};

#[derive(Debug, Clone)]
enum ParseState {
    Ground,
    Esc,
    Csi(Vec<u8>),
    Osc(Vec<u8>),
    OscEsc(Vec<u8>),
}

/// Deterministic, dependency-free pure-Rust VT engine implementing [`VtEngine`].
#[derive(Debug)]
pub struct MockVt {
    size: GridSize,
    scrollback_limit: usize,
    epoch: u64,
    scrollback: Vec<String>,
    viewport: Vec<String>,
    cursor: CursorState,
    default_cursor_shape: CursorShape,
    default_cursor_blink: bool,
    modes: ViewModes,
    gens: VtGenerations,
    dirty_rows: BTreeSet<u16>,
    scroll_delta: i32,
    full_dirty: bool,
    parse: ParseState,
    compressed_lines: usize,
}

impl MockVt {
    /// Creates a new blank `MockVt` with grid `size` and bounded `scrollback_limit`.
    pub fn new(size: GridSize, scrollback_limit: usize) -> Self {
        let rows = usize::from(size.rows.get());
        Self {
            size,
            scrollback_limit,
            epoch: 1,
            scrollback: Vec::new(),
            viewport: vec![String::new(); rows],
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            default_cursor_shape: CursorShape::Block,
            default_cursor_blink: false,
            modes: ViewModes::default(),
            gens: VtGenerations::default(),
            dirty_rows: BTreeSet::new(),
            scroll_delta: 0,
            full_dirty: true,
            parse: ParseState::Ground,
            compressed_lines: 0,
        }
    }

    /// Returns the plain text of absolute line `abs_line` (scrollback followed by viewport).
    pub fn line_text(&self, abs_line: u64) -> Option<&str> {
        let sb = self.scrollback.len() as u64;
        if abs_line < sb {
            self.scrollback.get(abs_line as usize).map(String::as_str)
        } else {
            self.viewport.get((abs_line - sb) as usize).map(String::as_str)
        }
    }

    fn put_char(&mut self, ch: char) {
        let cols = usize::from(self.size.cols.get());
        let row_idx = usize::from(self.cursor.row);
        if usize::from(self.cursor.col) >= cols {
            self.newline();
        }
        let cur_row = usize::from(self.cursor.row);
        let cur_col = usize::from(self.cursor.col);
        let line = &mut self.viewport[cur_row];
        let mut chars: Vec<char> = line.chars().collect();
        if chars.len() <= cur_col {
            chars.resize(cur_col + 1, ' ');
        }
        chars[cur_col] = ch;
        *line = chars.into_iter().collect();
        self.dirty_rows.insert(cur_row as u16);
        self.gens.grid += 1;
        if cur_col + 1 < cols {
            self.cursor.col += 1;
        } else {
            self.cursor.col = cols as u16;
        }
        let _ = row_idx;
    }

    fn newline(&mut self) {
        let rows = self.size.rows.get();
        self.cursor.col = 0;
        if self.cursor.row + 1 < rows {
            self.cursor.row += 1;
        } else {
            let evicted = self.viewport.remove(0);
            if !self.modes.alt_screen && self.scrollback_limit > 0 {
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.remove(0);
                    self.compressed_lines = self.compressed_lines.saturating_sub(1);
                }
                self.scrollback.push(evicted);
            }
            self.viewport.push(String::new());
            self.scroll_delta = self.scroll_delta.saturating_add(1);
            // Shift previously dirty row indices up by 1 and mark newly exposed bottom row dirty.
            let mut shifted = BTreeSet::new();
            for &r in &self.dirty_rows {
                if r > 0 {
                    shifted.insert(r - 1);
                }
            }
            shifted.insert(rows - 1);
            self.dirty_rows = shifted;
            self.gens.grid += 1;
        }
    }

    fn handle_csi(&mut self, params: &[u8], final_byte: u8, events: &mut Vec<VtEvent>) {
        match (params, final_byte) {
            (b"?2026", b'h') => events.push(VtEvent::SyncOutput(true)),
            (b"?2026", b'l') => events.push(VtEvent::SyncOutput(false)),
            (b"?1049", b'h') if !self.modes.alt_screen => {
                self.modes.alt_screen = true;
                self.epoch += 1;
                self.gens.modes += 1;
                self.viewport.fill_with(String::new);
                self.cursor.col = 0;
                self.cursor.row = 0;
                self.full_dirty = true;
            }
            (b"?1049", b'l') if self.modes.alt_screen => {
                self.modes.alt_screen = false;
                self.epoch += 1;
                self.gens.modes += 1;
                self.viewport.fill_with(String::new);
                self.cursor.col = 0;
                self.cursor.row = 0;
                self.full_dirty = true;
            }
            (b"6", b'n') => {
                let r = self.cursor.row + 1;
                let c = self.cursor.col + 1;
                events.push(VtEvent::DeviceResponse(format!("\x1b[{r};{c}R").into_bytes()));
            }
            _ => {}
        }
    }

    fn handle_osc(&mut self, raw: &[u8], events: &mut Vec<VtEvent>) {
        let Ok(text) = core::str::from_utf8(raw) else {
            return;
        };
        let Some((code, body)) = text.split_once(';') else {
            return;
        };
        match code {
            "0" | "2" => events.push(VtEvent::Title(String::from(body))),
            "7" => events.push(VtEvent::Cwd(String::from(body))),
            "133" => events.push(VtEvent::Osc133(String::from(body))),
            "633" => events.push(VtEvent::Osc633(String::from(body))),
            "7501" => events.push(VtEvent::Osc7501(String::from(body))),
            _ => {}
        }
    }

    fn pack_row(&self, abs_line: u64, text: &str) -> PackedRow {
        let trimmed = text.trim_end_matches(' ');
        let runs = if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![PackedRun {
                col_start: 0,
                col_len: trimmed.chars().count().min(usize::from(u16::MAX)) as u16,
                byte_offset: 0,
                byte_len: trimmed.len() as u32,
                style_idx: 0,
                link_idx: 0,
                flags: 0,
            }]
        };
        PackedRow {
            line: abs_line,
            flags: 0,
            text: String::from(trimmed),
            runs,
            styles: Vec::new(),
            links: Vec::new(),
            pieces: Vec::new(),
        }
    }
}

impl VtEngine for MockVt {
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) {
        for &b in bytes {
            let state = core::mem::replace(&mut self.parse, ParseState::Ground);
            match state {
                ParseState::Ground => match b {
                    0x1b => self.parse = ParseState::Esc,
                    0x07 => events.push(VtEvent::Bell),
                    b'\r' => self.cursor.col = 0,
                    b'\n' => self.newline(),
                    0x20..=0x7E => self.put_char(char::from(b)),
                    _ => {}
                },
                ParseState::Esc => match b {
                    b'[' => self.parse = ParseState::Csi(Vec::new()),
                    b']' => self.parse = ParseState::Osc(Vec::new()),
                    _ => self.parse = ParseState::Ground,
                },
                ParseState::Csi(mut buf) => {
                    if (0x40..=0x7E).contains(&b) {
                        self.handle_csi(&buf, b, events);
                        self.parse = ParseState::Ground;
                    } else if buf.len() < 64 {
                        buf.push(b);
                        self.parse = ParseState::Csi(buf);
                    }
                }
                ParseState::Osc(mut buf) => match b {
                    0x07 => {
                        self.handle_osc(&buf, events);
                        self.parse = ParseState::Ground;
                    }
                    0x1b => self.parse = ParseState::OscEsc(buf),
                    _ if buf.len() < 4_096 => {
                        buf.push(b);
                        self.parse = ParseState::Osc(buf);
                    }
                    _ => self.parse = ParseState::Ground,
                },
                ParseState::OscEsc(buf) => {
                    if b == b'\\' {
                        self.handle_osc(&buf, events);
                    }
                    self.parse = ParseState::Ground;
                }
            }
        }
    }

    fn resize(&mut self, size: GridSize, _cell_px: CellPx) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.epoch += 1;
        let rows = usize::from(size.rows.get());
        self.viewport.resize(rows, String::new());
        self.cursor.row = self.cursor.row.min(size.rows.get() - 1);
        self.cursor.col = self.cursor.col.min(size.cols.get() - 1);
        self.full_dirty = true;
        self.dirty_rows.clear();
        self.scroll_delta = 0;
        self.gens.grid += 1;
    }

    fn meta(&self) -> VtSnapshotMeta {
        VtSnapshotMeta {
            epoch: self.epoch,
            size: self.size,
            scrollback_lines: self.scrollback.len() as u64,
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
        let out = DirtyViewport {
            scroll_delta: self.scroll_delta,
            dirty_rows: self.dirty_rows.iter().copied().collect(),
            full_dirty: self.full_dirty,
        };
        self.scroll_delta = 0;
        self.dirty_rows.clear();
        self.full_dirty = false;
        out
    }

    fn extract_viewport_rows(&self, rows: &[u16], out: &mut Vec<PackedRow>) {
        let sb = self.scrollback.len() as u64;
        for &r in rows {
            if let Some(text) = self.viewport.get(usize::from(r)) {
                out.push(self.pack_row(sb + u64::from(r), text));
            }
        }
    }

    fn extract_history_rows(&self, from_line: u64, count: u32, out: &mut Vec<PackedRow>) {
        for offset in 0..u64::from(count) {
            let abs = from_line.saturating_add(offset);
            let Some(text) = self.line_text(abs) else {
                break;
            };
            out.push(self.pack_row(abs, text));
        }
    }

    fn word_at(&self, pos: CellPos, delimiters: &str) -> Option<CellSpan> {
        let text = self.line_text(pos.line)?;
        let chars: Vec<char> = text.chars().collect();
        let col = usize::from(pos.col);
        if col >= chars.len() {
            return None;
        }
        let is_delim = |c: char| delimiters.contains(c);
        if is_delim(chars[col]) {
            return Some(CellSpan { start: pos, end: pos });
        }
        let mut start = col;
        while start > 0 && !is_delim(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < chars.len() && !is_delim(chars[end + 1]) {
            end += 1;
        }
        Some(CellSpan {
            start: CellPos { line: pos.line, col: start as u16 },
            end: CellPos { line: pos.line, col: end as u16 },
        })
    }

    fn line_at(&self, line: u64) -> Option<CellSpan> {
        let text = self.line_text(line)?;
        let end_col = text.chars().count().saturating_sub(1) as u16;
        Some(CellSpan { start: CellPos { line, col: 0 }, end: CellPos { line, col: end_col } })
    }

    fn text_of(&self, span: CellSpan) -> String {
        let mut out = String::new();
        for line in span.start.line..=span.end.line {
            if line > span.start.line {
                out.push('\n');
            }
            if let Some(text) = self.line_text(line) {
                let chars: Vec<char> = text.chars().collect();
                let lo = if line == span.start.line { usize::from(span.start.col) } else { 0 };
                let hi = if line == span.end.line {
                    usize::from(span.end.col).min(chars.len().saturating_sub(1))
                } else {
                    chars.len().saturating_sub(1)
                };
                if lo < chars.len() && lo <= hi {
                    for &c in &chars[lo..=hi] {
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    fn text_of_rect(&self, span: CellSpan) -> String {
        let lo = usize::from(span.start.col.min(span.end.col));
        let hi = usize::from(span.start.col.max(span.end.col));
        let mut out = String::new();
        for line in span.start.line..=span.end.line {
            if line > span.start.line {
                out.push('\n');
            }
            if let Some(text) = self.line_text(line) {
                let chars: Vec<char> = text.chars().collect();
                if lo < chars.len() {
                    let end = hi.min(chars.len() - 1);
                    for &c in &chars[lo..=end] {
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    fn search_lines(
        &self,
        from_line: u64,
        to_line: u64,
        needle: &str,
        case_sensitive: bool,
        limit: usize,
        out: &mut Vec<CellSpan>,
    ) -> bool {
        if needle.is_empty() || from_line >= to_line {
            return false;
        }
        let needle_cmp = if case_sensitive { String::from(needle) } else { needle.to_lowercase() };
        let needle_chars = needle_cmp.chars().count();
        for line in from_line..to_line {
            let Some(raw) = self.line_text(line) else {
                break;
            };
            let hay = if case_sensitive { String::from(raw) } else { raw.to_lowercase() };
            for (byte_idx, _) in hay.match_indices(&needle_cmp) {
                if out.len() >= limit {
                    return true;
                }
                let start_col = hay[..byte_idx].chars().count() as u16;
                let end_col = start_col + (needle_chars.saturating_sub(1) as u16);
                out.push(CellSpan { start: CellPos { line, col: start_col }, end: CellPos { line, col: end_col } });
            }
        }
        false
    }

    fn images(&self) -> Vec<ImagePlacement> {
        Vec::new()
    }

    fn encode_key(&mut self, key: &KeyInput, out: &mut Vec<u8>) {
        if !key.text.is_empty() {
            out.extend_from_slice(key.text.as_bytes());
        } else if key.code == "Enter" {
            out.push(b'\r');
        }
    }

    fn encode_pointer(&mut self, ev: &PointerInput, out: &mut Vec<u8>) {
        if self.modes.mouse {
            out.extend_from_slice(format!("\x1b[<0;{};{}M", ev.x_px, ev.y_px).as_bytes());
        }
    }

    fn encode_focus(&mut self, focused: bool, out: &mut Vec<u8>) {
        out.extend_from_slice(if focused { b"\x1b[I" } else { b"\x1b[O" });
    }

    fn encode_paste(&mut self, text: &str, allow_unsafe: bool) -> PasteEncode {
        if !allow_unsafe && (text.contains('\n') || text.contains('\r')) {
            return PasteEncode::Unsafe;
        }
        PasteEncode::Bytes(text.as_bytes().to_vec())
    }

    fn set_cursor_defaults(&mut self, shape: CursorShape, blinking: bool) {
        self.default_cursor_shape = shape;
        self.default_cursor_blink = blinking;
        self.cursor.shape = shape;
        self.cursor.blinking = blinking;
    }

    fn compress_step(&mut self, max_pages: usize) -> CompressStep {
        let remaining = self.scrollback.len().saturating_sub(self.compressed_lines);
        if remaining == 0 {
            return CompressStep::Done;
        }
        let step = remaining.min(max_pages.max(1));
        self.compressed_lines += step;
        if self.compressed_lines >= self.scrollback.len() { CompressStep::Done } else { CompressStep::More }
    }

    fn scrollback_bytes(&self) -> u64 {
        self.scrollback.iter().map(|s| s.len() as u64).sum()
    }
}
