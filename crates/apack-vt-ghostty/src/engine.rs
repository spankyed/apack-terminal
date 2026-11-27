//! [`GhosttyEngine`] implementing [`apack_core::vt::VtEngine`] on unmodified `ghostty-vt-rs-sys`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use apack_core::vt::{
    CompressStep, DirtyViewport, PasteEncode, ProgramStatusUpdate, VtEngine, VtEvent, VtGenerations, VtSnapshotMeta,
};
use apack_proto::MAX_STRING_BYTES;
use apack_proto::types::{
    BlobHash, BlockedKind, CellPos, CellPx, CellSpan, CursorShape, CursorState, GridSize, ImageEncoding,
    ImagePlacement, KeyInput, PackedRow, PaletteOverrides, PointerInput, ProgressState, Rgb, StatusState, ViewModes,
};
use ghostty_vt_rs_sys as sys;

use crate::error::GhosttyError;
use crate::image_store::{ImageStore, encode_png_pixels, install_png_decoder};
use crate::input::{InputEncoders, encode_focus, encode_paste, query_mode};
use crate::placeholders::PLACEHOLDER;
use crate::rows::{ROW_LINK_BYTES, VirtualImageInfo, extract_row, pack_color, pack_style_flags};
use crate::{SESSION_IMAGE_STORAGE_BYTES, UNKNOWN_OSC_MAX_BYTES};

const REVERSE_VIDEO_MODE: u16 = 5;
const CURSOR_BLINK_MODE: u16 = 12;
const BRACKETED_PASTE_MODE: u16 = 2004;
/// Hard cap on `ghostty_search_get(MATCHES)` allocation (~3 MiB) to prevent OOM on dense single-page bursts.
const MAX_SEARCH_ALLOC: usize = 65_536;

const SEARCH_CANDIDATES: usize = 20_000;
const CONTINUATION_MAX_BYTES: usize = 4096;
const CLUSTER_CODEPOINTS: usize = 16;

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

/// Heap-allocated callback target pinned behind a raw [`NonNull`] pointer (never `Box<T>` with
/// `noalias`) so C FFI `USERDATA` pointers remain valid under Stacked/Tree Borrows across `&mut self`.
struct CallbackState {
    events: Vec<VtEvent>,
    cols: u16,
    rows: u16,
    cell_px: CellPx,
    first_active_line: u64,
}

struct CallbackBox(NonNull<CallbackState>);

// SAFETY: `CallbackBox` exclusively owns its heap allocation and is never shared across threads.
unsafe impl Send for CallbackBox {}

impl CallbackBox {
    fn new(state: CallbackState) -> Self {
        Self(NonNull::from(Box::leak(Box::new(state))))
    }

    fn as_ptr(&self) -> *mut CallbackState {
        self.0.as_ptr()
    }

    /// Borrows `CallbackState` mutably for a short scope strictly outside any C FFI call.
    fn with_mut<R>(&mut self, f: impl FnOnce(&mut CallbackState) -> R) -> R {
        // SAFETY: `&mut self` guarantees exclusive Rust access and no C callback is executing.
        f(unsafe { self.0.as_mut() })
    }
}

impl Drop for CallbackBox {
    fn drop(&mut self) {
        // SAFETY: `GhosttyEngine::drop` frees `self.handle` BEFORE dropping fields, so no C handle
        // references `self.0` when `CallbackBox::drop` reclaims the allocation.
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

#[derive(Default)]
struct ImageSyncCache {
    images_gen: u64,
    /// Maps active `(kitty_image_id, kitty_image_gen)` -> `(BlobHash, width, height)`.
    blobs: HashMap<(u32, u64), (BlobHash, u32, u32)>,
    /// Virtual placement grids keyed by `(image_id, placement_id)`.
    virtual_named: HashMap<(u32, u32), (u32, u32)>,
    /// Lowest-numbered virtual placement grid per `image_id`.
    virtual_lowest: HashMap<u32, (u32, (u32, u32))>,
}

/// `libghostty-vt` terminal emulator implementing [`VtEngine`].
///
/// Field safety: `Drop` explicitly frees `active_top`, `render_rows`, `render_state`, and
/// `handle` strictly BEFORE Rust drops the `callbacks` [`CallbackBox`] field.
pub struct GhosttyEngine {
    handle: sys::Terminal,
    render_state: sys::RenderState,
    render_rows: sys::RenderStateRowIterator,
    active_top: sys::TrackedGridRef,
    encoders: InputEncoders,
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
    /// Last raw `libghostty-vt` Kitty graphics generation observed on `self.handle` (used to drive
    /// monotonic `self.gens.images` even across `restore_snapshot` handle swaps).
    raw_images_gen: u64,
    dirty_rows: BTreeSet<u16>,
    scroll_delta: i32,
    full_dirty: bool,
    viewport_hashes: Vec<u64>,
    cursor_defaults: Option<(CursorShape, bool)>,
    callbacks: CallbackBox,
}

// SAFETY: `GhosttyEngine` exclusively owns every underlying C handle and its `CallbackBox`;
// none of them are shared across threads, so moving ownership between threads is sound.
unsafe impl Send for GhosttyEngine {}

impl GhosttyEngine {
    /// Creates a new `GhosttyEngine` with hardened defaults (zero file/temp-file/SHM image reads).
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

        let callbacks = CallbackBox::new(CallbackState {
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
            raw_images_gen: 0,
            dirty_rows: BTreeSet::new(),
            scroll_delta: 0,
            full_dirty: true,
            viewport_hashes: vec![0u64; usize::from(config.size.rows.get())],
            cursor_defaults: None,
            callbacks,
        };
        engine.configure_handle(engine.handle, false)?;
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

    /// Returns `true` when the VT parser and UTF-8 decoder are cleanly at ground state (no pending continuation).
    pub fn at_ground(&self) -> bool {
        handle_at_ground(self.handle)
    }

    /// Encodes a bounded binary `libghostty-vt` snapshot (at most `max_bytes`) for v2 keyframes.
    ///
    /// Fails with `OUT_OF_SPACE` if the snapshot exceeds `max_bytes` (never returns a truncated,
    /// unrestorable snapshot missing its `FINISH` record).
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
        if cap.cut {
            return Err(GhosttyError { call: "export_snapshot", code: sys::Result::OUT_OF_SPACE });
        }
        GhosttyError::check("ghostty_snapshot_encode", res)?;
        Ok(cap.out)
    }

    /// Atomically restores terminal state from a binary snapshot produced by [`Self::export_snapshot`].
    ///
    /// All-or-nothing: validates geometry, decodes all records, re-applies hardened broker settings
    /// (preserving restored DEC mode 12 cursor blink state), and swaps handles only after every step
    /// succeeds, leaving `self` completely intact on any error.
    pub fn restore_snapshot(&mut self, snapshot: &[u8]) -> Result<(), GhosttyError> {
        let mut decoder: sys::SnapshotDecoder = std::ptr::null_mut();
        // SAFETY: `decoder` is a valid out-pointer; `snapshot` outlives the decoder below.
        GhosttyError::check("ghostty_snapshot_decoder_new_buf", unsafe {
            sys::ghostty_snapshot_decoder_new_buf(std::ptr::null(), &raw mut decoder, snapshot.as_ptr(), snapshot.len())
        })?;
        let image_limit: u64 = if self.allow_images { SESSION_IMAGE_STORAGE_BYTES } else { 0 };
        let continuation_cap: usize = CONTINUATION_MAX_BYTES;
        let retain_continuation = true;
        let compress_history = true;
        let opts: [(sys::SnapshotDecoderOption::Type, *const core::ffi::c_void); 4] = [
            (sys::SnapshotDecoderOption::KITTY_IMAGE_STORAGE_LIMIT, (&raw const image_limit).cast()),
            (sys::SnapshotDecoderOption::MAX_CONTINUATION_BYTES, (&raw const continuation_cap).cast()),
            (sys::SnapshotDecoderOption::RETAIN_CONTINUATION, (&raw const retain_continuation).cast()),
            (sys::SnapshotDecoderOption::COMPRESS_HISTORY, (&raw const compress_history).cast()),
        ];
        // SAFETY: option pointer types match `libghostty-vt` `SnapshotDecoderOption` contracts.
        let mut set_res = sys::Result::SUCCESS;
        for (opt, ptr) in opts {
            // SAFETY: `decoder` is live and `ptr` points to the exact option type.
            set_res = unsafe { sys::ghostty_snapshot_decoder_set(decoder, opt, ptr) };
            if set_res != sys::Result::SUCCESS {
                break;
            }
        }
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
        if new_handle.is_null() {
            // SAFETY: freeing decoder on null handle error.
            unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
            return Err(GhosttyError { call: "ghostty_snapshot_decoder_ready", code: sys::Result::OUT_OF_MEMORY });
        }
        // SAFETY: decode all remaining history/image records while both `decoder` and `new_handle` are live.
        let mut decode_err: Option<GhosttyError> = None;
        for _ in 0..=snapshot.len() {
            let next_res = unsafe { sys::ghostty_snapshot_decoder_next(decoder) };
            if next_res == sys::Result::NO_VALUE {
                break;
            }
            if next_res != sys::Result::SUCCESS {
                decode_err = Some(GhosttyError { call: "ghostty_snapshot_decoder_next", code: next_res });
                break;
            }
        }
        // SAFETY: decoder is no longer needed once all records are decoded or failed.
        unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
        if let Some(err) = decode_err {
            // SAFETY: freeing decoded handle on corrupt-record failure without touching `self`.
            unsafe { sys::ghostty_terminal_free(new_handle) };
            return Err(err);
        }

        let Some(new_size) = read_u16(new_handle, sys::TerminalData::COLS)
            .zip(read_u16(new_handle, sys::TerminalData::ROWS))
            .and_then(|(c, r)| GridSize::new(c, r))
        else {
            // SAFETY: freeing decoded handle on invalid grid dimensions.
            unsafe { sys::ghostty_terminal_free(new_handle) };
            return Err(GhosttyError { call: "restore_snapshot(grid_size)", code: sys::Result::INVALID_VALUE });
        };

        // Configure `new_handle` BEFORE swapping so any error leaves `self` completely intact!
        if let Err(e) = self.configure_handle(new_handle, true) {
            // SAFETY: freeing decoded handle on settings failure.
            unsafe { sys::ghostty_terminal_free(new_handle) };
            return Err(e);
        }
        // SAFETY: `new_handle` is live; restores pixel cell geometry without changing grid cols/rows.
        let _ = unsafe {
            sys::ghostty_terminal_resize(
                new_handle,
                new_size.cols.get(),
                new_size.rows.get(),
                u32::from(self.cell_px.width),
                u32::from(self.cell_px.height),
            )
        };

        let new_mark = match track_active_top(new_handle) {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: freeing decoded handle if mark creation fails.
                unsafe { sys::ghostty_terminal_free(new_handle) };
                return Err(e);
            }
        };

        // Commit swap: free old active_top strictly BEFORE freeing old terminal handle.
        let old_mark = std::mem::replace(&mut self.active_top, new_mark);
        let old_handle = std::mem::replace(&mut self.handle, new_handle);
        // SAFETY: freeing old mark before its owning terminal handle.
        unsafe {
            sys::ghostty_tracked_grid_ref_free(old_mark);
            sys::ghostty_terminal_free(old_handle);
        }

        self.size = new_size;
        self.alt_screen = query_alt_screen(self.handle);
        self.scrollback_lines = query_scrollback_rows(self.handle);
        self.first_active_line = if self.alt_screen { 0 } else { self.scrollback_lines };
        let first_active = self.first_active_line;
        self.callbacks.with_mut(|cb| {
            cb.cols = new_size.cols.get();
            cb.rows = new_size.rows.get();
            cb.first_active_line = first_active;
            cb.events.clear();
        });
        self.epoch += 1;
        self.full_dirty = true;
        self.dirty_rows.clear();
        self.scroll_delta = 0;
        self.encoders.reset_cache();
        {
            let mut cache = self.image_cache.borrow_mut();
            cache.images_gen = 0;
            cache.blobs.clear();
            cache.virtual_named.clear();
            cache.virtual_lowest.clear();
        }
        self.raw_images_gen = query_images_generation(self.handle);
        self.gens.images = self.gens.images.saturating_add(1);
        self.refresh_render_state(true, 0);
        Ok(())
    }

    fn configure_handle(&self, handle: sys::Terminal, preserve_blink: bool) -> Result<(), GhosttyError> {
        let cb_ptr: *mut CallbackState = self.callbacks.as_ptr();
        // SAFETY: `USERDATA` stores `cb_ptr` from `CallbackBox` (never `Box<T>` `noalias`), valid
        // until after `handle` is freed in `Drop`.
        GhosttyError::check("ghostty_terminal_set(USERDATA)", unsafe {
            sys::ghostty_terminal_set(handle, sys::TerminalOption::USERDATA, cb_ptr.cast())
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
            GhosttyError::check(name, unsafe { sys::ghostty_terminal_set(handle, opt, func) })?;
        }

        let sb_lines = self.scrollback_lines_limit;
        let sb_bytes = usize::MAX;
        let unknown_cap = UNKNOWN_OSC_MAX_BYTES;
        let clipboard_cap = MAX_STRING_BYTES;
        let continuation_cap = CONTINUATION_MAX_BYTES;
        let image_limit: u64 = if self.allow_images { SESSION_IMAGE_STORAGE_BYTES } else { 0 };
        let no_files = false;
        let term_name = sys::String::from(self.terminfo_name.as_str());

        // SAFETY: each option pointer matches the documented C type (`size_t*`, `uint64_t*`, `bool*`, `NULL`, `GhosttyString*`).
        unsafe {
            GhosttyError::check(
                "SCROLLBACK_MAX_LINES",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::SCROLLBACK_MAX_LINES,
                    (&raw const sb_lines).cast(),
                ),
            )?;
            GhosttyError::check(
                "SCROLLBACK_MAX_BYTES",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::SCROLLBACK_MAX_BYTES,
                    (&raw const sb_bytes).cast(),
                ),
            )?;
            GhosttyError::check(
                "UNKNOWN_MAX_BYTES",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::UNKNOWN_MAX_BYTES,
                    (&raw const unknown_cap).cast(),
                ),
            )?;
            GhosttyError::check(
                "CLIPBOARD_WRITE_MAX_BYTES",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::CLIPBOARD_WRITE_MAX_BYTES,
                    (&raw const clipboard_cap).cast(),
                ),
            )?;
            GhosttyError::check(
                "CONTINUATION_MAX_BYTES",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::CONTINUATION_MAX_BYTES,
                    (&raw const continuation_cap).cast(),
                ),
            )?;
            GhosttyError::check(
                "KITTY_IMAGE_STORAGE_LIMIT",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT,
                    (&raw const image_limit).cast(),
                ),
            )?;
            // Hostile-PTY security invariant: VT core NEVER opens files, temp files, or shared memory!
            GhosttyError::check(
                "KITTY_IMAGE_MEDIUM_FILE",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::KITTY_IMAGE_MEDIUM_FILE,
                    (&raw const no_files).cast(),
                ),
            )?;
            GhosttyError::check(
                "KITTY_IMAGE_MEDIUM_TEMP_FILE",
                sys::ghostty_terminal_set(handle, sys::TerminalOption::KITTY_IMAGE_MEDIUM_TEMP_FILE, std::ptr::null()),
            )?;
            GhosttyError::check(
                "KITTY_IMAGE_MEDIUM_SHARED_MEM",
                sys::ghostty_terminal_set(
                    handle,
                    sys::TerminalOption::KITTY_IMAGE_MEDIUM_SHARED_MEM,
                    (&raw const no_files).cast(),
                ),
            )?;
            GhosttyError::check(
                "TERMINFO_NAME",
                sys::ghostty_terminal_set(handle, sys::TerminalOption::TERMINFO_NAME, (&raw const term_name).cast()),
            )?;
        }

        if let Some((shape, blinking)) = self.cursor_defaults {
            if preserve_blink {
                let prev_blink = query_mode(handle, CURSOR_BLINK_MODE);
                if !handle_at_ground(handle) {
                    // Never inject raw escape bytes into a mid-sequence continuation; preserve the
                    // restored blink state via cursor defaults directly when not at ground.
                    apply_cursor_defaults(handle, shape, prev_blink);
                } else {
                    apply_cursor_defaults(handle, shape, blinking);
                    // Restore DEC mode 12 cleanly now that we know `handle` is at ground.
                    let seq: &[u8] = if prev_blink { b"\x1b[?12h" } else { b"\x1b[?12l" };
                    // SAFETY: `handle` is live and at ground; `seq` is a valid static slice.
                    unsafe { sys::ghostty_terminal_vt_write(handle, seq.as_ptr(), seq.len()) };
                }
            } else {
                apply_cursor_defaults(handle, shape, blinking);
            }
        }
        Ok(())
    }

    fn update_scroll_and_screen(&mut self, events: &mut Vec<VtEvent>) -> u64 {
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

    fn format_span_raw(&self, span: CellSpan) -> String {
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

    fn sync_images_cache(&self) {
        let cur_gen = query_images_generation(self.handle);
        let mut cache = self.image_cache.borrow_mut();
        let gen_changed = cache.images_gen != cur_gen;
        if !gen_changed && cache.blobs.is_empty() {
            return;
        }
        // Fast path when `cur_gen` unchanged: touch every cached blob in `ImageStore` under one brief
        // lock; if all are still resident, nothing to re-encode!
        if !gen_changed {
            let all_resident = {
                let mut store = self.image_store.lock().expect("ImageStore mutex poisoned");
                cache.blobs.values().all(|&(hash, _, _)| store.touch(hash))
            };
            if all_resident {
                return;
            }
        }

        cache.images_gen = cur_gen;
        cache.virtual_named.clear();
        cache.virtual_lowest.clear();
        let Some(graphics) = query_graphics(self.handle) else {
            cache.blobs.clear();
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

        let mut active_keys: HashSet<(u32, u64)> = HashSet::new();
        let mut to_encode: Vec<(u32, u64, u32, u32, u8, Vec<u8>)> = Vec::new();
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
                // SAFETY: `graphics` is live for `&self`.
                let img = unsafe { sys::ghostty_kitty_graphics_image(graphics, image_id) };
                if !img.is_null() {
                    let img_gen = query_image_generation(img);
                    let key = (image_id, img_gen);
                    if active_keys.insert(key) {
                        let still_resident = cache.blobs.get(&key).is_some_and(|&(hash, _, _)| store.touch(hash));
                        if !still_resident && let Some((w, h, ch, px)) = copy_kitty_pixels(img) {
                            to_encode.push((image_id, img_gen, w, h, ch, px));
                        }
                    }
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
            drop(store);
        }
        // SAFETY: freeing iterator once.
        unsafe { sys::ghostty_kitty_graphics_placement_iterator_free(iter) };

        // Evict deleted/replaced image metadata so `cache.blobs` never grows unboundedly.
        cache.blobs.retain(|k, _| active_keys.contains(k));

        // Encode PNGs completely OUTSIDE the global `ImageStore` mutex so slow compression
        // never stalls other sessions/shards!
        if !to_encode.is_empty() {
            let encoded: Vec<(u32, u64, u32, u32, Vec<u8>)> = to_encode
                .into_iter()
                .filter_map(|(id, g, w, h, ch, px)| encode_png_pixels(w, h, ch, &px).map(|png| (id, g, w, h, png)))
                .collect();
            let mut store = self.image_store.lock().expect("ImageStore mutex poisoned");
            for (id, g, w, h, png) in encoded {
                if let Some(hash) = store.insert_encoded(ImageEncoding::Png, png) {
                    cache.blobs.insert((id, g), (hash, w, h));
                }
            }
        }
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
        // SAFETY: free every dependent C handle strictly BEFORE freeing `self.handle`, which in
        // turn runs BEFORE Rust drops `self.callbacks` (`CallbackBox`).
        unsafe {
            sys::ghostty_tracked_grid_ref_free(self.active_top);
            sys::ghostty_render_state_row_iterator_free(self.render_rows);
            sys::ghostty_render_state_free(self.render_state);
            sys::ghostty_terminal_free(self.handle);
        }
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

    /// Feeds a NUL-payload Kitty graphics probe command (`Load::asked()` or `Load::refusal()`)
    /// with file and shared-memory mediums temporarily enabled so `libghostty-vt` validates all
    /// command keys and returns its native reply without ever opening a real path, unconditionally
    /// disabling both mediums again before returning.
    pub fn feed_named_probe(&mut self, probe: &[u8], events: &mut Vec<VtEvent>) {
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
        let _ = self.feed_inner(probe, false, events);
    }

    /// Feeds `bytes` up to the first APC/SOS/PM string introducer (`TerminalBoundary::APC_START`),
    /// returning `(bytes_consumed, entered_apc)`.
    ///
    /// When `entered_apc` is `true`, `libghostty-vt` has consumed the introducer and sits at the
    /// start of the string body so an external scanner can inspect named Kitty graphics commands.
    pub fn feed_until_apc(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>) -> (usize, bool) {
        self.feed_inner(bytes, true, events)
    }

    fn feed_inner(&mut self, bytes: &[u8], stop_on_apc: bool, events: &mut Vec<VtEvent>) -> (usize, bool) {
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
}

fn apply_cursor_defaults(handle: sys::Terminal, shape: CursorShape, blinking: bool) {
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

// --- C callback trampolines ---

// SAFETY: `libghostty-vt` calls this synchronously during `vt_write`/`resize` with `userdata` set to
// our `CallbackBox` pointer and `data` readable for `len` bytes.
unsafe extern "C" fn cb_write_pty(_term: sys::Terminal, userdata: *mut std::ffi::c_void, data: *const u8, len: usize) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState`; `data` is readable for `len` bytes.
    let (state, bytes) = unsafe { (&mut *userdata.cast::<CallbackState>(), std::slice::from_raw_parts(data, len)) };
    state.events.push(VtEvent::DeviceResponse(bytes.to_vec()));
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `event`.
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
                let escaped = escape_osc633_field(&cmd);
                state.events.push(VtEvent::Osc133 { payload: format!("E;{escaped}"), cursor_line: line });
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `out_size`.
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and live `term`.
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and live `term`.
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer.
unsafe extern "C" fn cb_bell(_term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState`.
    unsafe { &mut *userdata.cast::<CallbackState>() }.events.push(VtEvent::Bell);
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `write`.
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `note`.
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `report`.
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

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `report`.
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
    // SAFETY: string slices are valid for this callback and copied out immediately.
    let id = unsafe { copy_ghostty_string(&rep.id) }.unwrap_or_default();
    if rep.state == sys::ProgramStatusState::CLEAR {
        state.events.push(VtEvent::ProgramStatus(ProgramStatusUpdate::Clear { id }));
        return;
    }
    let status_state = match rep.state {
        sys::ProgramStatusState::IDLE => StatusState::Idle,
        sys::ProgramStatusState::WORKING => StatusState::Working,
        sys::ProgramStatusState::DONE => StatusState::Done,
        sys::ProgramStatusState::BLOCKED => StatusState::Blocked,
        sys::ProgramStatusState::ERROR => StatusState::Error,
        _ => return,
    };
    let kind = match rep.kind {
        sys::ProgramStatusKind::PERMISSION => Some(BlockedKind::Permission),
        sys::ProgramStatusKind::QUESTION => Some(BlockedKind::Question),
        sys::ProgramStatusKind::AUTH => Some(BlockedKind::Auth),
        _ => None,
    };
    let progress = u8::try_from(rep.progress).ok();
    // SAFETY: string slices are valid for this callback and copied out immediately.
    let (app, title, message) =
        unsafe { (copy_ghostty_string(&rep.app), copy_ghostty_string(&rep.title), copy_ghostty_string(&rep.message)) };
    state.events.push(VtEvent::ProgramStatus(ProgramStatusUpdate::Set {
        id,
        state: status_state,
        kind,
        progress,
        app,
        title,
        message,
    }));
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer.
unsafe extern "C" fn cb_render_hold(_term: sys::Terminal, userdata: *mut std::ffi::c_void, held: bool) {
    if userdata.is_null() {
        return;
    }
    // SAFETY: `userdata` is our live `CallbackState`.
    unsafe { &mut *userdata.cast::<CallbackState>() }.events.push(VtEvent::SyncOutput(held));
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `seq`.
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
        let Some(payload) = sanitize_osc633_body(body, osc.truncated) else {
            return;
        };
        state.events.push(VtEvent::Osc633 { payload, cursor_line: state.first_active_line });
    }
}

/// Drops truncated unvouched claims, and strips the half-truncated CWD field from truncated `V;F;`
/// records while preserving their authenticated exit code.
pub(crate) fn sanitize_osc633_body(body: &str, truncated: bool) -> Option<String> {
    if !truncated {
        return Some(body.to_owned());
    }
    if !body.starts_with("V;") {
        return None;
    }
    if body.starts_with("V;F;") {
        // Format: V;F;<seq>;<key>;<status>;<cwd> -> keep up to and including the 5th `;` (empty cwd).
        let mut semis = body.match_indices(';');
        if let Some((fifth_pos, _)) = semis.nth(4) {
            return Some(body[..=fifth_pos].to_owned());
        }
    }
    Some(body.to_owned())
}

/// Escapes `\` / `;` / `\n` / `\r` in an already-decoded command string so `MarksTracker`'s
/// `OSC 133;E;<cmd>` parser recovers the exact original command verbatim.
pub(crate) fn escape_osc633_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ';' => out.push_str("\\x3b"),
            '\n' => out.push_str("\\x0a"),
            '\r' => out.push_str("\\x0d"),
            other => out.push(other),
        }
    }
    out
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

/// Hashes all visually observable state of viewport row `row` (wrap/prompt flags, cells, AND
/// side-table styles, grapheme continuations, and hyperlink URIs guarded by fast row-level flags).
fn hash_viewport_row(handle: sys::Terminal, row: u32, cols: u16) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
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
    let mut row_styled = false;
    let mut row_grapheme = false;
    let mut row_hyperlink = false;
    // SAFETY: `grid_ref` was just populated; each `RowData` key matches its documented out-pointer type.
    unsafe {
        if sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw_row) == sys::Result::SUCCESS {
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::WRAP, (&raw mut wrapped).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::SEMANTIC_PROMPT, (&raw mut prompt).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::STYLED, (&raw mut row_styled).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::GRAPHEME, (&raw mut row_grapheme).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::HYPERLINK, (&raw mut row_hyperlink).cast());
        }
    }
    let mut h: u64 =
        0xcbf2_9ce4_8422_2325u64.wrapping_add(u64::from(wrapped)).wrapping_mul(PRIME).wrapping_add(u64::from(prompt));
    let mut link_scratch: Vec<u8> = Vec::new();
    for col in 0..cols {
        grid_ref.x = col;
        let mut cell: sys::Cell = 0;
        // SAFETY: `grid_ref` names a valid column in the resolved row.
        if unsafe { sys::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut cell) } != sys::Result::SUCCESS {
            continue;
        }
        h = h.wrapping_mul(PRIME).wrapping_add(cell);

        if row_styled {
            let mut styled = false;
            // SAFETY: `HAS_STYLING` writes a `bool`.
            let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::HAS_STYLING, (&raw mut styled).cast()) };
            if styled {
                let mut st = sys::Style::default();
                // SAFETY: `grid_ref` is live and `st` is a valid out-pointer.
                let _ = unsafe { sys::ghostty_grid_ref_style(&raw const grid_ref, &raw mut st) };
                h = h
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_color(st.fg_color)))
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_color(st.bg_color)))
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_color(st.underline_color)))
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_style_flags(st.flags)));
            }
        }
        if row_grapheme {
            let mut content: sys::CellContentTag::Type = sys::CellContentTag::CODEPOINT;
            // SAFETY: `CONTENT_TAG` writes a `GhosttyCellContentTag`.
            let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::CONTENT_TAG, (&raw mut content).cast()) };
            if content == sys::CellContentTag::CODEPOINT_GRAPHEME {
                let mut cluster = Vec::new();
                if read_grapheme_codepoints(&grid_ref, &mut cluster) {
                    for &cp in &cluster {
                        h = h.wrapping_mul(PRIME).wrapping_add(u64::from(cp));
                    }
                }
            }
        }
        if row_hyperlink {
            let mut linked = false;
            // SAFETY: `HAS_HYPERLINK` writes a `bool`.
            let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::HAS_HYPERLINK, (&raw mut linked).cast()) };
            if linked {
                let mut short = [0u8; 256];
                let mut written = 0usize;
                // SAFETY: `short` is valid for `short.len()` bytes.
                let res = unsafe {
                    sys::ghostty_grid_ref_hyperlink_uri(
                        &raw const grid_ref,
                        short.as_mut_ptr(),
                        short.len(),
                        &raw mut written,
                    )
                };
                let slice: &[u8] = match res {
                    sys::Result::SUCCESS => &short[..written.min(short.len())],
                    sys::Result::OUT_OF_SPACE => {
                        link_scratch.resize(written.min(ROW_LINK_BYTES), 0);
                        // SAFETY: `link_scratch` has `link_scratch.len()` bytes.
                        let r2 = unsafe {
                            sys::ghostty_grid_ref_hyperlink_uri(
                                &raw const grid_ref,
                                link_scratch.as_mut_ptr(),
                                link_scratch.len(),
                                &raw mut written,
                            )
                        };
                        if r2 == sys::Result::SUCCESS { &link_scratch[..written.min(link_scratch.len())] } else { &[] }
                    }
                    _ => &[],
                };
                for &b in slice {
                    h = h.wrapping_mul(PRIME).wrapping_add(u64::from(b));
                }
            }
        }
    }
    h
}

fn viewport_row_has_placeholder(handle: sys::Terminal, row: u32) -> bool {
    let point = sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: row } },
    };
    let mut grid_ref = sys::GridRef::default();
    // SAFETY: `grid_ref` is a valid out-pointer on live `handle`.
    if unsafe { sys::ghostty_terminal_grid_ref(handle, point, &raw mut grid_ref) } != sys::Result::SUCCESS {
        return false;
    }
    let mut raw_row: sys::Row = 0;
    let mut has = false;
    // SAFETY: `KITTY_VIRTUAL_PLACEHOLDER` writes a `bool`.
    unsafe {
        if sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw_row) == sys::Result::SUCCESS {
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::KITTY_VIRTUAL_PLACEHOLDER, (&raw mut has).cast());
        }
    }
    has
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

fn copy_kitty_pixels(img: sys::KittyGraphicsImage) -> Option<(u32, u32, u8, Vec<u8>)> {
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
        return None;
    }
    let channels = match format {
        sys::KittyImageFormat::RGB => 3,
        sys::KittyImageFormat::RGBA => 4,
        _ => return None,
    };
    // SAFETY: `pixels.ptr` is non-null and readable for `pixels.len` bytes while terminal is immutable.
    let slice = unsafe { std::slice::from_raw_parts(pixels.ptr, pixels.len) };
    Some((width, height, channels, slice.to_vec()))
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

/// Decodes OSC 7 `file://host/path` percent-escapes, returning bare paths (OSC 9 / OSC 1337) verbatim.
pub(crate) fn decode_osc7_cwd(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("file://")
        && let Some(slash) = rest.find('/')
    {
        percent_decode(&rest[slash..])
    } else {
        raw.to_owned()
    }
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

/// Maximum grapheme cluster codepoints read on retry when a cluster exceeds `CLUSTER_CODEPOINTS`.
const MAX_CLUSTER_CODEPOINTS: usize = 256;

/// Returns `true` when `handle`'s VT parser and UTF-8 decoder have no pending continuation bytes.
fn handle_at_ground(handle: sys::Terminal) -> bool {
    let mut written = 0usize;
    // SAFETY: passing null buffer with length 0 queries continuation byte length cleanly.
    if unsafe { sys::ghostty_terminal_continuation_buf(handle, std::ptr::null_mut(), 0, &raw mut written) }
        == sys::Result::SUCCESS
    {
        written == 0
    } else {
        false
    }
}

/// Reads grapheme cluster codepoints from `grid_ref` into `out`, retrying on `OUT_OF_SPACE` up to
/// [`MAX_CLUSTER_CODEPOINTS`] so clusters with `> 16` codepoints are never silently dropped.
pub(crate) fn read_grapheme_codepoints(grid_ref: &sys::GridRef, out: &mut Vec<u32>) -> bool {
    out.clear();
    let mut stack = [0u32; CLUSTER_CODEPOINTS];
    let mut written = 0usize;
    // SAFETY: `stack` has length `CLUSTER_CODEPOINTS` matching the passed capacity.
    let res = unsafe { sys::ghostty_grid_ref_graphemes(grid_ref, stack.as_mut_ptr(), stack.len(), &raw mut written) };
    if res == sys::Result::SUCCESS {
        out.extend_from_slice(&stack[..written.min(stack.len())]);
        return true;
    }
    if res == sys::Result::OUT_OF_SPACE {
        let cap = written.clamp(stack.len(), MAX_CLUSTER_CODEPOINTS);
        let mut heap = vec![0u32; cap];
        let mut retry_written = 0usize;
        // SAFETY: `heap` has length `cap` matching the passed capacity.
        let retry =
            unsafe { sys::ghostty_grid_ref_graphemes(grid_ref, heap.as_mut_ptr(), heap.len(), &raw mut retry_written) };
        if retry == sys::Result::SUCCESS {
            out.extend_from_slice(&heap[..retry_written.min(heap.len())]);
            return true;
        }
    }
    false
}

impl GhosttyEngine {
    pub(crate) fn text_of_batched(&self, span: CellSpan, max_bytes: usize, batch_lines: u64) -> String {
        let step = batch_lines.max(1);
        let max_col = self.size.cols.get().saturating_sub(1);
        let mut out = String::new();
        let mut cur_line = span.start.line;
        while cur_line <= span.end.line && out.len() < max_bytes {
            let mut batch_end = cur_line.saturating_add(step.saturating_sub(1)).min(span.end.line);
            if let Some(logical) = self.line_at(batch_end) {
                batch_end = logical.end.line.min(span.end.line);
            }
            let sub = CellSpan {
                start: CellPos { line: cur_line, col: if cur_line == span.start.line { span.start.col } else { 0 } },
                end: CellPos { line: batch_end, col: if batch_end == span.end.line { span.end.col } else { max_col } },
            };
            if cur_line > span.start.line {
                out.push('\n');
            }
            let chunk = self.format_span_raw(sub);
            let remaining = max_bytes.saturating_sub(out.len());
            let take = chunk.floor_char_boundary(remaining);
            out.push_str(&chunk[..take]);
            if take != chunk.len() {
                break;
            }
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

/// Computes the byte offset of the end of the last complete record in a `libghostty-vt` snapshot.
#[cfg(test)]
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
