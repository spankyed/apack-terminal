//! Binary snapshot export/restore and `libghostty-vt` handle configuration.

use apack_proto::MAX_STRING_BYTES;
use apack_proto::types::GridSize;
use ghostty_vt_rs_sys as sys;

use crate::error::GhosttyError;
use crate::input::query_mode;
use crate::{SESSION_IMAGE_STORAGE_BYTES, UNKNOWN_OSC_MAX_BYTES};

use super::callbacks::{
    cb_bell, cb_clipboard_write, cb_notification, cb_program_status, cb_progress, cb_pwd_changed, cb_render_hold,
    cb_semantic_prompt, cb_size, cb_title_changed, cb_unknown_sequence, cb_write_pty, ffi_guard,
};
use super::ffi_queries::{handle_at_ground, query_alt_screen, query_scrollback_rows, read_u16, track_active_top};
use super::vt_impl::apply_cursor_defaults;
use super::{CURSOR_BLINK_MODE, CallbackState, GhosttyEngine};

const CONTINUATION_MAX_BYTES: usize = 4096;

impl GhosttyEngine {
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
            ffi_guard(false, || {
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
            })
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
        // Validate grid geometry immediately after header decode BEFORE decoding history/image records.
        let Some(new_size) = read_u16(new_handle, sys::TerminalData::COLS)
            .zip(read_u16(new_handle, sys::TerminalData::ROWS))
            .and_then(|(c, r)| GridSize::new(c, r))
        else {
            // SAFETY: freeing both decoder and handle on invalid grid dimensions.
            unsafe {
                sys::ghostty_snapshot_decoder_free(decoder);
                sys::ghostty_terminal_free(new_handle);
            }
            return Err(GhosttyError { call: "restore_snapshot(grid_size)", code: sys::Result::INVALID_VALUE });
        };

        // SAFETY: decode all remaining history/image records while both `decoder` and `new_handle` are live.
        let mut finished = false;
        let mut decode_err: Option<GhosttyError> = None;
        for _ in 0..=snapshot.len() {
            let next_res = unsafe { sys::ghostty_snapshot_decoder_next(decoder) };
            if next_res == sys::Result::NO_VALUE {
                finished = true;
                break;
            }
            if next_res != sys::Result::SUCCESS {
                decode_err = Some(GhosttyError { call: "ghostty_snapshot_decoder_next", code: next_res });
                break;
            }
        }
        // SAFETY: decoder is no longer needed once all records are decoded or failed.
        unsafe { sys::ghostty_snapshot_decoder_free(decoder) };
        if let Err(err) = check_decode_loop_outcome(finished, decode_err) {
            // SAFETY: freeing decoded handle on corrupt/incomplete record failure without touching `self`.
            unsafe { sys::ghostty_terminal_free(new_handle) };
            return Err(err);
        }

        // Allocate `new_mark` BEFORE `configure_handle`/`resize` so any failure leaves `self` and
        // `self.callbacks` completely untouched.
        let new_mark = match track_active_top(new_handle) {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: freeing decoded handle if mark creation fails.
                unsafe { sys::ghostty_terminal_free(new_handle) };
                return Err(e);
            }
        };

        // Configure `new_handle` BEFORE swapping so any error leaves `self` completely intact!
        if let Err(e) = self.configure_handle(new_handle, true) {
            // SAFETY: freeing mark strictly before its owning decoded handle on settings failure.
            unsafe {
                sys::ghostty_tracked_grid_ref_free(new_mark);
                sys::ghostty_terminal_free(new_handle);
            }
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
        self.image_cache.images_gen = 0;
        self.image_cache.blobs.clear();
        self.image_cache.virtual_named.clear();
        self.image_cache.virtual_lowest.clear();
        self.gens.images = self.gens.images.saturating_add(1);
        self.sync_images_cache();
        self.raw_images_gen = self.image_cache.images_gen;
        self.refresh_render_state(true, 0);
        Ok(())
    }

    pub(super) fn configure_handle(&self, handle: sys::Terminal, preserve_blink: bool) -> Result<(), GhosttyError> {
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
}

/// Validates that the snapshot decode loop exited cleanly with `NO_VALUE` and zero errors.
pub(crate) fn check_decode_loop_outcome(finished: bool, decode_err: Option<GhosttyError>) -> Result<(), GhosttyError> {
    if let Some(err) = decode_err {
        return Err(err);
    }
    if !finished {
        return Err(GhosttyError { call: "ghostty_snapshot_decoder_next", code: sys::Result::INVALID_VALUE });
    }
    Ok(())
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
