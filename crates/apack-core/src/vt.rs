//! Pure sans-IO [`VtEngine`] abstraction separating terminal emulation from session orchestration.
//!
//! Production builds back this trait with `apack-vt-ghostty` (`libghostty-vt`), while unit tests
//! and `apack-sim` plug in a deterministic pure-Rust `MockVt` with zero OS or FFI dependencies.

use alloc::string::String;
use alloc::vec::Vec;

use apack_proto::types::{
    BlockedKind, CellPos, CellPx, CellSpan, CursorShape, CursorState, GridSize, ImagePlacement, KeyInput, PackedRow,
    PaletteOverrides, PointerInput, ProgressState, StatusState, ViewModes,
};

/// Monotonic generation counters allowing generation-gated metadata queries (zero FFI when unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VtGenerations {
    /// Incremented whenever any cell or viewport row mutates.
    pub grid: u64,
    /// Incremented whenever OSC 4/10/11/12 palette overrides change.
    pub palette: u64,
    /// Incremented whenever image placements change.
    pub images: u64,
    /// Incremented whenever view-relevant terminal modes change.
    pub modes: u64,
}

/// Structured OSC 7501 program-status update emitted directly by [`VtEngine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramStatusUpdate {
    /// Create or update a program-status record (all fields already decoded by the VT engine).
    Set {
        /// Slot identifier (`""` for root status).
        id: String,
        /// Reported program state.
        state: StatusState,
        /// Optional blocked reason (valid only when `state == StatusState::Blocked`).
        kind: Option<BlockedKind>,
        /// Optional progress percentage (`0..=100`).
        progress: Option<u8>,
        /// Optional claimed application name.
        app: Option<String>,
        /// Optional title (may contain arbitrary characters including `;` and `=`).
        title: Option<String>,
        /// Optional message (may contain arbitrary characters including `;` and `=`).
        message: Option<String>,
    },
    /// Clear a program-status record (`id == ""` clears the root slot).
    Clear {
        /// Slot identifier to clear.
        id: String,
    },
}

/// Side-effect events emitted by [`VtEngine::feed`] or [`VtEngine::resize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VtEvent {
    /// Window title changed (OSC 0/2).
    Title(String),
    /// Audible or visual bell (BEL).
    Bell,
    /// Clipboard write request (OSC 52).
    Clipboard {
        /// Selection target (e.g., `"c"`).
        selection: String,
        /// Decoded UTF-8 clipboard text.
        data: String,
    },
    /// Desktop notification (OSC 9 / OSC 777).
    Notification {
        /// Notification title.
        title: String,
        /// Notification body.
        body: String,
    },
    /// Progress bar update (OSC 9;4).
    Progress {
        /// Progress mode.
        state: ProgressState,
        /// Optional percentage (`0..=100`).
        percent: Option<u8>,
    },
    /// Working directory reported via OSC 7.
    Cwd(String),
    /// DEC mode 2026 synchronized output toggled (`true` = begin hold, `false` = end hold).
    SyncOutput(bool),
    /// Screen epoch changed mid-stream (e.g. alternate-screen switch or hard reset).
    EpochChanged,
    /// Raw OSC 133 payload (after `133;`), with the absolute epoch cursor line at the exact point of arrival.
    Osc133 {
        /// Raw OSC 133 payload after `133;`.
        payload: String,
        /// Epoch-local absolute cursor line when this mark arrived.
        cursor_line: u64,
    },
    /// Raw OSC 633 payload (after `633;`), with the absolute epoch cursor line at the exact point of arrival.
    Osc633 {
        /// Raw OSC 633 payload after `633;`.
        payload: String,
        /// Epoch-local absolute cursor line when this mark arrived.
        cursor_line: u64,
    },
    /// Structured OSC 7501 program-status update (set or clear).
    ProgramStatus(ProgramStatusUpdate),
    /// Terminal reply bytes (e.g. DSR/DA/DECRQM/mode 2048) that must be written back to the PTY stdin.
    DeviceResponse(Vec<u8>),
}

/// Lightweight metadata snapshot describing the VT engine's current screen geometry and state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtSnapshotMeta {
    /// Epoch counter incremented on reflow or alternate-screen transitions.
    pub epoch: u64,
    /// Active viewport grid dimensions.
    pub size: GridSize,
    /// Monotonic epoch-local line number of viewport row `0`.
    ///
    /// Retained history occupies `[first_active_line - scrollback_lines, first_active_line)` and the
    /// active viewport occupies `[first_active_line, first_active_line + size.rows)`.
    pub first_active_line: u64,
    /// Number of retained scrollback lines above the active viewport (`<= first_active_line`).
    pub scrollback_lines: u64,
    /// Current cursor position, shape, blink, and visibility.
    pub cursor: CursorState,
    /// View-relevant terminal modes.
    pub modes: ViewModes,
    /// Current image-placement generation counter.
    pub images_gen: u64,
}

impl VtSnapshotMeta {
    /// Earliest retained absolute epoch line number (`first_active_line - scrollback_lines`).
    pub const fn oldest_line(&self) -> u64 {
        self.first_active_line.saturating_sub(self.scrollback_lines)
    }

    /// Exclusive upper bound on valid absolute epoch line numbers (`first_active_line + rows`).
    pub const fn total_lines(&self) -> u64 {
        self.first_active_line.saturating_add(self.size.rows.get() as u64)
    }
}

/// Viewport dirty summary drained from [`VtEngine::take_dirty_viewport`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DirtyViewport {
    /// Net viewport scroll shift since the previous drain (`> 0` scrolled up, `< 0` down).
    pub scroll_delta: i32,
    /// 0-based viewport row indices (`0..size.rows`) modified since the previous drain.
    pub dirty_rows: Vec<u16>,
    /// `true` when the entire viewport must be re-synced (e.g. epoch changed or scroll exceeded height).
    pub full_dirty: bool,
}

/// Result of encoding a structured paste against the current terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteEncode {
    /// Safe (or explicitly confirmed) byte sequence ready to write to the PTY.
    Bytes(Vec<u8>),
    /// Paste contained a command-executing sequence while `allow_unsafe == false`.
    Unsafe,
}

/// Progress indicator for cooperative incremental scrollback compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressStep {
    /// More uncompressed scrollback pages remain.
    More,
    /// All eligible scrollback pages are compressed.
    Done,
}

/// Pure sans-IO terminal emulator interface owned exclusively by one `SessionMachine`.
pub trait VtEngine {
    /// Feeds raw PTY bytes into the emulator, appending side effects to `events`.
    fn feed(&mut self, bytes: &[u8], events: &mut Vec<VtEvent>);

    /// Resizes the terminal grid and cell pixel geometry, incrementing `epoch` if reflow occurred
    /// and appending any synchronous side effects (e.g. mode 2048 size reports or sync-hold release).
    fn resize(&mut self, size: GridSize, cell_px: CellPx, events: &mut Vec<VtEvent>);

    /// Returns lightweight current screen metadata.
    fn meta(&self) -> VtSnapshotMeta;

    /// Returns monotonic generation counters for grid, palette, images, and modes.
    fn generations(&self) -> VtGenerations;

    /// Returns current dynamic palette overrides (OSC 4/10/11/12).
    fn palette_overrides(&self) -> PaletteOverrides;

    /// Drains viewport dirty tracking since the previous call.
    fn take_dirty_viewport(&mut self) -> DirtyViewport;

    /// Extracts packed rows for the given 0-based viewport row indices into `out`.
    fn extract_viewport_rows(&self, rows: &[u16], out: &mut Vec<PackedRow>);

    /// Extracts up to `count` packed rows starting at absolute epoch line `from_line`.
    fn extract_history_rows(&self, from_line: u64, count: u32, out: &mut Vec<PackedRow>);

    /// Resolves word boundaries at `pos` using `delimiters`.
    fn word_at(&self, pos: CellPos, delimiters: &str) -> Option<CellSpan>;

    /// Resolves wrapped logical line boundaries containing `line`.
    fn line_at(&self, line: u64) -> Option<CellSpan>;

    /// Extracts plain UTF-8 text across an inclusive cell span, stopping at `max_bytes`.
    fn text_of(&self, span: CellSpan, max_bytes: usize) -> String;

    /// Extracts rectangular block UTF-8 text across an inclusive cell span, stopping at `max_bytes`.
    fn text_of_rect(&self, span: CellSpan, max_bytes: usize) -> String;

    /// Searches epoch lines `[from_line, to_line)` for `needle`, appending up to `limit` spans.
    ///
    /// Returns `true` if additional matches existed past `limit`.
    fn search_lines(
        &self,
        from_line: u64,
        to_line: u64,
        needle: &str,
        case_sensitive: bool,
        limit: usize,
        out: &mut Vec<CellSpan>,
    ) -> bool;

    /// Returns current anchored image placements.
    fn images(&self) -> Vec<ImagePlacement>;

    /// Encodes a structured keyboard event into PTY input bytes.
    fn encode_key(&mut self, key: &KeyInput, out: &mut Vec<u8>);

    /// Encodes a structured pointer event into PTY input bytes using per-view `held_buttons` bitmask.
    fn encode_pointer(&mut self, ev: &PointerInput, held_buttons: &mut u8, out: &mut Vec<u8>);

    /// Encodes focus gained/lost into PTY bytes if focus reporting is enabled.
    fn encode_focus(&mut self, focused: bool, out: &mut Vec<u8>);

    /// Checks and encodes a structured paste (honouring bracketed paste mode and unsafe-paste safety).
    fn encode_paste(&mut self, text: &str, allow_unsafe: bool) -> PasteEncode;

    /// Sets fallback cursor style when the running program has not selected one.
    fn set_cursor_defaults(&mut self, shape: CursorShape, blinking: bool);

    /// Compresses up to `max_pages` idle scrollback pages cooperatively.
    fn compress_step(&mut self, max_pages: usize) -> CompressStep;

    /// Reports current estimated scrollback memory footprint in bytes.
    fn scrollback_bytes(&self) -> u64;
}
