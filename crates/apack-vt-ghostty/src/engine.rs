//! [`GhosttyEngine`] implementing [`apack_core::vt::VtEngine`] on unmodified `ghostty-vt-rs-sys`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use apack_core::vt::{CompressStep, DirtyViewport, PasteEncode, VtEngine, VtEvent, VtGenerations, VtSnapshotMeta};
use apack_proto::types::{
    BlobHash, CellPos, CellPx, CellSpan, CursorShape, CursorState, GridSize, ImagePlacement, KeyInput, PackedRow,
    PaletteOverrides, PointerInput, ProgressState, Rgb, ViewModes,
};
use ghostty_vt_rs_sys as sys;

use crate::error::GhosttyError;
use crate::image_store::{ImageStore, install_png_decoder};
use crate::input::{InputEncoders, encode_focus, encode_paste, query_mode};
use crate::rows::{VirtualImageInfo, extract_row};
use crate::{SESSION_IMAGE_STORAGE_BYTES, UNKNOWN_OSC_MAX_BYTES};

const REVERSE_VIDEO_MODE: u16 = 5;
const BRACKETED_PASTE_MODE: u16 = 2004;
const SEARCH_CANDIDATES: usize = 20_000;

/// Linked `libghostty-vt` compile-time feature flags queried via `ghostty_build_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtFeatures {
    /// Render-state dirty tracking.
    pub render_state: bool,
    /// Selection & plain-text formatter.
    pub formatter: bool,
    /// Word & line selection helpers.
    pub selection: bool,
    /// Incremental search engine.
    pub search: bool,
    /// Keyboard & mouse input encoders.
    pub input_encode: bool,
    /// Direct grid/cell introspection.
    pub grid_introspection: bool,
    /// Binary terminal snapshot encode/decode.
    pub snapshot: bool,
    /// Kitty graphics protocol support.
    pub kitty_graphics: bool,
}

/// Queries which optional features were compiled into the linked `libghostty-vt` static library.
pub fn features() -> VtFeatures {
    let has = |key: sys::BuildInfo::Type| {
        let mut present = false;
        // SAFETY: every `FEATURE_*` / `KITTY_GRAPHICS` key writes a `bool` to the out-pointer.
        if unsafe { sys::ghostty_build_info(key, (&raw mut present).cast::<std::ffi::c_void>()) }
            == sys::Result::SUCCESS
        {
            present
        } else {
            false
        }
    };
    VtFeatures {
        render_state: has(sys::BuildInfo::FEATURE_RENDER_STATE),
        formatter: has(sys::BuildInfo::FEATURE_FORMATTER),
        selection: has(sys::BuildInfo::FEATURE_SELECTION),
        search: has(sys::BuildInfo::FEATURE_SEARCH),
        input_encode: has(sys::BuildInfo::FEATURE_INPUT_ENCODE),
        grid_introspection: has(sys::BuildInfo::FEATURE_GRID_INTROSPECTION),
        snapshot: has(sys::BuildInfo::FEATURE_SNAPSHOT),
        kitty_graphics: has(sys::BuildInfo::KITTY_GRAPHICS),
    }
}

/// Configuration for creating a [`GhosttyEngine`].
#[derive(Debug, Clone)]
pub struct GhosttyConfig {
    /// Initial grid dimensions.
    pub size: GridSize,
    /// Initial cell pixel size (`0x0` for headless).
    pub cell_px: CellPx,
    /// Scrollback line cap (`0` disables scrollback).
    pub scrollback_lines: usize,
    /// Whether Kitty graphics are accepted (`false` sets image storage limit to 0).
    pub allow_images: bool,
    /// Optional terminfo `TERM` name reported to queries.
    pub terminfo_name: String,
    /// Optional shared broker-wide deduplicated [`ImageStore`].
    pub image_store: Option<Arc<Mutex<ImageStore>>>,
}

impl GhosttyConfig {
    /// Creates a standard configuration for `size` with 10,000 lines of scrollback and images enabled.
    pub fn new(size: GridSize) -> Self {
        Self {
            size,
            cell_px: CellPx::default(),
            scrollback_lines: 10_000,
            allow_images: true,
            terminfo_name: String::from("xterm-256color"),
            image_store: None,
        }
    }
}

/// Heap-allocated callback target pinned at a stable address for `libghostty-vt` C callbacks.
struct CallbackState {
    events: Vec<VtEvent>,
    cols: u16,
    rows: u16,
    cell_px: CellPx,
    first_active_line: u64,
}

#[derive(Default)]
struct ImageSyncCache {
    images_gen: u64,
    /// Maps `(kitty_image_id, kitty_image_gen)` -> `(BlobHash, width, height)`.
    blobs: HashMap<(u32, u64), (BlobHash, u32, u32)>,
    /// Virtual placement grids keyed by `(image_id, placement_id)`.
    virtual_named: HashMap<(u32, u32), (u32, u32)>,
    /// Lowest-numbered virtual placement grid per `image_id`.
    virtual_lowest: HashMap<u32, (u32, (u32, u32))>,
}

/// `libghostty-vt` terminal emulator implementing [`VtEngine`].
///
/// Field safety: `Drop` explicitly frees `active_top`, `render_rows`, `render_state`, and
/// `handle` before `callbacks` is dropped, guaranteeing C callbacks never see dangling userdata.
pub struct GhosttyEngine {
    handle: sys::Terminal,
    render_state: sys::RenderState,
    render_rows: sys::RenderStateRowIterator,
    active_top: sys::TrackedGridRef,
    encoders: InputEncoders,
    callbacks: Box<CallbackState>,
    image_store: Arc<Mutex<ImageStore>>,
    image_cache: RefCell<ImageSyncCache>,
    size: GridSize,
    cell_px: CellPx,
    scrollback_lines_limit: usize,
    allow_images: bool,
    terminfo_name: String,
    epoch: u64,
    first_active_line: u64,
    scrollback_lines: u64,
    alt_screen: bool,
    cursor: CursorState,
    modes: ViewModes,
    palette: PaletteOverrides,
    gens: VtGenerations,
    dirty_rows: BTreeSet<u16>,
    scroll_delta: i32,
    full_dirty: bool,
    viewport_hashes: Vec<u64>,
    cursor_defaults: Option<(CursorShape, bool)>,
}

// SAFETY: `GhosttyEngine` exclusively owns every underlying C handle and its boxed `CallbackState`;
// none of them are shared across threads, so moving ownership between threads is sound.
unsafe impl Send for GhosttyEngine {}

impl GhosttyEngine {
    /// Creates a new `GhosttyEngine` with hardened defaults (zero file/SHM image reads).
    pub fn new(config: GhosttyConfig) -> Result<Self, GhosttyError> {
        install_png_decoder();

        let mut handle: sys::Terminal = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer; null allocator uses the library default.
        let res = unsafe {
            sys::ghostty_terminal_new(std::ptr::null(), &raw mut handle, config.size.cols.get(), config.size.rows.get())
        };
        GhosttyError::check("ghostty_terminal_new", res)?;
        if handle.is_null() {
            return Err(GhosttyError { call: "ghostty_terminal_new", code: sys::Result::OUT_OF_MEMORY });
        }

        let mut render_state: sys::RenderState = std::ptr::null_mut();
        // SAFETY: `render_state` is a valid out-pointer.
        let rs_res = unsafe { sys::ghostty_render_state_new(std::ptr::null(), &raw mut render_state) };
        if let Err(e) = GhosttyError::check("ghostty_render_state_new", rs_res) {
            // SAFETY: freeing freshly created terminal handle on partial init failure.
            unsafe { sys::ghostty_terminal_free(handle) };
            return Err(e);
        }

        let mut render_rows: sys::RenderStateRowIterator = std::ptr::null_mut();
        // SAFETY: `render_rows` is a valid out-pointer.
        let rr_res = unsafe { sys::ghostty_render_state_row_iterator_new(std::ptr::null(), &raw mut render_rows) };
        if let Err(e) = GhosttyError::check("ghostty_render_state_row_iterator_new", rr_res) {
            // SAFETY: freeing partially initialized handles in reverse order.
            unsafe {
                sys::ghostty_render_state_free(render_state);
                sys::ghostty_terminal_free(handle);
            }
            return Err(e);
        }

        let encoders = match InputEncoders::new() {
            Ok(enc) => enc,
            Err(e) => {
                // SAFETY: freeing partially initialized handles on encoder init failure.
                unsafe {
                    sys::ghostty_render_state_row_iterator_free(render_rows);
                    sys::ghostty_render_state_free(render_state);
                    sys::ghostty_terminal_free(handle);
                }
                return Err(e);
            }
        };

        let active_top = match track_active_top(handle) {
            Ok(mark) => mark,
            Err(e) => {
                // SAFETY: freeing partially initialized handles on mark init failure.
                unsafe {
                    sys::ghostty_render_state_row_iterator_free(render_rows);
                    sys::ghostty_render_state_free(render_state);
                    sys::ghostty_terminal_free(handle);
                }
                return Err(e);
            }
        };

        let callbacks = Box::new(CallbackState {
            events: Vec::new(),
            cols: config.size.cols.get(),
            rows: config.size.rows.get(),
            cell_px: config.cell_px,
            first_active_line: 0,
        });
        let image_store = config.image_store.unwrap_or_else(|| Arc::new(Mutex::new(ImageStore::default())));

        let mut engine = Self {
            handle,
            render_state,
            render_rows,
            active_top,
            encoders,
            callbacks,
            image_store,
            image_cache: RefCell::new(ImageSyncCache::default()),
            size: config.size,
            cell_px: config.cell_px,
            scrollback_lines_limit: config.scrollback_lines,
            allow_images: config.allow_images,
            terminfo_name: config.terminfo_name,
            epoch: 1,
            first_active_line: 0,
            scrollback_lines: 0,
            alt_screen: false,
            cursor: CursorState { col: 0, row: 0, shape: CursorShape::Block, blinking: false, hidden: false },
            modes: ViewModes::default(),
            palette: PaletteOverrides::default(),
            gens: VtGenerations::default(),
            dirty_rows: BTreeSet::new(),
            scroll_delta: 0,
            full_dirty: true,
            viewport_hashes: vec![0u64; usize::from(config.size.rows.get())],
            cursor_defaults: None,
        };
        engine.apply_terminal_settings()?;
        if config.cell_px.width > 0 && config.cell_px.height > 0 {
            // SAFETY: handle is live; notifies core of initial pixel geometry.
            let _ = unsafe {
                sys::ghostty_terminal_resize(
                    engine.handle,
                    config.size.cols.get(),
                    config.size.rows.get(),
                    u32::from(config.cell_px.width),
                    u32::from(config.cell_px.height),
                )
            };
        }
        engine.refresh_render_state(true, 0);
        Ok(engine)
    }

    /// Returns a handle to the shared deduplicated [`ImageStore`].
    pub fn image_store(&self) -> &Arc<Mutex<ImageStore>> {
        &self.image_store
    }

    /// Encodes a bounded binary `libghostty-vt` snapshot (at most `max_bytes`) for v2 keyframes.
    pub fn export_snapshot(&self, max_bytes: usize) -> Result<Vec<u8>, GhosttyError> {
        struct CapBuf {
            out: Vec<u8>,
            max: usize,
            cut: bool,
        }
        // SAFETY: called synchronously by `ghostty_snapshot_encode` with `userdata` pointing to `CapBuf`.
        unsafe extern "C" fn write_cb(userdata: *mut std::ffi::c_void, data: *const u8, len: usize) -> bool {
            if userdata.is_null() || (len > 0 && data.is_null()) {
                return false;
            }
            // SAFETY: `userdata` is a valid `&mut CapBuf` alive for the encode call.
            let cap = unsafe { &mut *userdata.cast::<CapBuf>() };
            if cap.cut || len > cap.max.saturating_sub(cap.out.len()) {
                cap.cut = true;
                return false;
            }
            if len > 0 {
                // SAFETY: `data` is non-null and readable for `len` bytes.
                cap.out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
            }
            true
        }
        let mut cap = CapBuf { out: Vec::new(), max: max_bytes, cut: false };
        let writer = sys::Writer { write: Some(write_cb), userdata: (&raw mut cap).cast() };
        // SAFETY: `handle` is live for `&self` and `cap` outlives the synchronous encode call.
        let res = unsafe { sys::ghostty_snapshot_encode(self.handle, writer) };
        if !cap.cut {
            GhosttyError::check("ghostty_snapshot_encode", res)?;
        }
        // Truncate cleanly to the last complete record boundary (10-byte header + [tag:u16, len:u32, crc:u32, payload]).
        let end = last_snapshot_record_end(&cap.out);
        cap.out.truncate(end);
        Ok(cap.out)
    }

    /// Restores terminal state from a binary snapshot produced by [`Self::export_snapshot`],
    /// re-applying all hardened broker settings and bumping `epoch`.
    pub fn restore_snapshot(&mut self, snapshot: &[u8]) -> Result<(), GhosttyError> {
        let mut decoder: sys::SnapshotDecoder = std::ptr::null_mut();
        // SAFETY: `decoder` is a valid out-pointer; `snapshot` outlives the decoder below.
        GhosttyError::check("ghostty_snapshot_decoder_new_buf", unsafe {
            sys::ghostty_snapshot_decoder_new_buf(std::ptr::null(), &raw mut decoder, snapshot.as_ptr(), snapshot.len())
        })?;
        let image_limit: u64 = if self.allow_images { SESSION_IMAGE_STORAGE_BYTES } else { 0 };
        // SAFETY: `KITTY_IMAGE_STORAGE_LIMIT` takes a `uint64_t*`.
        let set_res = unsafe {
            sys::ghostty_snapshot_decoder_set(
                decoder,
                sys::SnapshotDecoderOption::KITTY_IMAGE_STORAGE_LIMIT,
                (&raw const image_limit).cast(),
            )
        };
        if let Err(e) = GhosttyError::check("ghostty_snapshot_decoder_set", set_res) {
            // SAFETY: freeing decoder on setup failure.
            unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
            return Err(e);
        }
        let mut new_handle: sys::Terminal = std::ptr::null_mut();
        // SAFETY: `decoder` is live and `new_handle` is a valid out-pointer.
        let ready_res = unsafe { sys::ghostty_snapshot_decoder_ready(decoder, &raw mut new_handle) };
        if let Err(e) = GhosttyError::check("ghostty_snapshot_decoder_ready", ready_res) {
            // SAFETY: freeing decoder on decode failure.
            unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
            return Err(e);
        }
        // SAFETY: decode all remaining history/image records while both `decoder` and `new_handle` are live.
        // Bounded by `snapshot.len()` so decoding always terminates deterministically.
        for _ in 0..=snapshot.len() {
            if unsafe { sys::ghostty_snapshot_decoder_next(decoder) } != sys::Result::SUCCESS {
                break;
            }
        }
        // SAFETY: decoder is no longer needed once all records are applied.
        unsafe { sys::ghostty_snapshot_decoder_free(decoder) };

        let new_mark = match track_active_top(new_handle) {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: freeing decoded handle if mark creation fails.
                unsafe { sys::ghostty_terminal_free(new_handle) };
                return Err(e);
            }
        };

        // Swap handles safely: free old active_top BEFORE freeing old terminal handle.
        let old_mark = std::mem::replace(&mut self.active_top, new_mark);
        let old_handle = std::mem::replace(&mut self.handle, new_handle);
        // SAFETY: freeing old mark before its owning terminal handle.
        unsafe {
            sys::ghostty_tracked_grid_ref_free(old_mark);
            sys::ghostty_terminal_free(old_handle);
        }

        self.apply_terminal_settings()?;
        let cols = read_u16(self.handle, sys::TerminalData::COLS).unwrap_or(self.size.cols.get());
        let rows = read_u16(self.handle, sys::TerminalData::ROWS).unwrap_or(self.size.rows.get());
        if let Some(gs) = GridSize::new(cols, rows) {
            self.size = gs;
            self.callbacks.cols = cols;
            self.callbacks.rows = rows;
        }
        self.alt_screen = query_alt_screen(self.handle);
        self.scrollback_lines = query_scrollback_rows(self.handle);
        self.first_active_line = if self.alt_screen { 0 } else { self.scrollback_lines };
        self.callbacks.first_active_line = self.first_active_line;
        self.epoch += 1;
        self.full_dirty = true;
        self.dirty_rows.clear();
        self.scroll_delta = 0;
        self.image_cache.borrow_mut().images_gen = 0;
        self.refresh_render_state(true, 0);
        Ok(())
    }

    fn apply_terminal_settings(&mut self) -> Result<(), GhosttyError> {
        let cb_ptr: *mut CallbackState = &mut *self.callbacks;
        // SAFETY: `USERDATA` stores `cb_ptr` which stays pinned at this heap address until after
        // `self.handle` is freed in `Drop`.
        GhosttyError::check("ghostty_terminal_set(USERDATA)", unsafe {
            sys::ghostty_terminal_set(self.handle, sys::TerminalOption::USERDATA, cb_ptr.cast())
        })?;

        for (opt, func, name) in [
            (sys::TerminalOption::WRITE_PTY, cb_write_pty as *const std::ffi::c_void, "WRITE_PTY"),
            (sys::TerminalOption::SEMANTIC_PROMPT, cb_semantic_prompt as *const std::ffi::c_void, "SEMANTIC_PROMPT"),
            (sys::TerminalOption::PWD_CHANGED, cb_pwd_changed as *const std::ffi::c_void, "PWD_CHANGED"),
            (sys::TerminalOption::SIZE, cb_size as *const std::ffi::c_void, "SIZE"),
            (sys::TerminalOption::RENDER_HOLD, cb_render_hold as *const std::ffi::c_void, "RENDER_HOLD"),
            (sys::TerminalOption::TITLE_CHANGED, cb_title_changed as *const std::ffi::c_void, "TITLE_CHANGED"),
            (sys::TerminalOption::BELL, cb_bell as *const std::ffi::c_void, "BELL"),
            (sys::TerminalOption::CLIPBOARD_WRITE, cb_clipboard_write as *const std::ffi::c_void, "CLIPBOARD_WRITE"),
            (
                sys::TerminalOption::DESKTOP_NOTIFICATION,
                cb_notification as *const std::ffi::c_void,
                "DESKTOP_NOTIFICATION",
            ),
            (sys::TerminalOption::PROGRESS_REPORT, cb_progress as *const std::ffi::c_void, "PROGRESS_REPORT"),
            (sys::TerminalOption::PROGRAM_STATUS, cb_program_status as *const std::ffi::c_void, "PROGRAM_STATUS"),
            (sys::TerminalOption::UNKNOWN_SEQUENCE, cb_unknown_sequence as *const std::ffi::c_void, "UNKNOWN_SEQUENCE"),
        ] {
            // SAFETY: each callback matches the exact `extern "C"` signature for its `TerminalOption`.
            GhosttyError::check(name, unsafe { sys::ghostty_terminal_set(self.handle, opt, func) })?;
        }

        let sb_lines = self.scrollback_lines_limit;
        let sb_bytes = usize::MAX;
        let unknown_cap = UNKNOWN_OSC_MAX_BYTES;
        let image_limit: u64 = if self.allow_images { SESSION_IMAGE_STORAGE_BYTES } else { 0 };
        let no_files = false;
        let term_name = sys::String::from(self.terminfo_name.as_str());

        // SAFETY: each option pointer matches the documented C type (`size_t*`, `uint64_t*`, `bool*`, `GhosttyString*`).
        unsafe {
            GhosttyError::check(
                "SCROLLBACK_MAX_LINES",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::SCROLLBACK_MAX_LINES,
                    (&raw const sb_lines).cast(),
                ),
            )?;
            GhosttyError::check(
                "SCROLLBACK_MAX_BYTES",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::SCROLLBACK_MAX_BYTES,
                    (&raw const sb_bytes).cast(),
                ),
            )?;
            GhosttyError::check(
                "UNKNOWN_MAX_BYTES",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::UNKNOWN_MAX_BYTES,
                    (&raw const unknown_cap).cast(),
                ),
            )?;
            GhosttyError::check(
                "KITTY_IMAGE_STORAGE_LIMIT",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT,
                    (&raw const image_limit).cast(),
                ),
            )?;
            // Hostile-PTY security invariant: VT core NEVER opens files or shared memory objects!
            GhosttyError::check(
                "KITTY_IMAGE_MEDIUM_FILE",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::KITTY_IMAGE_MEDIUM_FILE,
                    (&raw const no_files).cast(),
                ),
            )?;
            GhosttyError::check(
                "KITTY_IMAGE_MEDIUM_SHARED_MEM",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::KITTY_IMAGE_MEDIUM_SHARED_MEM,
                    (&raw const no_files).cast(),
                ),
            )?;
            GhosttyError::check(
                "TERMINFO_NAME",
                sys::ghostty_terminal_set(
                    self.handle,
                    sys::TerminalOption::TERMINFO_NAME,
                    (&raw const term_name).cast(),
                ),
            )?;
        }

        if let Some((shape, blinking)) = self.cursor_defaults {
            self.apply_cursor_defaults(shape, blinking);
        }
        Ok(())
    }

    fn apply_cursor_defaults(&self, shape: CursorShape, blinking: bool) {
        let style = match shape {
            CursorShape::Block => sys::CursorStyle::BLOCK,
            CursorShape::Hollow => sys::CursorStyle::BLOCK_HOLLOW,
            CursorShape::Bar => sys::CursorStyle::BAR,
            CursorShape::Underline => sys::CursorStyle::UNDERLINE,
        };
        // SAFETY: `DEFAULT_CURSOR_STYLE` takes `GhosttyCursorStyle*` and `DEFAULT_CURSOR_BLINK` takes `bool*`.
        unsafe {
            let _ = sys::ghostty_terminal_set(
                self.handle,
                sys::TerminalOption::DEFAULT_CURSOR_STYLE,
                (&raw const style).cast(),
            );
            let _ = sys::ghostty_terminal_set(
                self.handle,
                sys::TerminalOption::DEFAULT_CURSOR_BLINK,
                (&raw const blinking).cast(),
            );
        }
    }

    fn update_scroll_and_screen(&mut self, events: &mut Vec<VtEvent>) -> u64 {
        let after_alt = query_alt_screen(self.handle);
        let after_sb = query_scrollback_rows(self.handle);
        if after_alt != self.alt_screen {
            self.alt_screen = after_alt;
            self.scrollback_lines = after_sb;
            self.first_active_line = if after_alt { 0 } else { after_sb };
            self.callbacks.first_active_line = self.first_active_line;
            self.epoch += 1;
            self.full_dirty = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            self.reset_active_top_mark();
            events.push(VtEvent::EpochChanged);
            return 0;
        }

        // Measure exact lines scrolled up since last step using `active_top` TrackedGridRef.
        let scrolled_up: u64 = match tracked_screen_row(self.active_top) {
            Some(row) => after_sb.saturating_sub(u64::from(row)),
            None => {
                // Active top scrolled off the entire scrollback limit -> force full resync.
                self.full_dirty = true;
                after_sb.saturating_sub(self.scrollback_lines).max(u64::from(self.size.rows.get()))
            }
        };
        self.scrollback_lines = after_sb;
        let mut applied_scroll = 0u64;
        if scrolled_up != 0 {
            self.first_active_line = self.first_active_line.saturating_add(scrolled_up);
            self.callbacks.first_active_line = self.first_active_line;
            if scrolled_up >= u64::from(self.size.rows.get()) {
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

    fn reset_active_top_mark(&mut self) {
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

    fn refresh_render_state(&mut self, force_full: bool, scrolled_up: u64) {
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
            self.gens.grid += 1;
            for r in 0..rows_count {
                self.viewport_hashes[r] = hash_viewport_row(self.handle, r as u32, cols_count);
            }
        } else if dirty == sys::RenderStateDirty::FULL {
            // `libghostty-vt` reports FULL on viewport scroll; preserve scroll-blit delta by
            // shifting our per-row hashes and marking only newly revealed or modified rows dirty.
            let shift = (scrolled_up as usize).min(rows_count);
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

        self.gens.images = query_images_generation(self.handle);
    }

    fn sync_images_cache(&self) {
        let cur_gen = query_images_generation(self.handle);
        let mut cache = self.image_cache.borrow_mut();
        if cache.images_gen == cur_gen {
            return;
        }
        cache.images_gen = cur_gen;
        cache.virtual_named.clear();
        cache.virtual_lowest.clear();
        let Some(graphics) = query_graphics(self.handle) else {
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
        if filled == sys::Result::SUCCESS {
            let mut store = self.image_store.lock().expect("ImageStore mutex poisoned");
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
                sync_single_image(graphics, image_id, &mut store, &mut cache.blobs);

                let mut is_virtual = false;
                // SAFETY: `IS_VIRTUAL` writes a `bool`.
                let _ = unsafe {
                    sys::ghostty_kitty_graphics_placement_get(
                        iter,
                        sys::KittyGraphicsPlacementData::IS_VIRTUAL,
                        (&raw mut is_virtual).cast(),
                    )
                };
                if is_virtual {
                    let pid = read_placement_u32(iter, sys::KittyGraphicsPlacementData::PLACEMENT_ID).unwrap_or(0);
                    let cols = read_placement_u32(iter, sys::KittyGraphicsPlacementData::COLUMNS).unwrap_or(0);
                    let rows = read_placement_u32(iter, sys::KittyGraphicsPlacementData::ROWS).unwrap_or(0);
                    let grid = (cols, rows);
                    cache.virtual_named.insert((image_id, pid), grid);
                    cache
                        .virtual_lowest
                        .entry(image_id)
                        .and_modify(|e| *e = (*e).min((pid, grid)))
                        .or_insert((pid, grid));
                }
            }
        }
        // SAFETY: freeing iterator once.
        unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iter) };
    }

    fn resolve_virtual(&self, image_id: u32, placement_id: u32) -> Option<VirtualImageInfo> {
        self.sync_images_cache();
        let graphics = query_graphics(self.handle)?;
        // SAFETY: `graphics` is live for `&self`.
        let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, image_id) };
        if img.is_null() {
            return None;
        }
        let img_gen = query_image_generation(img);
        let cache = self.image_cache.borrow();
        let &(blob, w, h) = cache.blobs.get(&(image_id, img_gen))?;
        let grid = match placement_id {
            0 => cache.virtual_lowest.get(&image_id).map(|(_, g)| *g)?,
            pid => *cache.virtual_named.get(&(image_id, pid))?,
        };
        Some(VirtualImageInfo { blob, picture_px: (w, h), grid })
    }

    fn epoch_to_screen_row(&self, abs_line: u64) -> Option<u32> {
        let oldest = self.first_active_line.saturating_sub(self.scrollback_lines);
        let total = self.first_active_line.saturating_add(u64::from(self.size.rows.get()));
        if abs_line < oldest || abs_line >= total {
            return None;
        }
        u32::try_from(abs_line - oldest).ok()
    }

    fn screen_row_to_epoch(&self, screen_y: u32) -> u64 {
        let oldest = self.first_active_line.saturating_sub(self.scrollback_lines);
        oldest.saturating_add(u64::from(screen_y))
    }

    fn screen_grid_ref(&self, pos: CellPos) -> Option<sys::GridRef> {
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

    fn selection_to_span(&self, sel: &sys::Selection) -> Option<CellSpan> {
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

impl Drop for GhosttyEngine {
    fn drop(&mut self) {
        // SAFETY: free every dependent handle strictly BEFORE freeing `self.handle` (and before
        // `self.callbacks` drops automatically afterwards).
        unsafe {
            sys::ghostty_tracked_grid_ref_free(self.active_top);
            sys::ghostty_render_state_row_iterator_free(self.render_rows);
            sys::ghostty_render_state_free(self.render_state);
            sys::ghostty_terminal_free(self.handle);
        }
    }
}

impl VtEngine for GhosttyEngine {
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) {
        let mut rest = bytes;
        let mut total_scrolled = 0u64;
        while !rest.is_empty() {
            let mut stop =
                sys::TerminalWriteStop { size: std::mem::size_of::<sys::TerminalWriteStop>(), ..Default::default() };
            // SAFETY: `self.handle` is live for `&mut self`, `rest` outlives the call, `stop` is sized.
            let res = unsafe {
                sys::ghostty_terminal_vt_write_until(
                    self.handle,
                    rest.as_ptr(),
                    rest.len(),
                    sys::TerminalBoundary::OSC_END,
                    &raw mut stop,
                )
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

            // Synchronize scroll / alt-screen epoch BEFORE stamping cursor_line on callbacks from this slice!
            let mut pending = std::mem::take(&mut self.callbacks.events);
            total_scrolled = total_scrolled.saturating_add(self.update_scroll_and_screen(events));
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
            events.extend(pending);
        }
        if !bytes.is_empty() {
            self.refresh_render_state(false, total_scrolled);
        }
    }

    fn resize(&mut self, size: GridSize, cell_px: CellPx) {
        let grid_changed = size != self.size;
        if !grid_changed && cell_px == self.cell_px {
            return;
        }
        self.size = size;
        self.cell_px = cell_px;
        self.callbacks.cols = size.cols.get();
        self.callbacks.rows = size.rows.get();
        self.callbacks.cell_px = cell_px;
        // SAFETY: `self.handle` is live for `&mut self`.
        let _ = unsafe {
            sys::ghostty_terminal_resize(
                self.handle,
                size.cols.get(),
                size.rows.get(),
                u32::from(cell_px.width),
                u32::from(cell_px.height),
            )
        };
        if grid_changed {
            self.epoch += 1;
            self.scrollback_lines = query_scrollback_rows(self.handle);
            self.first_active_line = if self.alt_screen { 0 } else { self.scrollback_lines };
            self.callbacks.first_active_line = self.first_active_line;
            self.full_dirty = true;
            self.dirty_rows.clear();
            self.scroll_delta = 0;
            self.reset_active_top_mark();
            self.refresh_render_state(true, 0);
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

    fn word_at(&self, pos: CellPos, _delimiters: &str) -> Option<CellSpan> {
        let grid_ref = self.screen_grid_ref(pos)?;
        let opts = sys::TerminalSelectWordOptions {
            size: std::mem::size_of::<sys::TerminalSelectWordOptions>(),
            ref_: grid_ref,
            ..Default::default()
        };
        let mut sel = sys::Selection::default();
        // SAFETY: `opts` is sized and `sel` is a valid out-pointer on live `self.handle`.
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

    fn text_of(&self, span: CellSpan) -> String {
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
        // SAFETY: `SELECTION` takes `GhosttySelection*`, `UNWRAP` takes `bool*`; buffer freed below.
        let res = unsafe {
            let mut r = sys::ghostty_formatter_set(formatter, sys::FormatterOption::SELECTION, (&raw const sel).cast());
            if r == sys::Result::SUCCESS {
                r = sys::ghostty_formatter_set(formatter, sys::FormatterOption::UNWRAP, (&raw const unwrap).cast());
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

    fn text_of_rect(&self, span: CellSpan) -> String {
        let lo_col = span.start.col.min(span.end.col);
        let hi_col = span.start.col.max(span.end.col).min(self.size.cols.get().saturating_sub(1));
        let mut out = String::new();
        for (i, line) in (span.start.line..=span.end.line).enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let Some(screen_y) = self.epoch_to_screen_row(line) else {
                continue;
            };
            let mut row_text = String::new();
            for col in lo_col..=hi_col {
                let point = sys::Point {
                    tag: sys::PointTag::SCREEN,
                    value: sys::PointValue { coordinate: sys::PointCoordinate { x: col, y: screen_y } },
                };
                let mut grid_ref = sys::GridRef::default();
                // SAFETY: `grid_ref` is a valid out-pointer on live `self.handle`.
                if unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut grid_ref) }
                    != sys::Result::SUCCESS
                {
                    continue;
                }
                let mut cell: sys::Cell = 0;
                // SAFETY: `grid_ref` was just resolved.
                if unsafe { sys::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut cell) } != sys::Result::SUCCESS {
                    continue;
                }
                let mut cp: u32 = 0;
                // SAFETY: `CODEPOINT` writes a `uint32_t`.
                let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::CODEPOINT, (&raw mut cp).cast()) };
                row_text.push(char::from_u32(cp).filter(|&c| c != '\0').unwrap_or(' '));
            }
            out.push_str(row_text.trim_end_matches(' '));
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
        if needle.is_empty() || limit == 0 {
            return false;
        }
        let mut search: sys::Search = std::ptr::null_mut();
        // SAFETY: `search` is a valid out-pointer; freed unconditionally below.
        if unsafe { sys::ghostty_search_new(std::ptr::null(), &raw mut search, self.handle) } != sys::Result::SUCCESS {
            return false;
        }
        let text = sys::String::from(needle);
        let mut raw_matches: Vec<sys::Selection> = Vec::new();
        let wanted = SEARCH_CANDIDATES.max(limit);
        let mut complete = false;
        let max_passes = (self.scrollback_lines as usize).saturating_add(usize::from(self.size.rows.get())).max(64);
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
                raw_matches.resize(total, sys::Selection::default());
                let mut buf = sys::SelectionBuffer { ptr: raw_matches.as_mut_ptr(), cap: raw_matches.len(), len: 0 };
                r = sys::ghostty_search_get(search, sys::SearchData::MATCHES, (&raw mut buf).cast());
                raw_matches.truncate(if r == sys::Result::SUCCESS { buf.len.min(raw_matches.len()) } else { 0 });
            }
            r
        };
        // SAFETY: freeing search handle once.
        unsafe { sys::ghostty_search_free(search) };
        if res != sys::Result::SUCCESS {
            return false;
        }

        let mut formatter: sys::Formatter = std::ptr::null_mut();
        if case_sensitive && !raw_matches.is_empty() {
            // SAFETY: creating a single reused formatter for exact-case filtering; freed below.
            let _ = unsafe { sys::ghostty_formatter_terminal_new(std::ptr::null(), &raw mut formatter, self.handle) };
            if !formatter.is_null() {
                let unwrap = true;
                // SAFETY: `UNWRAP` takes a `bool*`.
                let _ = unsafe {
                    sys::ghostty_formatter_set(formatter, sys::FormatterOption::UNWRAP, (&raw const unwrap).cast())
                };
            }
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

    fn images(&self) -> Vec<ImagePlacement> {
        self.sync_images_cache();
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
            let cache = self.image_cache.borrow();
            // SAFETY: iterating placements while `self.handle` is borrowed immutably.
            while unsafe { sys::ghostty_kitty_graphics_placement_next(iter) } {
                let Some(id) = read_placement_u32(iter, sys::KittyGraphicsPlacementData::IMAGE_ID) else {
                    continue;
                };
                // SAFETY: `graphics` is live for `&self`.
                let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, id) };
                if img.is_null() {
                    continue;
                }
                let img_gen = query_image_generation(img);
                let Some(&(blob, w, h)) = cache.blobs.get(&(id, img_gen)) else {
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
                let mut z: i32 = 0;
                // SAFETY: `Z` writes an `int32_t`.
                let _ = unsafe {
                    sys::ghostty_kitty_graphics_placement_get(
                        iter,
                        sys::KittyGraphicsPlacementData::Z,
                        (&raw mut z).cast(),
                    )
                };
                out.push(ImagePlacement {
                    blob,
                    line: span.start.line,
                    col: i32::from(span.start.col),
                    pixel_size: [w, h],
                    cell_span: [cols.min(u32::from(u16::MAX)) as u16, rows.min(u32::from(u16::MAX)) as u16],
                    z_index: z,
                });
            }
        }
        // SAFETY: freeing placement iterator once.
        unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iter) };
        out
    }

    fn encode_key(&mut self, key: &KeyInput, out: &mut Vec<u8>) {
        self.encoders.encode_key(self.handle, key, out);
    }

    fn encode_pointer(&mut self, ev: &PointerInput, out: &mut Vec<u8>) {
        self.encoders.encode_pointer(self.handle, self.size, ev, out);
    }

    fn encode_focus(&mut self, focused: bool, out: &mut Vec<u8>) {
        encode_focus(self.handle, focused, out);
    }

    fn encode_paste(&mut self, text: &str, allow_unsafe: bool) -> PasteEncode {
        encode_paste(self.handle, text, allow_unsafe)
    }

    fn set_cursor_defaults(&mut self, shape: CursorShape, blinking: bool) {
        self.cursor_defaults = Some((shape, blinking));
        self.apply_cursor_defaults(shape, blinking);
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
}

// --- C callback trampolines ---

// SAFETY: `libghostty-vt` calls this synchronously during `vt_write` with `userdata` set to our
// pinned `CallbackState` and `data` readable for `len` bytes.
unsafe extern "C" fn cb_write_pty(_term: sys::Terminal, userdata: *mut std::ffi::c_void, data: *const u8, len: usize) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState`; `data` is readable for `len` bytes.
    let (state, bytes) = unsafe { (&mut *userdata.cast::<CallbackState>(), std::slice::from_raw_parts(data, len)) };
    state.events.push(VtEvent::DeviceResponse(bytes.to_vec()));
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `event`.
unsafe extern "C" fn cb_semantic_prompt(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    event: *const sys::TerminalSemanticPrompt,
) {
    if userdata.is_null() || event.is_null() {
        return;
    }
    // SAFETY: both pointers are valid for the duration of this synchronous callback.
    let (state, ev) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*event) };
    let line = state.first_active_line;
    match ev.kind {
        sys::SemanticPromptKind::GHOSTTY_SEMANTIC_PROMPT_PROMPT_START => {
            if ev.prompt_kind == sys::SemanticPromptPromptKind::GHOSTTY_SEMANTIC_PROMPT_PROMPT_PRIMARY {
                state.events.push(VtEvent::Osc133 { payload: String::from("A"), cursor_line: line });
            }
        }
        sys::SemanticPromptKind::GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => {
            // SAFETY: `ev.command` is owned by `libghostty-vt` for this callback and copied out.
            if let Some(cmd) = unsafe { copy_ghostty_string(&ev.command) } {
                state.events.push(VtEvent::Osc133 { payload: format!("E;{cmd}"), cursor_line: line });
            }
            state.events.push(VtEvent::Osc133 { payload: String::from("C"), cursor_line: line });
        }
        sys::SemanticPromptKind::GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => {
            let payload = if ev.has_exit_code { format!("D;{}", ev.exit_code) } else { String::from("D") };
            state.events.push(VtEvent::Osc133 { payload, cursor_line: line });
        }
        _ => {}
    }
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `out_size`.
unsafe extern "C" fn cb_size(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    out_size: *mut sys::SizeReportSize,
) -> bool {
    if userdata.is_null() || out_size.is_null() {
        return false;
    }
    // SAFETY: both pointers are valid for this call.
    let (state, out) = unsafe { (&*userdata.cast::<CallbackState>(), &mut *out_size) };
    if state.cell_px.width == 0 || state.cell_px.height == 0 {
        return false;
    }
    out.rows = state.rows;
    out.columns = state.cols;
    out.cell_width = u32::from(state.cell_px.width);
    out.cell_height = u32::from(state.cell_px.height);
    true
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and live `term`.
unsafe extern "C" fn cb_pwd_changed(term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    if userdata.is_null() {
        return;
    }
    let mut val = sys::String::default();
    // SAFETY: `PWD` writes a `GhosttyString` valid during this callback.
    if unsafe { sys::ghostty_terminal_get(term, sys::TerminalData::PWD, (&raw mut val).cast()) } != sys::Result::SUCCESS
    {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState` and `val` was just populated.
    let (state, text) = unsafe { (&mut *userdata.cast::<CallbackState>(), copy_ghostty_string(&val)) };
    if let Some(raw) = text {
        state.events.push(VtEvent::Cwd(decode_osc7_cwd(&raw)));
    }
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and live `term`.
unsafe extern "C" fn cb_title_changed(term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    if userdata.is_null() {
        return;
    }
    let mut val = sys::String::default();
    // SAFETY: `TITLE` writes a `GhosttyString` valid during this callback.
    if unsafe { sys::ghostty_terminal_get(term, sys::TerminalData::TITLE, (&raw mut val).cast()) }
        != sys::Result::SUCCESS
    {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState` and `val` was just populated.
    let (state, title) = unsafe { (&mut *userdata.cast::<CallbackState>(), copy_ghostty_string(&val)) };
    state.events.push(VtEvent::Title(title.unwrap_or_default()));
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState`.
unsafe extern "C" fn cb_bell(_term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState`.
    unsafe { &mut *userdata.cast::<CallbackState>() }.events.push(VtEvent::Bell);
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `write`.
unsafe extern "C" fn cb_clipboard_write(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    write: *const sys::ClipboardWrite,
) {
    if userdata.is_null() || write.is_null() {
        return;
    }
    // SAFETY: both pointers are valid for this synchronous callback.
    let (state, req) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*write) };
    if !req.contents.is_null() && req.location == sys::ClipboardLocation::STANDARD {
        // SAFETY: `req.contents` has `req.contents_len` entries for this call.
        let contents = unsafe { std::slice::from_raw_parts(req.contents, req.contents_len) };
        let text = contents.iter().find_map(|c| {
            // SAFETY: each `GhosttyString` is valid for this callback and copied out.
            let mime = unsafe { copy_ghostty_string(&c.mime) }?;
            // SAFETY: `c.data` has the same callback lifetime.
            mime.starts_with("text/").then(|| unsafe { copy_ghostty_string(&c.data) }.unwrap_or_default())
        });
        if let Some(data) = text {
            state.events.push(VtEvent::Clipboard { selection: String::from("c"), data });
        }
    }
    if let Some(reply) = req.reply {
        let answer = sys::ClipboardWriteReply {
            size: std::mem::size_of::<sys::ClipboardWriteReply>(),
            result: sys::ClipboardWriteResult::SUCCESS,
            remember: false,
        };
        // SAFETY: invoking the callback-supplied reply function with a sized struct.
        unsafe { reply(write, &raw const answer) };
    }
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `note`.
unsafe extern "C" fn cb_notification(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    note: *const sys::TerminalDesktopNotification,
) {
    if userdata.is_null() || note.is_null() {
        return;
    }
    // SAFETY: both pointers are valid for this call and strings are copied out.
    let (state, title, body) = unsafe {
        let n = &*note;
        (&mut *userdata.cast::<CallbackState>(), copy_ghostty_string(&n.title), copy_ghostty_string(&n.body))
    };
    state.events.push(VtEvent::Notification { title: title.unwrap_or_default(), body: body.unwrap_or_default() });
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `report`.
unsafe extern "C" fn cb_progress(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    report: *const sys::TerminalProgressReport,
) {
    if userdata.is_null() || report.is_null() {
        return;
    }
    // SAFETY: both pointers are valid for this call.
    let (state, rep) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*report) };
    let progress_state = match rep.state {
        sys::TerminalProgressState::SET => ProgressState::Running,
        sys::TerminalProgressState::ERROR => ProgressState::Error,
        sys::TerminalProgressState::INDETERMINATE => ProgressState::Indeterminate,
        sys::TerminalProgressState::PAUSE => ProgressState::Paused,
        _ => ProgressState::None,
    };
    state.events.push(VtEvent::Progress { state: progress_state, percent: u8::try_from(rep.progress).ok() });
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `report`.
unsafe extern "C" fn cb_program_status(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    report: *const sys::TerminalProgramStatus,
) {
    if userdata.is_null() || report.is_null() {
        return;
    }
    // SAFETY: both pointers are valid for this call.
    let (state, rep) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*report) };
    let state_str = match rep.state {
        sys::ProgramStatusState::IDLE => "idle",
        sys::ProgramStatusState::WORKING => "working",
        sys::ProgramStatusState::DONE => "done",
        sys::ProgramStatusState::BLOCKED => "blocked",
        sys::ProgramStatusState::ERROR => "error",
        sys::ProgramStatusState::CLEAR => "clear",
        _ => return,
    };
    let mut parts = vec![format!("state={state_str}")];
    let kind_str = match rep.kind {
        sys::ProgramStatusKind::PERMISSION => Some("permission"),
        sys::ProgramStatusKind::QUESTION => Some("question"),
        sys::ProgramStatusKind::AUTH => Some("auth"),
        _ => None,
    };
    if let Some(k) = kind_str {
        parts.push(format!("kind={k}"));
    }
    if let Ok(p) = u8::try_from(rep.progress) {
        parts.push(format!("progress={p}"));
    }
    // SAFETY: string slices are valid for this callback and copied out.
    let (id, app, title, msg) = unsafe {
        (
            copy_ghostty_string(&rep.id),
            copy_ghostty_string(&rep.app),
            copy_ghostty_string(&rep.title),
            copy_ghostty_string(&rep.message),
        )
    };
    if let Some(v) = id {
        parts.push(format!("id={v}"));
    }
    if let Some(v) = app {
        parts.push(format!("app={v}"));
    }
    if let Some(v) = title {
        parts.push(format!("title={v}"));
    }
    if let Some(v) = msg {
        parts.push(format!("message={v}"));
    }
    state.events.push(VtEvent::Osc7501(parts.join(";")));
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState`.
unsafe extern "C" fn cb_render_hold(_term: sys::Terminal, userdata: *mut std::ffi::c_void, held: bool) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState`.
    unsafe { &mut *userdata.cast::<CallbackState>() }.events.push(VtEvent::SyncOutput(held));
}

// SAFETY: `libghostty-vt` calls this synchronously with our pinned `CallbackState` and valid `seq`.
unsafe extern "C" fn cb_unknown_sequence(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    seq: *const sys::TerminalUnknownSequence,
) {
    if userdata.is_null() || seq.is_null() {
        return;
    }
    // SAFETY: both pointers are valid for this call.
    let (state, seq) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*seq) };
    if seq.tag != sys::TerminalUnknownSequenceTag::GHOSTTY_TERMINAL_UNKNOWN_SEQUENCE_OSC {
        return;
    }
    // SAFETY: `tag == OSC` guarantees `seq.value.osc` is initialized.
    let osc = unsafe { &seq.value.osc };
    if osc.content.ptr.is_null() || osc.content.len == 0 {
        return;
    }
    // SAFETY: `osc.content` points to `len` readable bytes for this callback.
    let raw = unsafe { std::slice::from_raw_parts(osc.content.ptr, osc.content.len) };
    let text = String::from_utf8_lossy(raw);
    if let Some(body) = text.strip_prefix("633;") {
        // Claims cut short by `UNKNOWN_MAX_BYTES` are dropped; keyed `V;` marks keep their
        // authenticated prefix (`V;<kind>;<seq>;<key>;...`).
        if osc.truncated && !body.starts_with("V;") {
            return;
        }
        state.events.push(VtEvent::Osc633 { payload: body.to_owned(), cursor_line: state.first_active_line });
    }
}

/// Copies a `libghostty-vt` `GhosttyString` into an owned UTF-8 `String`.
///
/// # Safety
///
/// `text` must point to live memory owned by `libghostty-vt` for the duration of this call.
unsafe fn copy_ghostty_string(text: &sys::String) -> Option<String> {
    if text.ptr.is_null() || text.len == 0 {
        return None;
    }
    // SAFETY: caller guarantees `ptr` is readable for `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(text.ptr, text.len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

// --- Helper FFI queries ---

fn hash_viewport_row(handle: sys::Terminal, row: u32, cols: u16) -> u64 {
    let point = sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: row } },
    };
    let mut grid_ref = sys::GridRef::default();
    // SAFETY: `grid_ref` is a valid out-pointer on live `handle`.
    if unsafe { sys::ghostty_terminal_grid_ref(handle, point, &raw mut grid_ref) } != sys::Result::SUCCESS {
        return 0;
    }
    let mut raw_row: sys::Row = 0;
    let mut wrapped = false;
    let mut prompt: sys::RowSemanticPrompt::Type = sys::RowSemanticPrompt::NONE;
    // SAFETY: `grid_ref` was just populated; `WRAP` writes `bool` and `SEMANTIC_PROMPT` writes `RowSemanticPrompt`.
    unsafe {
        if sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw_row) == sys::Result::SUCCESS {
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::WRAP, (&raw mut wrapped).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::SEMANTIC_PROMPT, (&raw mut prompt).cast());
        }
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325u64
        .wrapping_add(u64::from(wrapped))
        .wrapping_mul(0x0000_0100_0000_01b3)
        .wrapping_add(u64::from(prompt));
    for col in 0..cols {
        grid_ref.x = col;
        let mut cell: sys::Cell = 0;
        // SAFETY: `grid_ref` names a valid column in the resolved row.
        if unsafe { sys::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut cell) } == sys::Result::SUCCESS {
            h = h.wrapping_mul(0x0000_0100_0000_01b3).wrapping_add(cell);
        }
    }
    h
}

fn track_active_top(handle: sys::Terminal) -> Result<sys::TrackedGridRef, GhosttyError> {
    let point = sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: 0 } },
    };
    let mut mark: sys::TrackedGridRef = std::ptr::null_mut();
    // SAFETY: `handle` is live and `mark` is a valid out-pointer.
    let res = unsafe { sys::ghostty_terminal_grid_ref_track(handle, point, &raw mut mark) };
    GhosttyError::check("ghostty_terminal_grid_ref_track", res)?;
    if mark.is_null() {
        return Err(GhosttyError { call: "ghostty_terminal_grid_ref_track", code: sys::Result::OUT_OF_MEMORY });
    }
    Ok(mark)
}

fn tracked_screen_row(mark: sys::TrackedGridRef) -> Option<u32> {
    let mut pt = sys::PointCoordinate { x: 0, y: 0 };
    // SAFETY: `mark` is live and `pt` is a valid out-pointer.
    let res = unsafe { sys::ghostty_tracked_grid_ref_point(mark, sys::PointTag::SCREEN, &raw mut pt) };
    (res == sys::Result::SUCCESS).then_some(pt.y)
}

fn read_u16(handle: sys::Terminal, key: sys::TerminalData::Type) -> Option<u16> {
    let mut val = 0u16;
    // SAFETY: caller passes only `u16`-typed `TerminalData` keys (`COLS`, `ROWS`, `CURSOR_X`, `CURSOR_Y`).
    let res = unsafe { sys::ghostty_terminal_get(handle, key, (&raw mut val).cast()) };
    (res == sys::Result::SUCCESS).then_some(val)
}

fn query_alt_screen(handle: sys::Terminal) -> bool {
    let mut screen: sys::TerminalScreen::Type = sys::TerminalScreen::PRIMARY;
    // SAFETY: `ACTIVE_SCREEN` writes a `GhosttyTerminalScreen`.
    let res = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::ACTIVE_SCREEN, (&raw mut screen).cast()) };
    res == sys::Result::SUCCESS && screen == sys::TerminalScreen::ALTERNATE
}

fn query_scrollback_rows(handle: sys::Terminal) -> u64 {
    let mut rows: usize = 0;
    // SAFETY: `SCROLLBACK_ROWS` writes a `size_t`.
    let res = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::SCROLLBACK_ROWS, (&raw mut rows).cast()) };
    if res == sys::Result::SUCCESS { rows as u64 } else { 0 }
}

fn query_mouse_tracking(handle: sys::Terminal) -> bool {
    let mut tracking = false;
    // SAFETY: `MOUSE_TRACKING` writes a `bool`.
    if unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::MOUSE_TRACKING, (&raw mut tracking).cast()) }
        == sys::Result::SUCCESS
    {
        tracking
    } else {
        false
    }
}

fn query_color(handle: sys::Terminal, key: sys::TerminalData::Type) -> Option<Rgb> {
    let mut c = sys::ColorRgb { r: 0, g: 0, b: 0 };
    // SAFETY: colour keys write a `GhosttyColorRgb`.
    let res = unsafe { sys::ghostty_terminal_get(handle, key, (&raw mut c).cast()) };
    (res == sys::Result::SUCCESS).then_some(Rgb([c.r, c.g, c.b]))
}

fn query_palette_overrides(handle: sys::Terminal) -> PaletteOverrides {
    use sys::TerminalData as D;
    let diff = |now_k: D::Type, def_k: D::Type| {
        let now = query_color(handle, now_k)?;
        (Some(now) != query_color(handle, def_k)).then_some(now)
    };
    let mut now_pal = [sys::ColorRgb { r: 0, g: 0, b: 0 }; 256];
    let mut def_pal = [sys::ColorRgb { r: 0, g: 0, b: 0 }; 256];
    // SAFETY: `COLOR_PALETTE` and `COLOR_PALETTE_DEFAULT` each write 256 `GhosttyColorRgb` entries.
    let _ = unsafe {
        let _ = sys::ghostty_terminal_get(handle, D::COLOR_PALETTE, now_pal.as_mut_ptr().cast());
        sys::ghostty_terminal_get(handle, D::COLOR_PALETTE_DEFAULT, def_pal.as_mut_ptr().cast())
    };
    let palette = (0..=u8::MAX)
        .zip(now_pal.iter().zip(def_pal.iter()))
        .filter(|(_, (n, d))| (n.r, n.g, n.b) != (d.r, d.g, d.b))
        .map(|(idx, (n, _))| (idx, Rgb([n.r, n.g, n.b])))
        .collect();
    PaletteOverrides {
        fg: diff(D::COLOR_FOREGROUND, D::COLOR_FOREGROUND_DEFAULT),
        bg: diff(D::COLOR_BACKGROUND, D::COLOR_BACKGROUND_DEFAULT),
        cursor: diff(D::COLOR_CURSOR, D::COLOR_CURSOR_DEFAULT),
        palette,
    }
}

fn query_graphics(handle: sys::Terminal) -> Option<sys::KittyGraphics> {
    let mut graphics: sys::KittyGraphics = std::ptr::null_mut();
    // SAFETY: `KITTY_GRAPHICS` writes a `GhosttyKittyGraphics`.
    let _ = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::KITTY_GRAPHICS, (&raw mut graphics).cast()) };
    (!graphics.is_null()).then_some(graphics)
}

fn query_images_generation(handle: sys::Terminal) -> u64 {
    let Some(graphics) = query_graphics(handle) else { return 0 };
    let mut generation: u64 = 0;
    // SAFETY: `GENERATION` writes a `uint64_t`.
    let res = unsafe {
        sys::ghostty_kitty_graphics_get(graphics, sys::KittyGraphicsData::GENERATION, (&raw mut generation).cast())
    };
    if res == sys::Result::SUCCESS { generation } else { 0 }
}

fn query_image_generation(img: sys::KittyGraphicsImage) -> u64 {
    let mut generation: u64 = 0;
    // SAFETY: `GENERATION` writes a `uint64_t`.
    let _ = unsafe {
        sys::ghostty_kitty_graphics_image_get(
            img,
            sys::KittyGraphicsImageData::GENERATION,
            (&raw mut generation).cast(),
        )
    };
    generation
}

fn read_placement_u32(
    iter: sys::KittyGraphicsPlacementIterator,
    key: sys::KittyGraphicsPlacementData::Type,
) -> Option<u32> {
    let mut val = 0u32;
    // SAFETY: caller passes only `uint32_t`-typed placement keys.
    let res = unsafe { sys::ghostty_kitty_graphics_placement_get(iter, key, (&raw mut val).cast()) };
    (res == sys::Result::SUCCESS).then_some(val)
}

fn sync_single_image(
    graphics: sys::KittyGraphics,
    image_id: u32,
    store: &mut ImageStore,
    blobs: &mut HashMap<(u32, u64), (BlobHash, u32, u32)>,
) {
    // SAFETY: `graphics` is live for this call.
    let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, image_id) };
    if img.is_null() {
        return;
    }
    let img_gen = query_image_generation(img);
    if blobs.contains_key(&(image_id, img_gen)) {
        return;
    }
    let mut width = 0u32;
    let mut height = 0u32;
    let mut format: sys::KittyImageFormat::Type = sys::KittyImageFormat::RGBA;
    let mut pixels = sys::String::default();
    // SAFETY: each out-pointer matches its documented `KittyGraphicsImageData` type.
    unsafe {
        let _ = sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::WIDTH, (&raw mut width).cast());
        let _ =
            sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::HEIGHT, (&raw mut height).cast());
        let _ =
            sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::FORMAT, (&raw mut format).cast());
        let _ =
            sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::PIXELS, (&raw mut pixels).cast());
    }
    if pixels.ptr.is_null() {
        return;
    }
    let channels = match format {
        sys::KittyImageFormat::RGB => 3,
        sys::KittyImageFormat::RGBA => 4,
        _ => return,
    };
    // SAFETY: `pixels.ptr` is non-null and readable for `pixels.len` bytes while terminal is immutable.
    let slice = unsafe { std::slice::from_raw_parts(pixels.ptr, pixels.len) };
    if let Some(hash) = store.insert_pixels(width, height, channels, slice) {
        blobs.insert((image_id, img_gen), (hash, width, height));
    }
}

fn selection_matches_exact(formatter: sys::Formatter, sel: &sys::Selection, needle: &[u8]) -> bool {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `formatter` is live; `SELECTION` copies `sel`; output buffer freed below.
    let res = unsafe {
        let mut r =
            sys::ghostty_formatter_set(formatter, sys::FormatterOption::SELECTION, std::ptr::from_ref(sel).cast());
        if r == sys::Result::SUCCESS {
            r = sys::ghostty_formatter_format_alloc(formatter, std::ptr::null(), &raw mut ptr, &raw mut len);
        }
        r
    };
    let matched = match NonNull::new(ptr).filter(|_| res == sys::Result::SUCCESS) {
        None => needle.is_empty(),
        // SAFETY: `ptr` is non-null and initialized for `len` bytes.
        Some(nn) => unsafe { std::slice::from_raw_parts(nn.as_ptr(), len) == needle },
    };
    // SAFETY: `ghostty_free` accepts null pointers.
    unsafe { sys::ghostty_free(std::ptr::null(), ptr, len) };
    matched
}

/// Decodes OSC 7 `file://host/path` percent-escapes or returns bare paths verbatim.
pub(crate) fn decode_osc7_cwd(raw: &str) -> String {
    let path = if let Some(rest) = raw.strip_prefix("file://")
        && let Some(slash) = rest.find('/')
    {
        &rest[slash..]
    } else {
        raw
    };
    percent_decode(path)
}

fn percent_decode(text: &str) -> String {
    let mut rest = text.as_bytes();
    let mut out = Vec::with_capacity(rest.len());
    loop {
        match rest {
            [] => break,
            [b'%', hi_b, lo_b, tail @ ..] => {
                if let (Some(hi), Some(lo)) = (hex_val(*hi_b), hex_val(*lo_b)) {
                    out.push((hi << 4) + lo);
                    rest = tail;
                } else {
                    out.push(b'%');
                    rest = &rest[1..];
                }
            }
            [b, tail @ ..] => {
                out.push(*b);
                rest = tail;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Computes the byte offset of the end of the last complete record in a `libghostty-vt` snapshot.
pub(crate) fn last_snapshot_record_end(snapshot: &[u8]) -> usize {
    const RECORD_HEAD: usize = 10;
    let mut at = 10.min(snapshot.len());
    while let Some(head) = snapshot.get(at..at + RECORD_HEAD) {
        let len = u32::from_le_bytes([head[2], head[3], head[4], head[5]]) as usize;
        match RECORD_HEAD.checked_add(len).and_then(|step| at.checked_add(step)) {
            Some(next) if next <= snapshot.len() => at = next,
            _ => break,
        }
    }
    at
}
