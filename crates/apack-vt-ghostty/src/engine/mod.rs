//! [`GhosttyEngine`] implementing [`apack_core::vt::VtEngine`] on unmodified `ghostty-vt-rs-sys`.

mod callbacks;
mod ffi_queries;
mod render;
mod snapshot;
mod vt_impl;

use std::collections::{BTreeSet, HashMap};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use apack_core::vt::{VtEvent, VtGenerations};
use apack_proto::types::{BlobHash, CellPx, CursorShape, CursorState, GridSize, PaletteOverrides, ViewModes};
use ghostty_vt_rs_sys as sys;

use crate::error::GhosttyError;
use crate::image_store::{ImageStore, install_png_decoder};
use crate::input::InputEncoders;

#[cfg(test)]
pub(crate) use callbacks::{escape_osc633_field, ffi_guard, sanitize_osc633_body};
#[cfg(test)]
pub(crate) use ffi_queries::decode_osc7_cwd;
pub(crate) use ffi_queries::read_grapheme_codepoints;
#[cfg(test)]
pub(crate) use snapshot::{check_decode_loop_outcome, last_snapshot_record_end};
#[cfg(test)]
pub(crate) use vt_impl::{checked_image_placement, coalesce_feed_events, resolve_drawn_pixel_size};

use ffi_queries::{handle_at_ground, track_active_top};

pub(super) const CURSOR_BLINK_MODE: u16 = 12;

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

/// Validated single-command NUL-payload Kitty graphics probe (`G<keys>;AA==\x1b\\\x1b_`).
///
/// Guarantees by construction that feeding this probe while `KITTY_IMAGE_MEDIUM_{FILE,SHARED_MEM}`
/// are temporarily enabled can never open a host path or inject a secondary APC/escape sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedImageProbe(Vec<u8>);

impl NamedImageProbe {
    /// Maximum byte length of a validated named-image probe command.
    pub const MAX_BYTES: usize = 4096;
    const SUFFIX: &'static [u8] = b";AA==\x1b\\\x1b_";

    /// Validates `bytes` as a single NUL-payload Kitty probe command with strictly ASCII key bytes.
    pub fn parse(bytes: Vec<u8>) -> Option<Self> {
        if bytes.len() > Self::MAX_BYTES {
            return None;
        }
        let inner = bytes.strip_prefix(b"G")?.strip_suffix(Self::SUFFIX)?;
        if !inner.iter().all(|&b| b.is_ascii_alphanumeric() || matches!(b, b',' | b'=' | b'-' | b'+')) {
            return None;
        }
        Some(Self(bytes))
    }

    /// Returns the validated probe wire bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
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
    image_cache: ImageSyncCache,
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
            image_cache: ImageSyncCache::default(),
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
