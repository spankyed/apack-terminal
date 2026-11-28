//! Incremental VT feeding, viewport render-state refresh, image cache sync, and coordinate mapping.

use std::collections::{BTreeSet, HashSet};
use std::ptr::NonNull;

use apack_core::vt::VtEvent;
use apack_proto::types::{BlobHash, CellPos, CellSpan, CursorShape, CursorState, ImageEncoding, ViewModes};
use ghostty_vt_rs_sys as sys;

use crate::MAX_IMAGE_PLACEMENTS;
use crate::image_store::encode_png_pixels;
use crate::input::query_mode;
use crate::rows::VirtualImageInfo;

use super::ffi_queries::{
    copy_kitty_pixels, hash_viewport_row, query_alt_screen, query_graphics, query_image_generation,
    query_images_generation, query_mouse_tracking, query_palette_overrides, query_scrollback_rows, read_placement_u32,
    read_u16, track_active_top, tracked_screen_row, viewport_row_has_placeholder,
};
use super::vt_impl::coalesce_feed_events;
use super::{GhosttyEngine, NamedImageProbe};

const REVERSE_VIDEO_MODE: u16 = 5;
const BRACKETED_PASTE_MODE: u16 = 2004;

impl GhosttyEngine {
    pub(super) fn update_scroll_and_screen(&mut self, events: &mut Vec<VtEvent>) -> u64 {
        let after_alt = query_alt_screen(self.handle);
        let after_sb = query_scrollback_rows(self.handle);
        if after_alt != self.alt_screen {
            self.alt_screen = after_alt;
            self.scrollback_lines = after_sb;
            self.first_active_line = if after_alt { 0 } else { after_sb };
            let first = self.first_active_line;
            self.callbacks.with_mut(|cb| cb.first_active_line = first);
            self.epoch += 1;
            self.full_dirty = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            self.reset_active_top_mark();
            events.push(VtEvent::EpochChanged);
            return 0;
        }

        // Alternate screen has zero scrollback and fixed `first_active_line == 0`.
        if self.alt_screen {
            self.scrollback_lines = 0;
            self.first_active_line = 0;
            if tracked_screen_row(self.active_top).is_none() {
                self.full_dirty = true;
                self.dirty_rows.clear();
                self.scroll_delta = 0;
                self.reset_active_top_mark();
            }
            return 0;
        }

        // Measure exact lines scrolled up since last step using `active_top` TrackedGridRef.
        let Some(row) = tracked_screen_row(self.active_top) else {
            // Active top scrolled off the scrollback limit (or `scrollback_lines == 0`):
            // bump epoch rather than guessing scroll count and silently renumbering surviving lines!
            self.scrollback_lines = after_sb;
            self.first_active_line = after_sb;
            self.callbacks.with_mut(|cb| cb.first_active_line = after_sb);
            self.epoch += 1;
            self.full_dirty = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            self.reset_active_top_mark();
            events.push(VtEvent::EpochChanged);
            return 0;
        };

        let scrolled_up = after_sb.saturating_sub(u64::from(row));
        self.scrollback_lines = after_sb;
        let mut applied_scroll = 0u64;
        if scrolled_up != 0 {
            self.first_active_line = self.first_active_line.saturating_add(scrolled_up);
            let first = self.first_active_line;
            self.callbacks.with_mut(|cb| cb.first_active_line = first);
            let cumulative = u64::try_from(self.scroll_delta).unwrap_or(0).saturating_add(scrolled_up);
            if self.full_dirty || cumulative >= u64::from(self.size.rows.get()) {
                self.full_dirty = true;
                self.dirty_rows.clear();
                self.scroll_delta = 0;
            } else {
                let shift = scrolled_up as i32;
                let mut shifted = BTreeSet::new();
                for &r in &self.dirty_rows {
                    let moved = i32::from(r) - shift;
                    if moved >= 0 {
                        shifted.insert(moved as u16);
                    }
                }
                self.dirty_rows = shifted;
                self.scroll_delta = self.scroll_delta.saturating_add(shift);
                applied_scroll = scrolled_up;
            }
        }
        self.reset_active_top_mark();
        applied_scroll
    }

    pub(super) fn format_span_raw(&self, span: CellSpan, trim: bool) -> String {
        let (Some(start_ref), Some(end_ref)) = (self.screen_grid_ref(span.start), self.screen_grid_ref(span.end))
        else {
            return String::new();
        };
        let sel = sys::Selection { start: start_ref, end: end_ref, rectangle: false };
        let mut formatter: sys::Formatter = std::ptr::null_mut();
        // SAFETY: `formatter` is a valid out-pointer and `self.handle` outlives it.
        if unsafe { sys::ghostty_formatter_terminal_new(std::ptr::null(), &raw mut formatter, self.handle) }
            != sys::Result::SUCCESS
        {
            return String::new();
        }
        let unwrap = true;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len: usize = 0;
        // SAFETY: `SELECTION` takes `GhosttySelection*`, `UNWRAP` and `TRIM` take `bool*`; buffer freed below.
        let res = unsafe {
            let mut r = sys::ghostty_formatter_set(formatter, sys::FormatterOption::SELECTION, (&raw const sel).cast());
            if r == sys::Result::SUCCESS {
                r = sys::ghostty_formatter_set(formatter, sys::FormatterOption::UNWRAP, (&raw const unwrap).cast());
            }
            if r == sys::Result::SUCCESS {
                r = sys::ghostty_formatter_set(formatter, sys::FormatterOption::TRIM, (&raw const trim).cast());
            }
            if r == sys::Result::SUCCESS {
                r = sys::ghostty_formatter_format_alloc(formatter, std::ptr::null(), &raw mut ptr, &raw mut len);
            }
            r
        };
        let out = match NonNull::new(ptr).filter(|_| res == sys::Result::SUCCESS) {
            // SAFETY: `ptr` is non-null and initialized for `len` bytes by `ghostty_formatter_format_alloc`.
            Some(nn) => String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(nn.as_ptr(), len) }).into_owned(),
            None => String::new(),
        };
        // SAFETY: freeing allocated buffer and formatter handle once.
        unsafe {
            sys::ghostty_free(std::ptr::null(), ptr, len);
            sys::ghostty_formatter_free(formatter);
        }
        out
    }

    pub(super) fn reset_active_top_mark(&mut self) {
        let point = sys::Point {
            tag: sys::PointTag::ACTIVE,
            value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: 0 } },
        };
        if tracked_screen_row(self.active_top).is_some() {
            // SAFETY: `active_top` is live and attached to `self.handle`.
            let _ = unsafe { sys::ghostty_tracked_grid_ref_set(self.active_top, self.handle, point) };
            return;
        }
        if let Ok(fresh) = track_active_top(self.handle) {
            let old = std::mem::replace(&mut self.active_top, fresh);
            // SAFETY: freeing replaced tracked grid ref while terminal handle is still live.
            unsafe { sys::ghostty_tracked_grid_ref_free(old) };
        }
    }

    pub(super) fn refresh_render_state(&mut self, force_full: bool, scrolled_up: u64) {
        // SAFETY: both handles are live and owned exclusively by `&mut self`.
        if unsafe { sys::ghostty_render_state_update(self.render_state, self.handle) } != sys::Result::SUCCESS {
            return;
        }
        let rows_count = usize::from(self.size.rows.get());
        let cols_count = self.size.cols.get();
        if self.viewport_hashes.len() != rows_count {
            self.viewport_hashes.resize(rows_count, 0);
        }
        let mut dirty: sys::RenderStateDirty::Type = sys::RenderStateDirty::FULL;
        // SAFETY: `DIRTY` writes a `GhosttyRenderStateDirty`.
        let _ = unsafe {
            sys::ghostty_render_state_get(self.render_state, sys::RenderStateData::DIRTY, (&raw mut dirty).cast())
        };
        if force_full || (dirty == sys::RenderStateDirty::FULL && (scrolled_up == 0 || self.full_dirty)) {
            self.full_dirty = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            self.gens.grid += 1;
            for r in 0..rows_count {
                self.viewport_hashes[r] = hash_viewport_row(self.handle, r as u32, cols_count);
            }
        } else if dirty == sys::RenderStateDirty::FULL {
            // `libghostty-vt` reports FULL on viewport scroll; preserve scroll-blit delta by
            // shifting our per-row hashes (including side-table styles/graphemes/links) and marking
            // only newly revealed or modified rows dirty.
            let shift = usize::try_from(scrolled_up).unwrap_or(rows_count).min(rows_count);
            self.viewport_hashes.copy_within(shift..rows_count, 0);
            let keep = rows_count - shift;
            for r in 0..rows_count {
                let h = hash_viewport_row(self.handle, r as u32, cols_count);
                if r >= keep || h != self.viewport_hashes[r] {
                    self.dirty_rows.insert(r as u16);
                    self.viewport_hashes[r] = h;
                }
            }
            self.gens.grid += 1;
        } else if dirty == sys::RenderStateDirty::PARTIAL {
            // SAFETY: `ROW_ITERATOR` fills `self.render_rows` for this render state snapshot.
            if unsafe {
                sys::ghostty_render_state_get(
                    self.render_state,
                    sys::RenderStateData::ROW_ITERATOR,
                    (&raw mut self.render_rows).cast(),
                )
            } == sys::Result::SUCCESS
            {
                let mut row_idx = 0u16;
                // SAFETY: iterating over the freshly populated row iterator.
                while unsafe { sys::ghostty_render_state_row_iterator_next(self.render_rows) } {
                    let mut is_dirty = false;
                    // SAFETY: `DIRTY` writes a `bool` for the current row.
                    let _ = unsafe {
                        sys::ghostty_render_state_row_get(
                            self.render_rows,
                            sys::RenderStateRowData::DIRTY,
                            (&raw mut is_dirty).cast(),
                        )
                    };
                    if is_dirty {
                        self.dirty_rows.insert(row_idx);
                        if let Some(slot) = self.viewport_hashes.get_mut(usize::from(row_idx)) {
                            *slot = hash_viewport_row(self.handle, u32::from(row_idx), cols_count);
                        }
                        self.gens.grid += 1;
                    }
                    row_idx = row_idx.saturating_add(1);
                }
            }
        }

        // Update resolved cursor from render state.
        let mut rc =
            sys::RenderStateCursor { size: std::mem::size_of::<sys::RenderStateCursor>(), ..Default::default() };
        // SAFETY: `CURSOR` writes a sized `GhosttyRenderStateCursor`.
        let _ = unsafe {
            sys::ghostty_render_state_get(self.render_state, sys::RenderStateData::CURSOR, (&raw mut rc).cast())
        };
        if rc.viewport_has_value {
            let shape = match rc.visual_style {
                sys::CursorStyle::BAR => CursorShape::Bar,
                sys::CursorStyle::UNDERLINE => CursorShape::Underline,
                sys::CursorStyle::BLOCK_HOLLOW => CursorShape::Hollow,
                _ => CursorShape::Block,
            };
            self.cursor = CursorState {
                col: rc.viewport_x,
                row: rc.viewport_y,
                shape,
                blinking: rc.blinking,
                hidden: !rc.visible,
            };
        }

        // Clean render state so the next `feed` only reports newly mutated rows.
        // SAFETY: `render_state` is live.
        let _ = unsafe { sys::ghostty_render_state_clean(self.render_state) };

        // Refresh view modes and generation counters.
        let new_modes = ViewModes {
            alt_screen: self.alt_screen,
            mouse: query_mouse_tracking(self.handle),
            reverse_video: query_mode(self.handle, REVERSE_VIDEO_MODE),
            bracketed_paste: query_mode(self.handle, BRACKETED_PASTE_MODE),
        };
        if new_modes != self.modes {
            self.modes = new_modes;
            self.gens.modes += 1;
        }

        let new_palette = query_palette_overrides(self.handle);
        if new_palette != self.palette {
            self.palette = new_palette;
            self.gens.palette += 1;
        }

        let new_images_gen = query_images_generation(self.handle);
        if new_images_gen != self.raw_images_gen {
            self.sync_images_cache();
            if self.image_cache.images_gen == new_images_gen {
                self.raw_images_gen = new_images_gen;
                self.gens.images = self.gens.images.saturating_add(1);
                // When images upload/replace/delete after U+10EEEE placeholders are already on screen,
                // mark placeholder-bearing viewport rows dirty so `PackedRow.pieces` updates!
                if !self.full_dirty {
                    for r in 0..rows_count {
                        if viewport_row_has_placeholder(self.handle, r as u32) {
                            self.dirty_rows.insert(r as u16);
                            self.gens.grid += 1;
                        }
                    }
                }
            }
        }
    }

    pub(super) fn sync_images_cache(&mut self) {
        self.sync_images_cache_capped(MAX_IMAGE_PLACEMENTS);
    }

    #[cfg(test)]
    pub(crate) fn resync_images_capped(&mut self, max_placements: usize) -> (usize, usize) {
        self.image_cache.images_gen = 0;
        self.sync_images_cache_capped(max_placements);
        (self.image_cache.blobs.len(), self.image_cache.virtual_named.len())
    }

    pub(crate) fn sync_images_cache_capped(&mut self, max_placements: usize) {
        let cur_gen = query_images_generation(self.handle);
        if self.image_cache.images_gen == cur_gen {
            return;
        }

        let Some(graphics) = query_graphics(self.handle) else {
            self.image_cache.images_gen = cur_gen;
            self.image_cache.blobs.clear();
            self.image_cache.virtual_named.clear();
            self.image_cache.virtual_lowest.clear();
            return;
        };

        let mut iter: sys::KittyGraphicsPlacementIterator = std::ptr::null_mut();
        // SAFETY: `iter` is a valid out-pointer; freed unconditionally below.
        if unsafe { sys::ghostty_kitty_graphics_placement_iterator_new(std::ptr::null(), &raw mut iter) }
            != sys::Result::SUCCESS
        {
            return;
        }
        // SAFETY: `PLACEMENT_ITERATOR` fills `iter` from `graphics`.
        let filled = unsafe {
            sys::ghostty_kitty_graphics_get(
                graphics,
                sys::KittyGraphicsData::PLACEMENT_ITERATOR,
                (&raw mut iter).cast(),
            )
        };
        if filled != sys::Result::SUCCESS {
            // SAFETY: freeing iterator on transient query error without latching `images_gen`.
            unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iter) };
            return;
        }

        self.image_cache.images_gen = cur_gen;
        self.image_cache.virtual_named.clear();
        self.image_cache.virtual_lowest.clear();

        let mut direct_keys: HashSet<(u32, u64)> = HashSet::new();
        let mut active_keys: HashSet<(u32, u64)> = HashSet::new();
        let mut to_encode: Vec<(u32, u64, u32, u32, u8, Vec<u8>)> = Vec::new();
        {
            let mut store = self.image_store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // SAFETY: iterating placements while no write mutates `self.handle`.
            while unsafe { sys::ghostty_kitty_graphics_placement_next(iter) } {
                let mut image_id: u32 = 0;
                // SAFETY: `IMAGE_ID` writes a `uint32_t`.
                if unsafe {
                    sys::ghostty_kitty_graphics_placement_get(
                        iter,
                        sys::KittyGraphicsPlacementData::IMAGE_ID,
                        (&raw mut image_id).cast(),
                    )
                } != sys::Result::SUCCESS
                {
                    continue;
                }
                let mut is_virtual = false;
                // SAFETY: `IS_VIRTUAL` writes a `bool`.
                let _ = unsafe {
                    sys::ghostty_kitty_graphics_placement_get(
                        iter,
                        sys::KittyGraphicsPlacementData::IS_VIRTUAL,
                        (&raw mut is_virtual).cast(),
                    )
                };
                // SAFETY: `graphics` is live for `&mut self`.
                let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, image_id) };
                if img.is_null() {
                    continue;
                }
                let img_gen = query_image_generation(img);
                let key = (image_id, img_gen);

                // Direct and virtual placements enforce independent caps so virtual placements
                // never starve `vt.images()` and every cached virtual entry has its blob cached.
                let admitted = if is_virtual {
                    if self.image_cache.virtual_named.len() < max_placements {
                        let pid = read_placement_u32(iter, sys::KittyGraphicsPlacementData::PLACEMENT_ID).unwrap_or(0);
                        let cols = read_placement_u32(iter, sys::KittyGraphicsPlacementData::COLUMNS).unwrap_or(0);
                        let rows = read_placement_u32(iter, sys::KittyGraphicsPlacementData::ROWS).unwrap_or(0);
                        let grid = (cols, rows);
                        self.image_cache.virtual_named.insert((image_id, pid), grid);
                        self.image_cache
                            .virtual_lowest
                            .entry(image_id)
                            .and_modify(|e| *e = (*e).min((pid, grid)))
                            .or_insert((pid, grid));
                        true
                    } else {
                        false
                    }
                } else if direct_keys.contains(&key) {
                    true
                } else if direct_keys.len() < max_placements {
                    direct_keys.insert(key);
                    true
                } else {
                    false
                };

                if admitted && active_keys.insert(key) {
                    let still_resident =
                        self.image_cache.blobs.get(&key).is_some_and(|&(hash, _, _)| store.touch(hash));
                    if !still_resident && let Some((w, h, ch, px)) = copy_kitty_pixels(img) {
                        to_encode.push((image_id, img_gen, w, h, ch, px));
                    }
                }
            }
        }
        // SAFETY: freeing iterator once.
        unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iter) };

        // Evict deleted/replaced image metadata so `image_cache.blobs` never grows unboundedly.
        self.image_cache.blobs.retain(|k, _| active_keys.contains(k));

        // Encode PNGs completely OUTSIDE the global `ImageStore` mutex so slow compression
        // never stalls other sessions/shards!
        if !to_encode.is_empty() {
            let encoded: Vec<(u32, u64, u32, u32, Vec<u8>)> = to_encode
                .into_iter()
                .filter_map(|(id, g, w, h, ch, px)| encode_png_pixels(w, h, ch, &px).map(|png| (id, g, w, h, png)))
                .collect();
            let mut store = self.image_store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, g, w, h, png) in encoded {
                if let Some(hash) = store.insert_encoded(ImageEncoding::Png, png) {
                    self.image_cache.blobs.insert((id, g), (hash, w, h));
                }
            }
        }
    }

    /// Re-encodes a live session image back into the shared [`ImageStore`] if another session
    /// evicted it under global LRU pressure, keeping `&self` row/placement reads completely lock-free.
    pub fn rehydrate_image_blob(&mut self, blob: BlobHash) -> bool {
        let Some(image_id) =
            self.image_cache.blobs.iter().find_map(|(&(id, _), &(hash, _, _))| (hash == blob).then_some(id))
        else {
            return false;
        };
        let Some(graphics) = query_graphics(self.handle) else {
            return false;
        };
        // SAFETY: `graphics` is live for `&mut self`.
        let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, image_id) };
        if img.is_null() {
            return false;
        }
        let Some((w, h, ch, px)) = copy_kitty_pixels(img) else {
            return false;
        };
        let Some(png) = encode_png_pixels(w, h, ch, &px) else {
            return false;
        };
        let mut store = self.image_store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        store.insert_encoded(ImageEncoding::Png, png) == Some(blob)
    }

    pub(super) fn resolve_virtual(&self, image_id: u32, placement_id: u32) -> Option<VirtualImageInfo> {
        let graphics = query_graphics(self.handle)?;
        // SAFETY: `graphics` is live for `&self`.
        let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, image_id) };
        if img.is_null() {
            return None;
        }
        let img_gen = query_image_generation(img);
        let &(blob, w, h) = self.image_cache.blobs.get(&(image_id, img_gen))?;
        let grid = match placement_id {
            0 => self.image_cache.virtual_lowest.get(&image_id).map(|(_, g)| *g)?,
            pid => *self.image_cache.virtual_named.get(&(image_id, pid))?,
        };
        Some(VirtualImageInfo { blob, picture_px: (w, h), grid })
    }

    pub(super) fn screen_row_is_wrapped(&self, abs_line: u64) -> bool {
        let Some(grid_ref) = self.screen_grid_ref(CellPos { line: abs_line, col: 0 }) else {
            return false;
        };
        let mut raw_row: sys::Row = 0;
        let mut wrapped = false;
        // SAFETY: `grid_ref` was resolved on live `self.handle`; `WRAP` writes a `bool`.
        unsafe {
            if sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw_row) == sys::Result::SUCCESS {
                let _ = sys::ghostty_row_get(raw_row, sys::RowData::WRAP, (&raw mut wrapped).cast());
            }
        }
        wrapped
    }

    pub(super) fn epoch_to_screen_row(&self, abs_line: u64) -> Option<u32> {
        let oldest = self.first_active_line.saturating_sub(self.scrollback_lines);
        let total = self.first_active_line.saturating_add(u64::from(self.size.rows.get()));
        if abs_line < oldest || abs_line >= total {
            return None;
        }
        u32::try_from(abs_line - oldest).ok()
    }

    pub(super) fn screen_row_to_epoch(&self, screen_y: u32) -> u64 {
        let oldest = self.first_active_line.saturating_sub(self.scrollback_lines);
        oldest.saturating_add(u64::from(screen_y))
    }

    pub(super) fn screen_grid_ref(&self, pos: CellPos) -> Option<sys::GridRef> {
        let y = self.epoch_to_screen_row(pos.line)?;
        let point = sys::Point {
            tag: sys::PointTag::SCREEN,
            value: sys::PointValue { coordinate: sys::PointCoordinate { x: pos.col, y } },
        };
        let mut grid_ref = sys::GridRef::default();
        // SAFETY: `grid_ref` is a valid out-pointer; `self.handle` is live.
        let res = unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut grid_ref) };
        (res == sys::Result::SUCCESS).then_some(grid_ref)
    }

    pub(super) fn selection_to_span(&self, sel: &sys::Selection) -> Option<CellSpan> {
        let mut start_pt = sys::PointCoordinate { x: 0, y: 0 };
        let mut end_pt = sys::PointCoordinate { x: 0, y: 0 };
        // SAFETY: both `GridRef`s came from `self.handle` and no write occurred since.
        let ok = unsafe {
            sys::ghostty_terminal_point_from_grid_ref(
                self.handle,
                &raw const sel.start,
                sys::PointTag::SCREEN,
                &raw mut start_pt,
            ) == sys::Result::SUCCESS
                && sys::ghostty_terminal_point_from_grid_ref(
                    self.handle,
                    &raw const sel.end,
                    sys::PointTag::SCREEN,
                    &raw mut end_pt,
                ) == sys::Result::SUCCESS
        };
        if !ok {
            return None;
        }
        let a = CellPos { line: self.screen_row_to_epoch(start_pt.y), col: start_pt.x };
        let b = CellPos { line: self.screen_row_to_epoch(end_pt.y), col: end_pt.x };
        Some(CellSpan { start: a.min(b), end: a.max(b) })
    }
}

impl GhosttyEngine {
    /// Temporarily overrides `libghostty-vt`'s per-command Kitty graphics APC byte limit
    /// (`Some(n)` for large `a=q` queries, `None` to restore the library default).
    pub fn set_kitty_command_bytes(&mut self, bytes: Option<usize>) {
        let ptr = bytes.as_ref().map_or(std::ptr::null(), |b| std::ptr::from_ref(b).cast::<std::ffi::c_void>());
        // SAFETY: `self.handle` is live for `&mut self`; `APC_MAX_BYTES_KITTY` takes `*const usize` or null.
        let _ = unsafe { sys::ghostty_terminal_set(self.handle, sys::TerminalOption::APC_MAX_BYTES_KITTY, ptr) };
    }

    /// Feeds a validated single-command NUL-payload Kitty graphics probe ([`NamedImageProbe`])
    /// with file and shared-memory mediums temporarily enabled so `libghostty-vt` validates all
    /// command keys and returns its native reply without ever opening a real path, unconditionally
    /// disabling both mediums again before returning.
    pub fn feed_named_probe(&mut self, probe: &NamedImageProbe, events: &mut Vec<VtEvent>) {
        struct DisableNamedMediums(sys::Terminal);
        impl Drop for DisableNamedMediums {
            fn drop(&mut self) {
                let disabled = false;
                let ptr = (&raw const disabled).cast::<std::ffi::c_void>();
                // SAFETY: `self.0` stays live for the duration of `feed_named_probe`.
                unsafe {
                    let _ = sys::ghostty_terminal_set(self.0, sys::TerminalOption::KITTY_IMAGE_MEDIUM_FILE, ptr);
                    let _ = sys::ghostty_terminal_set(self.0, sys::TerminalOption::KITTY_IMAGE_MEDIUM_SHARED_MEM, ptr);
                }
            }
        }
        let enabled = true;
        let ptr = (&raw const enabled).cast::<std::ffi::c_void>();
        // SAFETY: `self.handle` is live for `&mut self`; `DisableNamedMediums` restores `false` on exit.
        unsafe {
            let _ = sys::ghostty_terminal_set(self.handle, sys::TerminalOption::KITTY_IMAGE_MEDIUM_FILE, ptr);
            let _ = sys::ghostty_terminal_set(self.handle, sys::TerminalOption::KITTY_IMAGE_MEDIUM_SHARED_MEM, ptr);
        }
        let _guard = DisableNamedMediums(self.handle);
        let _ = self.feed_inner(probe.as_bytes(), false, events);
    }

    /// Feeds `bytes` up to the first APC/SOS/PM string introducer (`TerminalBoundary::APC_START`),
    /// returning `(bytes_consumed, entered_apc)`.
    ///
    /// When `entered_apc` is `true`, `libghostty-vt` has consumed the introducer and sits at the
    /// start of the string body so an external scanner can inspect named Kitty graphics commands.
    pub fn feed_until_apc(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) -> (usize, bool) {
        self.feed_inner(bytes, true, events)
    }

    pub(super) fn feed_inner(&mut self, bytes: &[u8], stop_on_apc: bool, events: &mut Vec<VtEvent>) -> (usize, bool) {
        let mut rest = bytes;
        let mut total_consumed = 0usize;
        let mut entered_apc = false;
        let mut total_scrolled = 0u64;
        let mut raw_events: Vec<VtEvent> = Vec::new();
        let boundaries = if stop_on_apc {
            sys::TerminalBoundary::OSC_END | sys::TerminalBoundary::APC_START
        } else {
            sys::TerminalBoundary::OSC_END
        };
        while !rest.is_empty() {
            let mut stop =
                sys::TerminalWriteStop { size: std::mem::size_of::<sys::TerminalWriteStop>(), ..Default::default() };
            // SAFETY: `self.handle` is live for `&mut self`, `rest` outlives the call, `stop` is sized.
            let res = unsafe {
                sys::ghostty_terminal_vt_write_until(self.handle, rest.as_ptr(), rest.len(), boundaries, &raw mut stop)
            };
            let consumed = match res {
                sys::Result::SUCCESS => stop.consumed.clamp(1, rest.len()),
                sys::Result::NO_VALUE => rest.len(),
                _ => {
                    // SAFETY: fallback plain write when `vt_write_until` returns an unexpected code.
                    unsafe { sys::ghostty_terminal_vt_write(self.handle, rest.as_ptr(), rest.len()) };
                    rest.len()
                }
            };
            rest = &rest[consumed..];
            total_consumed = total_consumed.saturating_add(consumed);

            // Synchronize scroll / alt-screen epoch BEFORE stamping cursor_line on callbacks from this slice!
            let mut pending = self.callbacks.with_mut(|cb| std::mem::take(&mut cb.events));
            total_scrolled = total_scrolled.saturating_add(self.update_scroll_and_screen(&mut raw_events));
            let cur_y = read_u16(self.handle, sys::TerminalData::CURSOR_Y).unwrap_or(0);
            let abs_cursor_line = self.first_active_line.saturating_add(u64::from(cur_y));
            for ev in &mut pending {
                match ev {
                    VtEvent::Osc133 { cursor_line, .. } | VtEvent::Osc633 { cursor_line, .. } => {
                        *cursor_line = abs_cursor_line;
                    }
                    _ => {}
                }
            }
            raw_events.extend(pending);

            if stop_on_apc
                && res == sys::Result::SUCCESS
                && stop.reached == sys::TerminalBoundary::APC_START
                && matches!(stop.byte, b'_' | 0x9f)
            {
                entered_apc = true;
                break;
            }
        }
        coalesce_feed_events(raw_events, events);
        if total_consumed != 0 {
            self.refresh_render_state(false, total_scrolled);
        }
        (total_consumed, entered_apc)
    }
}
