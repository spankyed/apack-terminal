//! Synchronous C callback trampolines registered on `libghostty-vt` handles.

use apack_core::vt::{ProgramStatusUpdate, VtEvent};
use apack_proto::types::{BlockedKind, ProgressState, StatusState};
use ghostty_vt_rs_sys as sys;

use super::CallbackState;
use super::ffi_queries::decode_osc7_cwd;

// --- C callback trampolines ---

/// Prevents any Rust panic from unwinding across an `extern "C"` FFI boundary into `libghostty-vt`.
#[inline]
pub(crate) fn ffi_guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(default)
}

// SAFETY: `libghostty-vt` calls this synchronously during `vt_write`/`resize` with `userdata` set to
// our `CallbackBox` pointer and `data` readable for `len` bytes.
pub(super) unsafe extern "C" fn cb_write_pty(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    data: *const u8,
    len: usize,
) {
    ffi_guard((), || {
        if userdata.is_null() || data.is_null() || len == 0 {
            return;
        }
        // SAFETY: `userdata` is our live `CallbackState`; `data` is readable for `len` bytes.
        let (state, bytes) = unsafe { (&mut *userdata.cast::<CallbackState>(), std::slice::from_raw_parts(data, len)) };
        state.events.push(VtEvent::DeviceResponse(bytes.to_vec()));
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `event`.
pub(super) unsafe extern "C" fn cb_semantic_prompt(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    event: *const sys::TerminalSemanticPrompt,
) {
    ffi_guard((), || {
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
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `out_size`.
pub(super) unsafe extern "C" fn cb_size(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    out_size: *mut sys::SizeReportSize,
) -> bool {
    ffi_guard(false, || {
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
    })
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and live `term`.
pub(super) unsafe extern "C" fn cb_pwd_changed(term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    ffi_guard((), || {
        if userdata.is_null() {
            return;
        }
        let mut val = sys::String::default();
        // SAFETY: `PWD` writes a `GhosttyString` valid during this callback.
        if unsafe { sys::ghostty_terminal_get(term, sys::TerminalData::PWD, (&raw mut val).cast()) }
            != sys::Result::SUCCESS
        {
            return;
        }
        // SAFETY: `userdata` is our live `CallbackState` and `val` was just populated.
        let (state, text) = unsafe { (&mut *userdata.cast::<CallbackState>(), copy_ghostty_string(&val)) };
        if let Some(raw) = text {
            state.events.push(VtEvent::Cwd(decode_osc7_cwd(&raw)));
        }
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and live `term`.
pub(super) unsafe extern "C" fn cb_title_changed(term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    ffi_guard((), || {
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
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer.
pub(super) unsafe extern "C" fn cb_bell(_term: sys::Terminal, userdata: *mut std::ffi::c_void) {
    ffi_guard((), || {
        if userdata.is_null() {
            return;
        }
        // SAFETY: `userdata` is our live `CallbackState`.
        unsafe { &mut *userdata.cast::<CallbackState>() }.events.push(VtEvent::Bell);
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `write`.
pub(super) unsafe extern "C" fn cb_clipboard_write(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    write: *const sys::ClipboardWrite,
) {
    ffi_guard((), || {
        if userdata.is_null() || write.is_null() {
            return;
        }
        let reply_fn = {
            // SAFETY: both pointers are valid for this synchronous callback; `&mut CallbackState`
            // is dropped strictly before invoking `reply_fn` below so re-entrant callbacks cannot alias.
            let (state, req) = unsafe { (&mut *userdata.cast::<CallbackState>(), &*write) };
            if !req.contents.is_null() && req.contents_len > 0 && req.location == sys::ClipboardLocation::STANDARD {
                // SAFETY: `req.contents` is non-null with `req.contents_len > 0` entries for this call.
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
            req.reply
        };
        if let Some(reply) = reply_fn {
            let answer = sys::ClipboardWriteReply {
                size: std::mem::size_of::<sys::ClipboardWriteReply>(),
                result: sys::ClipboardWriteResult::SUCCESS,
                remember: false,
            };
            // SAFETY: invoking the callback-supplied reply function after releasing `&mut CallbackState`.
            unsafe { reply(write, &raw const answer) };
        }
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `note`.
pub(super) unsafe extern "C" fn cb_notification(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    note: *const sys::TerminalDesktopNotification,
) {
    ffi_guard((), || {
        if userdata.is_null() || note.is_null() {
            return;
        }
        // SAFETY: both pointers are valid for this call and strings are copied out.
        let (state, title, body) = unsafe {
            let n = &*note;
            (&mut *userdata.cast::<CallbackState>(), copy_ghostty_string(&n.title), copy_ghostty_string(&n.body))
        };
        state.events.push(VtEvent::Notification { title: title.unwrap_or_default(), body: body.unwrap_or_default() });
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `report`.
pub(super) unsafe extern "C" fn cb_progress(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    report: *const sys::TerminalProgressReport,
) {
    ffi_guard((), || {
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
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `report`.
pub(super) unsafe extern "C" fn cb_program_status(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    report: *const sys::TerminalProgramStatus,
) {
    ffi_guard((), || {
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
        let (app, title, message) = unsafe {
            (copy_ghostty_string(&rep.app), copy_ghostty_string(&rep.title), copy_ghostty_string(&rep.message))
        };
        state.events.push(VtEvent::ProgramStatus(ProgramStatusUpdate::Set {
            id,
            state: status_state,
            kind,
            progress,
            app,
            title,
            message,
        }));
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer.
pub(super) unsafe extern "C" fn cb_render_hold(_term: sys::Terminal, userdata: *mut std::ffi::c_void, held: bool) {
    ffi_guard((), || {
        if userdata.is_null() {
            return;
        }
        // SAFETY: `userdata` is our live `CallbackState`.
        unsafe { &mut *userdata.cast::<CallbackState>() }.events.push(VtEvent::SyncOutput(held));
    });
}

// SAFETY: `libghostty-vt` calls this synchronously with our `CallbackBox` pointer and valid `seq`.
pub(super) unsafe extern "C" fn cb_unknown_sequence(
    _term: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    seq: *const sys::TerminalUnknownSequence,
) {
    ffi_guard((), || {
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
    });
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

pub(super) fn percent_decode(text: &str) -> String {
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
