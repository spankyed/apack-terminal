//! [`apack_core::vt::VtEngine`] implementation, batched text extraction, and bounded search.

use apack_core::vt::{CompressStep, DirtyViewport, PasteEncode, VtEngine, VtEvent, VtGenerations, VtSnapshotMeta};
use apack_proto::types::{
    CellPos, CellPx, CellSpan, CursorShape, GridSize, ImagePlacement, KeyInput, PackedRow, PaletteOverrides,
    PointerInput,
};
use ghostty_vt_rs_sys as sys;

use crate::MAX_IMAGE_PLACEMENTS;
use crate::input::{encode_focus, encode_paste};
use crate::placeholders::PLACEHOLDER;
use crate::rows::extract_row;

use super::GhosttyEngine;
use super::ffi_queries::{
    checked_image_placement, query_graphics, query_image_generation, query_scrollback_rows, read_grapheme_codepoints,
    read_placement_u32, resolve_drawn_pixel_size, selection_matches_exact, viewport_row_has_placeholder,
};

/// Hard cap on `ghostty_search_get(MATCHES)` allocation (~3 MiB) to prevent OOM on dense single-page bursts.
const MAX_SEARCH_ALLOC: usize = 65_536;
const SEARCH_CANDIDATES: usize = 20_000;
/// Maximum physical rows formatted in a single `libghostty-vt` formatter call so a 100k-row wrapped line cannot OOM.
const MAX_FORMAT_BATCH_ROWS: u64 = 512;

impl VtEngine for GhosttyEngine {
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) {
        let _ = self.feed_inner(bytes, false, events);
    }

    fn resize(&mut self, size: GridSize, cell_px: CellPx, events: &mut Vec<VtEvent>) {
        let grid_changed = size != self.size;
        if !grid_changed && cell_px == self.cell_px {
            return;
        }
        self.size = size;
        self.cell_px = cell_px;
        self.callbacks.with_mut(|cb| {
            cb.cols = size.cols.get();
            cb.rows = size.rows.get();
            cb.cell_px = cell_px;
        });
        // SAFETY: `self.handle` is live for `&mut self`; may invoke `cb_write_pty` (mode 2048) or `cb_render_hold`.
        let _ = unsafe {
            sys::ghostty_terminal_resize(
                self.handle,
                size.cols.get(),
                size.rows.get(),
                u32::from(cell_px.width),
                u32::from(cell_px.height),
            )
        };
        let pending = self.callbacks.with_mut(|cb| std::mem::take(&mut cb.events));
        events.extend(pending);
        if grid_changed {
            self.epoch += 1;
            self.scrollback_lines = query_scrollback_rows(self.handle);
            self.first_active_line = if self.alt_screen { 0 } else { self.scrollback_lines };
            let first = self.first_active_line;
            self.callbacks.with_mut(|cb| cb.first_active_line = first);
            self.full_dirty = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            self.reset_active_top_mark();
            self.refresh_render_state(true, 0);
        } else {
            // Cell pixel geometry changed without grid reflow: recompute placeholder pieces and
            // anchored image cell spans by dirtying placeholder rows and bumping `gens.images`.
            let rows_count = usize::from(self.size.rows.get());
            let mut any_placeholder = false;
            for r in 0..rows_count {
                if viewport_row_has_placeholder(self.handle, r as u32) {
                    self.dirty_rows.insert(r as u16);
                    self.gens.grid += 1;
                    any_placeholder = true;
                }
            }
            if any_placeholder || self.raw_images_gen != 0 {
                self.gens.images = self.gens.images.saturating_add(1);
            }
        }
    }

    fn meta(&self) -> VtSnapshotMeta {
        VtSnapshotMeta {
            epoch: self.epoch,
            size: self.size,
            first_active_line: self.first_active_line,
            scrollback_lines: self.scrollback_lines,
            cursor: self.cursor,
            modes: self.modes,
            images_gen: self.gens.images,
        }
    }

    fn generations(&self) -> VtGenerations {
        self.gens
    }

    fn palette_overrides(&self) -> PaletteOverrides {
        self.palette.clone()
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
        let cols = self.size.cols.get();
        let mut resolver = |img_id, pid| self.resolve_virtual(img_id, pid);
        for &r in rows {
            if r >= self.size.rows.get() {
                continue;
            }
            let abs = self.first_active_line.saturating_add(u64::from(r));
            if let Some(packed) =
                extract_row(self.handle, sys::PointTag::ACTIVE, u32::from(r), cols, abs, self.cell_px, &mut resolver)
            {
                out.push(packed);
            }
        }
    }

    fn extract_history_rows(&self, from_line: u64, count: u32, out: &mut Vec<PackedRow>) {
        let cols = self.size.cols.get();
        let mut resolver = |img_id, pid| self.resolve_virtual(img_id, pid);
        for offset in 0..u64::from(count) {
            let abs = from_line.saturating_add(offset);
            let Some(screen_y) = self.epoch_to_screen_row(abs) else {
                break;
            };
            if let Some(packed) =
                extract_row(self.handle, sys::PointTag::SCREEN, screen_y, cols, abs, self.cell_px, &mut resolver)
            {
                out.push(packed);
            }
        }
    }

    fn word_at(&self, pos: CellPos, delimiters: &str) -> Option<CellSpan> {
        let grid_ref = self.screen_grid_ref(pos)?;
        let boundary_cps: Vec<u32> = delimiters.chars().map(u32::from).collect();
        let opts = sys::TerminalSelectWordOptions {
            size: std::mem::size_of::<sys::TerminalSelectWordOptions>(),
            ref_: grid_ref,
            boundary_codepoints: if boundary_cps.is_empty() { std::ptr::null() } else { boundary_cps.as_ptr() },
            boundary_codepoints_len: boundary_cps.len(),
        };
        let mut sel = sys::Selection::default();
        // SAFETY: `opts` is sized, `boundary_cps` outlives the call, and `sel` is a valid out-pointer.
        let res = unsafe { sys::ghostty_terminal_select_word(self.handle, &raw const opts, &raw mut sel) };
        (res == sys::Result::SUCCESS).then(|| self.selection_to_span(&sel)).flatten()
    }

    fn line_at(&self, line: u64) -> Option<CellSpan> {
        let grid_ref = self.screen_grid_ref(CellPos { line, col: 0 })?;
        let opts = sys::TerminalSelectLineOptions {
            size: std::mem::size_of::<sys::TerminalSelectLineOptions>(),
            ref_: grid_ref,
            ..Default::default()
        };
        let mut sel = sys::Selection::default();
        // SAFETY: `opts` is sized and `sel` is a valid out-pointer on live `self.handle`.
        let res = unsafe { sys::ghostty_terminal_select_line(self.handle, &raw const opts, &raw mut sel) };
        (res == sys::Result::SUCCESS).then(|| self.selection_to_span(&sel)).flatten()
    }

    fn text_of(&self, span: CellSpan, max_bytes: usize) -> String {
        self.text_of_batched(span, max_bytes, 256)
    }

    fn text_of_rect(&self, span: CellSpan, max_bytes: usize) -> String {
        if max_bytes == 0 {
            return String::new();
        }
        let lo_col = span.start.col.min(span.end.col);
        let hi_col = span.start.col.max(span.end.col).min(self.size.cols.get().saturating_sub(1));
        let mut out = String::new();
        for (i, line) in (span.start.line..=span.end.line).enumerate() {
            if out.len() >= max_bytes {
                break;
            }
            if i > 0 {
                out.push('\n');
            }
            let Some(screen_y) = self.epoch_to_screen_row(line) else {
                continue;
            };
            let point = sys::Point {
                tag: sys::PointTag::SCREEN,
                value: sys::PointValue { coordinate: sys::PointCoordinate { x: lo_col, y: screen_y } },
            };
            let mut grid_ref = sys::GridRef::default();
            // SAFETY: resolve row once per line instead of walking scrollback pages per cell.
            if unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut grid_ref) } != sys::Result::SUCCESS
            {
                continue;
            }
            let mut row_text = String::new();
            for col in lo_col..=hi_col {
                grid_ref.x = col;
                let mut cell: sys::Cell = 0;
                // SAFETY: `grid_ref` names a valid column in the resolved row.
                if unsafe { sys::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut cell) } != sys::Result::SUCCESS {
                    continue;
                }
                let mut wide: sys::CellWide::Type = sys::CellWide::NARROW;
                let mut content: sys::CellContentTag::Type = sys::CellContentTag::CODEPOINT;
                let mut cp: u32 = 0;
                // SAFETY: out-pointer types match `libghostty-vt` CellData contract.
                unsafe {
                    let _ = sys::ghostty_cell_get(cell, sys::CellData::WIDE, (&raw mut wide).cast());
                    let _ = sys::ghostty_cell_get(cell, sys::CellData::CONTENT_TAG, (&raw mut content).cast());
                    let _ = sys::ghostty_cell_get(cell, sys::CellData::CODEPOINT, (&raw mut cp).cast());
                }
                if wide == sys::CellWide::SPACER_TAIL || wide == sys::CellWide::SPACER_HEAD {
                    continue;
                }
                if cp == u32::from(PLACEHOLDER) {
                    row_text.push(' ');
                    continue;
                }
                if content == sys::CellContentTag::CODEPOINT_GRAPHEME {
                    let mut cluster = Vec::new();
                    if read_grapheme_codepoints(&grid_ref, &mut cluster) {
                        row_text.extend(cluster.iter().filter_map(|&c| char::from_u32(c)));
                        continue;
                    }
                }
                row_text.push(char::from_u32(cp).filter(|&c| c != '\0').unwrap_or(' '));
            }
            let trimmed = row_text.trim_end_matches(' ');
            let remaining = max_bytes.saturating_sub(out.len());
            if trimmed.len() <= remaining {
                out.push_str(trimmed);
            } else {
                let mut part = trimmed.to_owned();
                part.truncate(part.floor_char_boundary(remaining));
                out.push_str(&part);
                break;
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
        self.search_lines_bounded(
            from_line,
            to_line,
            needle,
            case_sensitive,
            limit,
            SEARCH_CANDIDATES,
            MAX_SEARCH_ALLOC,
            out,
        )
    }

    fn images(&self) -> Vec<ImagePlacement> {
        self.images_capped(MAX_IMAGE_PLACEMENTS)
    }

    fn encode_key(&mut self, key: &KeyInput, out: &mut Vec<u8>) {
        self.encoders.encode_key(self.handle, key, out);
    }

    fn encode_pointer(&mut self, ev: &PointerInput, held_buttons: &mut u8, out: &mut Vec<u8>) {
        self.encoders.encode_pointer(self.handle, self.size, ev, held_buttons, out);
    }

    fn encode_focus(&mut self, focused: bool, out: &mut Vec<u8>) {
        encode_focus(self.handle, focused, out);
    }

    fn encode_paste(&mut self, text: &str, allow_unsafe: bool) -> PasteEncode {
        encode_paste(self.handle, text, allow_unsafe)
    }

    fn set_cursor_defaults(&mut self, shape: CursorShape, blinking: bool) {
        self.cursor_defaults = Some((shape, blinking));
        apply_cursor_defaults(self.handle, shape, blinking);
        self.refresh_render_state(false, 0);
    }

    fn compress_step(&mut self, max_pages: usize) -> CompressStep {
        for _ in 0..max_pages {
            let mut outcome: sys::TerminalCompressionResult::Type = sys::TerminalCompressionResult::COMPLETE;
            // SAFETY: `self.handle` is live for `&mut self` and `outcome` is a valid out-pointer.
            let res = unsafe {
                sys::ghostty_terminal_compress(self.handle, sys::TerminalCompressionMode::INCREMENTAL, &raw mut outcome)
            };
            if res != sys::Result::SUCCESS || outcome != sys::TerminalCompressionResult::PENDING {
                return CompressStep::Done;
            }
        }
        CompressStep::More
    }

    fn scrollback_bytes(&self) -> u64 {
        let mut usage =
            sys::TerminalMemoryUsage { size: std::mem::size_of::<sys::TerminalMemoryUsage>(), ..Default::default() };
        // SAFETY: `MEMORY_USAGE` writes a sized `GhosttyTerminalMemoryUsage`.
        let res =
            unsafe { sys::ghostty_terminal_get(self.handle, sys::TerminalData::MEMORY_USAGE, (&raw mut usage).cast()) };
        if res == sys::Result::SUCCESS { usage.primary_resident_bytes } else { 0 }
    }

    fn ensure_min_epoch(&mut self, min_epoch: u64) {
        self.epoch = self.epoch.max(min_epoch);
    }
}

pub(super) fn apply_cursor_defaults(handle: sys::Terminal, shape: CursorShape, blinking: bool) {
    let style = match shape {
        CursorShape::Block => sys::CursorStyle::BLOCK,
        CursorShape::Hollow => sys::CursorStyle::BLOCK_HOLLOW,
        CursorShape::Bar => sys::CursorStyle::BAR,
        CursorShape::Underline => sys::CursorStyle::UNDERLINE,
    };
    // SAFETY: `DEFAULT_CURSOR_STYLE` takes `GhosttyCursorStyle*` and `DEFAULT_CURSOR_BLINK` takes `bool*`.
    unsafe {
        let _ = sys::ghostty_terminal_set(handle, sys::TerminalOption::DEFAULT_CURSOR_STYLE, (&raw const style).cast());
        let _ =
            sys::ghostty_terminal_set(handle, sys::TerminalOption::DEFAULT_CURSOR_BLINK, (&raw const blinking).cast());
    }
}

/// Coalesces flood-prone stateless/latest-wins events (`Bell`, consecutive `Title`/`Progress`)
/// within a single `feed()` call while strictly preserving ordering of marks, epochs, and replies.
pub(crate) fn coalesce_feed_events(raw: Vec<VtEvent>, out: &mut Vec<VtEvent>) {
    let mut saw_bell = false;
    for ev in raw {
        match ev {
            VtEvent::Bell => {
                if !saw_bell {
                    saw_bell = true;
                    out.push(VtEvent::Bell);
                }
            }
            VtEvent::Title(title) => {
                if let Some(VtEvent::Title(prev)) = out.last_mut() {
                    *prev = title;
                } else {
                    out.push(VtEvent::Title(title));
                }
            }
            VtEvent::Progress { state, percent } => {
                if let Some(VtEvent::Progress { state: ps, percent: pp }) = out.last_mut() {
                    *ps = state;
                    *pp = percent;
                } else {
                    out.push(VtEvent::Progress { state, percent });
                }
            }
            other => out.push(other),
        }
    }
}

impl GhosttyEngine {
    pub(crate) fn images_capped(&self, max_placements: usize) -> Vec<ImagePlacement> {
        let Some(graphics) = query_graphics(self.handle) else {
            return Vec::new();
        };
        let mut iter: sys::KittyGraphicsPlacementIterator = std::ptr::null_mut();
        // SAFETY: `iter` is a valid out-pointer; freed below.
        if unsafe { sys::ghostty_kitty_graphics_placement_iterator_new(std::ptr::null(), &raw mut iter) }
            != sys::Result::SUCCESS
        {
            return Vec::new();
        }
        // SAFETY: `PLACEMENT_ITERATOR` fills `iter`.
        let filled = unsafe {
            sys::ghostty_kitty_graphics_get(
                graphics,
                sys::KittyGraphicsData::PLACEMENT_ITERATOR,
                (&raw mut iter).cast(),
            )
        };
        let mut out = Vec::new();
        if filled == sys::Result::SUCCESS {
            // SAFETY: iterating placements while `self.handle` is borrowed immutably.
            while unsafe { sys::ghostty_kitty_graphics_placement_next(iter) } {
                if out.len() >= max_placements {
                    break;
                }
                let Some(id) = read_placement_u32(iter, sys::KittyGraphicsPlacementData::IMAGE_ID) else {
                    continue;
                };
                // SAFETY: `graphics` is live for `&self`.
                let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, id) };
                if img.is_null() {
                    continue;
                }
                let img_gen = query_image_generation(img);
                let Some(&(blob, _img_w, _img_h)) = self.image_cache.blobs.get(&(id, img_gen)) else {
                    continue;
                };
                let mut rect = sys::Selection::default();
                // SAFETY: virtual placements return non-SUCCESS here and are skipped (they live in `PackedRow.pieces`).
                if unsafe { sys::ghostty_kitty_graphics_placement_rect(iter, img, self.handle, &raw mut rect) }
                    != sys::Result::SUCCESS
                {
                    continue;
                }
                let Some(span) = self.selection_to_span(&rect) else {
                    continue;
                };
                let (mut cols, mut rows) = (0u32, 0u32);
                // SAFETY: querying grid span of this live placement.
                if unsafe {
                    sys::ghostty_kitty_graphics_placement_grid_size(
                        iter,
                        img,
                        self.handle,
                        &raw mut cols,
                        &raw mut rows,
                    )
                } != sys::Result::SUCCESS
                {
                    continue;
                }
                let (mut src_x, mut src_y, mut src_w, mut src_h) = (0u32, 0u32, 0u32, 0u32);
                // SAFETY: querying source crop rectangle of this live placement.
                if unsafe {
                    sys::ghostty_kitty_graphics_placement_source_rect(
                        iter,
                        img,
                        &raw mut src_x,
                        &raw mut src_y,
                        &raw mut src_w,
                        &raw mut src_h,
                    )
                } != sys::Result::SUCCESS
                {
                    continue;
                }
                let (mut px_w, mut px_h) = (0u32, 0u32);
                // SAFETY: querying drawn physical pixel size of this live placement.
                let drawn_res = unsafe {
                    sys::ghostty_kitty_graphics_placement_pixel_size(
                        iter,
                        img,
                        self.handle,
                        &raw mut px_w,
                        &raw mut px_h,
                    )
                };
                let pixel_size =
                    resolve_drawn_pixel_size(drawn_res == sys::Result::SUCCESS, px_w, px_h, [src_w, src_h]);
                let x_off = read_placement_u32(iter, sys::KittyGraphicsPlacementData::X_OFFSET).unwrap_or(0);
                let y_off = read_placement_u32(iter, sys::KittyGraphicsPlacementData::Y_OFFSET).unwrap_or(0);
                let mut z: i32 = 0;
                // SAFETY: `Z` writes an `int32_t`.
                let _ = unsafe {
                    sys::ghostty_kitty_graphics_placement_get(
                        iter,
                        sys::KittyGraphicsPlacementData::Z,
                        (&raw mut z).cast(),
                    )
                };
                if let Some(placement) = checked_image_placement(
                    blob,
                    span.start.line,
                    i32::from(span.start.col),
                    [src_x, src_y, src_w, src_h],
                    [x_off, y_off],
                    pixel_size,
                    cols,
                    rows,
                    z,
                ) {
                    out.push(placement);
                }
            }
        }
        // SAFETY: freeing placement iterator once.
        unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iter) };
        out
    }

    pub(crate) fn text_of_batched(&self, span: CellSpan, max_bytes: usize, batch_lines: u64) -> String {
        let step = batch_lines.clamp(1, MAX_FORMAT_BATCH_ROWS);
        let max_batch_rows = step.saturating_mul(2).min(MAX_FORMAT_BATCH_ROWS);
        let max_col = self.size.cols.get().saturating_sub(1);
        let mut out = String::new();
        let mut cur_line = span.start.line;
        let mut prev_wrapped = false;
        while cur_line <= span.end.line && out.len() < max_bytes {
            let base_end = cur_line.saturating_add(step.saturating_sub(1)).min(span.end.line);
            let hard_end = cur_line.saturating_add(max_batch_rows.saturating_sub(1)).min(span.end.line);
            let batch_end = match self.line_at(base_end) {
                Some(logical) => {
                    let ext = logical.end.line.clamp(base_end, hard_end);
                    // Never mix earlier unwrapped lines with a mid-wrap cut in the same formatter batch,
                    // because `FormatterOption::TRIM = false` applies to every line in the batch.
                    if self.screen_row_is_wrapped(ext) && logical.start.line > cur_line {
                        logical.start.line - 1
                    } else {
                        ext
                    }
                }
                None => base_end,
            };
            let sub = CellSpan {
                start: CellPos { line: cur_line, col: if cur_line == span.start.line { span.start.col } else { 0 } },
                end: CellPos { line: batch_end, col: if batch_end == span.end.line { span.end.col } else { max_col } },
            };
            if cur_line > span.start.line && !prev_wrapped {
                out.push('\n');
            }
            let wrapped = self.screen_row_is_wrapped(batch_end);
            // Preserve interior trailing spaces when a batch cuts mid-way through a soft-wrapped logical line.
            let trim = !(wrapped && sub.end.col == max_col);
            let chunk = self.format_span_raw(sub, trim);
            let remaining = max_bytes.saturating_sub(out.len());
            let take = chunk.floor_char_boundary(remaining);
            out.push_str(&chunk[..take]);
            if take != chunk.len() {
                break;
            }
            prev_wrapped = wrapped;
            let Some(next) = batch_end.checked_add(1) else {
                break;
            };
            cur_line = next;
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_lines_bounded(
        &self,
        from_line: u64,
        to_line: u64,
        needle: &str,
        case_sensitive: bool,
        limit: usize,
        wanted: usize,
        max_alloc: usize,
        out: &mut Vec<CellSpan>,
    ) -> bool {
        if needle.is_empty() || from_line >= to_line || limit == 0 {
            return false;
        }
        let mut search: sys::Search = std::ptr::null_mut();
        // SAFETY: `search` is a valid out-pointer; freed unconditionally below.
        if unsafe { sys::ghostty_search_new(std::ptr::null(), &raw mut search, self.handle) } != sys::Result::SUCCESS {
            return false;
        }
        let text = sys::String::from(needle);
        let mut raw_matches: Vec<sys::Selection> = Vec::new();
        let mut complete = false;
        let mut alloc_exceeded = false;
        let total_rows = usize::try_from(self.scrollback_lines)
            .unwrap_or(usize::MAX)
            .saturating_add(usize::from(self.size.rows.get()));
        let max_passes = total_rows.saturating_mul(16).max(1024);
        // SAFETY: `search` is live; `NEEDLE` copies `text`, `MATCHES` writes up to `cap` selections.
        let res = unsafe {
            let mut r =
                sys::ghostty_search_set(search, sys::SearchOption::GHOSTTY_SEARCH_OPT_NEEDLE, (&raw const text).cast());
            let mut total: usize = 0;
            let mut needs_feed = true;
            for _ in 0..max_passes {
                if r != sys::Result::SUCCESS {
                    break;
                }
                if needs_feed {
                    r = sys::ghostty_search_feed(search);
                    if r != sys::Result::SUCCESS {
                        break;
                    }
                }
                let mut status: sys::SearchStatus::Type = sys::SearchStatus::RUNNING;
                r = sys::ghostty_search_tick(search, &raw mut status);
                if r != sys::Result::SUCCESS {
                    break;
                }
                r = sys::ghostty_search_get(search, sys::SearchData::TOTAL_MATCHES, (&raw mut total).cast());
                if status == sys::SearchStatus::COMPLETE {
                    complete = true;
                    break;
                }
                if total >= wanted {
                    break;
                }
                needs_feed = status != sys::SearchStatus::RUNNING;
            }
            if r == sys::Result::SUCCESS {
                if total > max_alloc {
                    // `ghostty_search_get(MATCHES)` requires `cap >= total`; refuse unbounded allocation
                    // and explicitly signal truncation instead of silently returning `false`.
                    alloc_exceeded = true;
                } else if total != 0 {
                    raw_matches.resize(total, sys::Selection::default());
                    let mut buf =
                        sys::SelectionBuffer { ptr: raw_matches.as_mut_ptr(), cap: raw_matches.len(), len: 0 };
                    r = sys::ghostty_search_get(search, sys::SearchData::MATCHES, (&raw mut buf).cast());
                    raw_matches.truncate(if r == sys::Result::SUCCESS { buf.len.min(raw_matches.len()) } else { 0 });
                }
            }
            r
        };
        // SAFETY: freeing search handle once.
        unsafe { sys::ghostty_search_free(search) };
        if res != sys::Result::SUCCESS {
            return false;
        }
        if alloc_exceeded {
            return true;
        }

        let mut formatter: sys::Formatter = std::ptr::null_mut();
        if case_sensitive && !raw_matches.is_empty() {
            // SAFETY: creating a single reused formatter for exact-case filtering; fail-closed on error.
            let _ = unsafe { sys::ghostty_formatter_terminal_new(std::ptr::null(), &raw mut formatter, self.handle) };
            if formatter.is_null() {
                return false;
            }
            let unwrap = true;
            // SAFETY: `UNWRAP` takes a `bool*`.
            let _ = unsafe {
                sys::ghostty_formatter_set(formatter, sys::FormatterOption::UNWRAP, (&raw const unwrap).cast())
            };
        }

        let needle_bytes = needle.as_bytes();
        let mut filtered: Vec<CellSpan> = Vec::new();
        for sel in &raw_matches {
            let Some(span) = self.selection_to_span(sel) else {
                continue;
            };
            if span.start.line < from_line || span.end.line >= to_line {
                continue;
            }
            if !formatter.is_null() && !selection_matches_exact(formatter, sel, needle_bytes) {
                continue;
            }
            filtered.push(span);
        }
        // SAFETY: `ghostty_formatter_free` accepts a null pointer.
        unsafe { sys::ghostty_formatter_free(formatter) };

        filtered.sort_by_key(|s| (s.start.line, s.start.col, s.end.line, s.end.col));
        let mut truncated = !complete;
        if filtered.len() > limit {
            filtered.truncate(limit);
            truncated = true;
        }
        out.extend(filtered);
        truncated
    }
}
