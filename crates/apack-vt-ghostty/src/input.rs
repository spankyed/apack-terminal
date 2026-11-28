//! Broker-side VT keyboard, pointer, focus, and paste encoders backed by `libghostty-vt`.

use apack_core::vt::PasteEncode;
use apack_proto::types::{GridSize, KeyAction, KeyInput, PointerAction, PointerInput};
use ghostty_vt_rs_sys as sys;

use crate::error::GhosttyError;

const FOCUS_EVENTS_MODE: u16 = 1004;
const BRACKETED_PASTE_MODE: u16 = 2004;
const PASTE_END: &[u8] = b"\x1b[201~";
const MOUSE_MODES: [u16; 8] = [9, 1000, 1002, 1003, 1005, 1006, 1015, 1016];
const MAX_CELL_PIXELS: u32 = 1024;

/// Owned `libghostty-vt` key & mouse encoders for a single session.
pub struct InputEncoders {
    key: sys::KeyEncoder,
    key_event: sys::KeyEvent,
    mouse: sys::MouseEncoder,
    mouse_event: sys::MouseEvent,
    mouse_modes: Option<[bool; MOUSE_MODES.len()]>,
    mouse_size: Option<[u32; 4]>,
}

// SAFETY: handles are owned exclusively by one session and never shared across threads.
unsafe impl Send for InputEncoders {}

impl InputEncoders {
    /// Allocates fresh key & mouse encoder handles from `libghostty-vt`.
    pub fn new() -> Result<Self, GhosttyError> {
        let mut enc = Self {
            key: std::ptr::null_mut(),
            key_event: std::ptr::null_mut(),
            mouse: std::ptr::null_mut(),
            mouse_event: std::ptr::null_mut(),
            mouse_modes: None,
            mouse_size: None,
        };
        // SAFETY: each out-pointer is valid; any partial failure leaves remaining handles null
        // which `Drop` frees safely because `ghostty_*_free` accepts null pointers.
        unsafe {
            GhosttyError::check(
                "ghostty_key_encoder_new",
                sys::ghostty_key_encoder_new(std::ptr::null(), &raw mut enc.key),
            )?;
            GhosttyError::check(
                "ghostty_key_event_new",
                sys::ghostty_key_event_new(std::ptr::null(), &raw mut enc.key_event),
            )?;
            GhosttyError::check(
                "ghostty_mouse_encoder_new",
                sys::ghostty_mouse_encoder_new(std::ptr::null(), &raw mut enc.mouse),
            )?;
            GhosttyError::check(
                "ghostty_mouse_event_new",
                sys::ghostty_mouse_event_new(std::ptr::null(), &raw mut enc.mouse_event),
            )?;
        }
        Ok(enc)
    }

    /// Encodes a structured keyboard input against `terminal`'s active modes.
    pub(crate) fn encode_key(&mut self, terminal: sys::Terminal, input: &KeyInput, out: &mut Vec<u8>) {
        let action = match input.action {
            KeyAction::Press => sys::KeyAction::PRESS,
            KeyAction::Repeat => sys::KeyAction::REPEAT,
            KeyAction::Release => sys::KeyAction::RELEASE,
        };
        let text = if action == sys::KeyAction::RELEASE { "" } else { typed(&input.text) };
        let mods = u16::from(input.modifiers);
        let consumed_mods = consumed(input);
        let option = if input.composed { sys::OptionAsAlt::FALSE } else { sys::OptionAsAlt::TRUE };

        // SAFETY: all handles are live for `&mut self` and `terminal` is live for this call.
        unsafe {
            if sys::ghostty_key_encoder_set_from_terminal(self.key, terminal) != sys::Result::SUCCESS {
                return;
            }
            let _ = sys::ghostty_key_encoder_set(
                self.key,
                sys::KeyEncoderOption::MACOS_OPTION_AS_ALT,
                (&raw const option).cast(),
            );
            let _ = sys::ghostty_key_event_set_action(self.key_event, action);
            let _ = sys::ghostty_key_event_set_key(self.key_event, key_named(&input.code));
            let _ = sys::ghostty_key_event_set_mods(self.key_event, mods);
            let _ = sys::ghostty_key_event_set_consumed_mods(self.key_event, consumed_mods);
            let _ = sys::ghostty_key_event_set_composing(self.key_event, false);
            let _ = sys::ghostty_key_event_set_unshifted_codepoint(self.key_event, input.unshifted_codepoint);
        }
        let (encoder, event) = (self.key, self.key_event);
        // SAFETY: `text` stays live across the encode call and is cleared immediately afterwards.
        let set_utf8 = unsafe { sys::ghostty_key_event_set_utf8(event, sys::String::from(text)) };
        if set_utf8 == sys::Result::SUCCESS
            && let Ok(bytes) = sized("ghostty_key_encoder_encode", |buf, len, written| {
                // SAFETY: `buf` is valid for `len` bytes and `written` is a valid out-pointer.
                unsafe { sys::ghostty_key_encoder_encode(encoder, event, buf, len, written) }
            })
        {
            out.extend_from_slice(&bytes);
        }
        // SAFETY: clear borrowed `text` pointer so `key_event` never holds a dangling slice.
        unsafe {
            sys::ghostty_key_event_set_utf8(self.key_event, sys::String::from(""));
        }
    }

    /// Clears cached terminal mouse-mode & geometry snapshots (called after snapshot restore).
    pub(crate) fn reset_cache(&mut self) {
        self.mouse_modes = None;
        self.mouse_size = None;
    }

    /// Encodes a structured pointer/wheel input against `terminal`'s active mouse modes using per-view `held_buttons`.
    pub(crate) fn encode_pointer(
        &mut self,
        terminal: sys::Terminal,
        grid: GridSize,
        ev: &PointerInput,
        held_buttons: &mut u8,
        out: &mut Vec<u8>,
    ) {
        let cell_w = u32::from(ev.cell_px.width).clamp(1, MAX_CELL_PIXELS);
        let cell_h = u32::from(ev.cell_px.height).clamp(1, MAX_CELL_PIXELS);
        let screen_w = u32::from(grid.cols.get()).saturating_mul(cell_w);
        let screen_h = u32::from(grid.rows.get()).saturating_mul(cell_h);
        let position = sys::MousePosition {
            x: (ev.x_px.min(screen_w.saturating_add(cell_w))) as f32,
            y: (ev.y_px.min(screen_h.saturating_add(cell_h))) as f32,
        };
        let (action, button) = match ev.action {
            PointerAction::Press => (sys::MouseAction::PRESS, button_named(ev.button)),
            PointerAction::Release => (sys::MouseAction::RELEASE, button_named(ev.button)),
            PointerAction::Motion => (sys::MouseAction::MOTION, button_named(ev.button)),
            PointerAction::WheelUp => (sys::MouseAction::PRESS, Some(sys::MouseButton::FOUR)),
            PointerAction::WheelDown => (sys::MouseAction::PRESS, Some(sys::MouseButton::FIVE)),
            PointerAction::WheelLeft => (sys::MouseAction::PRESS, Some(sys::MouseButton::SIX)),
            PointerAction::WheelRight => (sys::MouseAction::PRESS, Some(sys::MouseButton::SEVEN)),
        };
        if ev.button < 3 {
            match ev.action {
                PointerAction::Press => *held_buttons |= 1 << ev.button,
                PointerAction::Release => *held_buttons &= !(1 << ev.button),
                _ => {}
            }
        }
        let pressed = *held_buttons != 0;
        let mut modes = [false; MOUSE_MODES.len()];
        for (slot, mode) in modes.iter_mut().zip(MOUSE_MODES) {
            *slot = query_mode(terminal, mode);
        }
        if self.mouse_modes != Some(modes) {
            let track = true;
            // SAFETY: handles are live; `TRACK_LAST_CELL` expects a `bool*`.
            unsafe {
                if sys::ghostty_mouse_encoder_set_from_terminal(self.mouse, terminal) != sys::Result::SUCCESS {
                    return;
                }
                let _ = sys::ghostty_mouse_encoder_set(
                    self.mouse,
                    sys::MouseEncoderOption::TRACK_LAST_CELL,
                    (&raw const track).cast(),
                );
            }
            self.mouse_modes = Some(modes);
        }
        let measured = [screen_w, screen_h, cell_w, cell_h];
        if self.mouse_size != Some(measured) {
            let size = sys::MouseEncoderSize {
                size: std::mem::size_of::<sys::MouseEncoderSize>(),
                screen_width: screen_w,
                screen_height: screen_h,
                cell_width: cell_w,
                cell_height: cell_h,
                ..Default::default()
            };
            // SAFETY: `SIZE` takes a sized `GhosttyMouseEncoderSize*`.
            if unsafe {
                sys::ghostty_mouse_encoder_set(self.mouse, sys::MouseEncoderOption::SIZE, (&raw const size).cast())
            } != sys::Result::SUCCESS
            {
                return;
            }
            self.mouse_size = Some(measured);
        }

        // SAFETY: all handles are live and each option matches its documented C type.
        unsafe {
            match button {
                Some(b) => {
                    let _ = sys::ghostty_mouse_event_set_button(self.mouse_event, b);
                }
                None => {
                    let _ = sys::ghostty_mouse_event_clear_button(self.mouse_event);
                }
            }
            let _ = sys::ghostty_mouse_encoder_set(
                self.mouse,
                sys::MouseEncoderOption::ANY_BUTTON_PRESSED,
                (&raw const pressed).cast(),
            );
            let _ = sys::ghostty_mouse_event_set_action(self.mouse_event, action);
            let _ = sys::ghostty_mouse_event_set_mods(self.mouse_event, u16::from(ev.modifiers));
            let _ = sys::ghostty_mouse_event_set_position(self.mouse_event, position);
        }
        let (encoder, event) = (self.mouse, self.mouse_event);
        if let Ok(bytes) = sized("ghostty_mouse_encoder_encode", |buf, len, written| {
            // SAFETY: `buf` is valid for `len` bytes and `written` is a valid out-pointer.
            unsafe { sys::ghostty_mouse_encoder_encode(encoder, event, buf, len, written) }
        }) {
            out.extend_from_slice(&bytes);
        }
    }
}

impl Drop for InputEncoders {
    fn drop(&mut self) {
        // SAFETY: each handle is freed once; `ghostty_*_free` accepts null pointers.
        unsafe {
            sys::ghostty_key_event_free(self.key_event);
            sys::ghostty_key_encoder_free(self.key);
            sys::ghostty_mouse_event_free(self.mouse_event);
            sys::ghostty_mouse_encoder_free(self.mouse);
        }
    }
}

/// Encodes focus gained/lost if DEC mode 1004 is enabled on `terminal`.
pub(crate) fn encode_focus(terminal: sys::Terminal, focused: bool, out: &mut Vec<u8>) {
    if !query_mode(terminal, FOCUS_EVENTS_MODE) {
        return;
    }
    let event = if focused { sys::FocusEvent::GAINED } else { sys::FocusEvent::LOST };
    if let Ok(bytes) = sized("ghostty_focus_encode", |buf, len, written| {
        // SAFETY: `buf` is valid for `len` bytes and `written` is a valid out-pointer.
        unsafe { sys::ghostty_focus_encode(event, buf, len, written) }
    }) {
        out.extend_from_slice(&bytes);
    }
}

/// Checks unsafe paste sequences and encodes `text` honouring DEC mode 2004 (bracketed paste).
pub(crate) fn encode_paste(terminal: sys::Terminal, text: &str, allow_unsafe: bool) -> PasteEncode {
    let bracketed = query_mode(terminal, BRACKETED_PASTE_MODE);
    let bytes = text.as_bytes();
    const PASTE_END_C1: &[u8] = b"\xc2\x9b201~";
    let dangerous = if bracketed {
        bytes.windows(PASTE_END.len()).any(|w| w == PASTE_END)
            || bytes.windows(PASTE_END_C1.len()).any(|w| w == PASTE_END_C1)
    } else {
        bytes.iter().any(|&b| b == b'\n' || b == b'\r')
    };
    if dangerous && !allow_unsafe {
        return PasteEncode::Unsafe;
    }
    let mut scratch = bytes.to_vec();
    let ptr = scratch.as_mut_ptr().cast::<std::ffi::c_char>();
    let len = scratch.len();
    match sized("ghostty_paste_encode", |buf, room, written| {
        // SAFETY: `ptr` points to `len` mutable bytes in `scratch` which outlives both attempts;
        // `buf` is valid for `room` bytes.
        unsafe { sys::ghostty_paste_encode(ptr, len, bracketed, buf, room, written) }
    }) {
        Ok(encoded) => PasteEncode::Bytes(encoded),
        Err(_) => PasteEncode::Bytes(Vec::new()),
    }
}

pub(crate) fn query_mode(terminal: sys::Terminal, mode: u16) -> bool {
    let mut cfg = sys::TerminalModeConfig { mode, value: false };
    // SAFETY: `MODE` reads `cfg.mode` and writes `cfg.value` on a live terminal handle.
    let res = unsafe { sys::ghostty_terminal_get(terminal, sys::TerminalData::MODE, (&raw mut cfg).cast()) };
    res == sys::Result::SUCCESS && cfg.value
}

fn sized(
    call: &'static str,
    mut encode: impl FnMut(*mut u8, usize, *mut usize) -> sys::Result::Type,
) -> Result<Vec<u8>, GhosttyError> {
    let mut buf = vec![0u8; 128];
    let mut written: usize = 0;
    let mut res = encode(buf.as_mut_ptr(), buf.len(), &raw mut written);
    if res == sys::Result::OUT_OF_SPACE {
        buf.resize(written, 0);
        res = encode(buf.as_mut_ptr(), buf.len(), &raw mut written);
    }
    GhosttyError::check(call, res)?;
    buf.truncate(written);
    Ok(buf)
}

pub(crate) fn consumed(key: &KeyInput) -> u16 {
    let mut chars = key.text.chars();
    let (Some(first), None) = (chars.next(), chars.next()) else { return 0 };
    if key.unshifted_codepoint == 0 || u32::from(first) == key.unshifted_codepoint || first.is_control() {
        return 0;
    }
    let mods = u16::from(key.modifiers);
    if mods & sys::MODS_CTRL != 0 {
        return 0;
    }
    let composing = if key.composed { sys::MODS_SHIFT + sys::MODS_ALT } else { sys::MODS_SHIFT };
    mods & composing
}

fn typed(text: &str) -> &str {
    let is_control_or_mac_fn = |ch: char| matches!(ch, '\0'..='\u{1f}' | '\u{7f}' | '\u{f700}'..='\u{f8ff}');
    if text.chars().any(is_control_or_mac_fn) { "" } else { text }
}

fn button_named(button: u8) -> Option<sys::MouseButton::Type> {
    match button {
        0 => Some(sys::MouseButton::LEFT),
        1 => Some(sys::MouseButton::MIDDLE),
        2 => Some(sys::MouseButton::RIGHT),
        _ => None,
    }
}

fn key_named(code: &str) -> sys::Key::Type {
    KEYS.iter().find(|(name, _)| *name == code).map_or(sys::Key::UNIDENTIFIED, |(_, k)| *k)
}

const KEYS: &[(&str, sys::Key::Type)] = &[
    ("Backquote", sys::Key::BACKQUOTE),
    ("Backslash", sys::Key::BACKSLASH),
    ("BracketLeft", sys::Key::BRACKET_LEFT),
    ("BracketRight", sys::Key::BRACKET_RIGHT),
    ("Comma", sys::Key::COMMA),
    ("Digit0", sys::Key::DIGIT_0),
    ("Digit1", sys::Key::DIGIT_1),
    ("Digit2", sys::Key::DIGIT_2),
    ("Digit3", sys::Key::DIGIT_3),
    ("Digit4", sys::Key::DIGIT_4),
    ("Digit5", sys::Key::DIGIT_5),
    ("Digit6", sys::Key::DIGIT_6),
    ("Digit7", sys::Key::DIGIT_7),
    ("Digit8", sys::Key::DIGIT_8),
    ("Digit9", sys::Key::DIGIT_9),
    ("Equal", sys::Key::EQUAL),
    ("IntlBackslash", sys::Key::INTL_BACKSLASH),
    ("IntlRo", sys::Key::INTL_RO),
    ("IntlYen", sys::Key::INTL_YEN),
    ("KeyA", sys::Key::A),
    ("KeyB", sys::Key::B),
    ("KeyC", sys::Key::C),
    ("KeyD", sys::Key::D),
    ("KeyE", sys::Key::E),
    ("KeyF", sys::Key::F),
    ("KeyG", sys::Key::G),
    ("KeyH", sys::Key::H),
    ("KeyI", sys::Key::I),
    ("KeyJ", sys::Key::J),
    ("KeyK", sys::Key::K),
    ("KeyL", sys::Key::L),
    ("KeyM", sys::Key::M),
    ("KeyN", sys::Key::N),
    ("KeyO", sys::Key::O),
    ("KeyP", sys::Key::P),
    ("KeyQ", sys::Key::Q),
    ("KeyR", sys::Key::R),
    ("KeyS", sys::Key::S),
    ("KeyT", sys::Key::T),
    ("KeyU", sys::Key::U),
    ("KeyV", sys::Key::V),
    ("KeyW", sys::Key::W),
    ("KeyX", sys::Key::X),
    ("KeyY", sys::Key::Y),
    ("KeyZ", sys::Key::Z),
    ("Minus", sys::Key::MINUS),
    ("Period", sys::Key::PERIOD),
    ("Quote", sys::Key::QUOTE),
    ("Semicolon", sys::Key::SEMICOLON),
    ("Slash", sys::Key::SLASH),
    ("AltLeft", sys::Key::ALT_LEFT),
    ("AltRight", sys::Key::ALT_RIGHT),
    ("Backspace", sys::Key::BACKSPACE),
    ("CapsLock", sys::Key::CAPS_LOCK),
    ("ContextMenu", sys::Key::CONTEXT_MENU),
    ("ControlLeft", sys::Key::CONTROL_LEFT),
    ("ControlRight", sys::Key::CONTROL_RIGHT),
    ("Enter", sys::Key::ENTER),
    ("MetaLeft", sys::Key::META_LEFT),
    ("MetaRight", sys::Key::META_RIGHT),
    ("ShiftLeft", sys::Key::SHIFT_LEFT),
    ("ShiftRight", sys::Key::SHIFT_RIGHT),
    ("Space", sys::Key::SPACE),
    ("Tab", sys::Key::TAB),
    ("Convert", sys::Key::CONVERT),
    ("KanaMode", sys::Key::KANA_MODE),
    ("NonConvert", sys::Key::NON_CONVERT),
    ("Delete", sys::Key::DELETE),
    ("End", sys::Key::END),
    ("Help", sys::Key::HELP),
    ("Home", sys::Key::HOME),
    ("Insert", sys::Key::INSERT),
    ("PageDown", sys::Key::PAGE_DOWN),
    ("PageUp", sys::Key::PAGE_UP),
    ("ArrowDown", sys::Key::ARROW_DOWN),
    ("ArrowLeft", sys::Key::ARROW_LEFT),
    ("ArrowRight", sys::Key::ARROW_RIGHT),
    ("ArrowUp", sys::Key::ARROW_UP),
    ("NumLock", sys::Key::NUM_LOCK),
    ("Numpad0", sys::Key::NUMPAD_0),
    ("Numpad1", sys::Key::NUMPAD_1),
    ("Numpad2", sys::Key::NUMPAD_2),
    ("Numpad3", sys::Key::NUMPAD_3),
    ("Numpad4", sys::Key::NUMPAD_4),
    ("Numpad5", sys::Key::NUMPAD_5),
    ("Numpad6", sys::Key::NUMPAD_6),
    ("Numpad7", sys::Key::NUMPAD_7),
    ("Numpad8", sys::Key::NUMPAD_8),
    ("Numpad9", sys::Key::NUMPAD_9),
    ("NumpadAdd", sys::Key::NUMPAD_ADD),
    ("NumpadBackspace", sys::Key::NUMPAD_BACKSPACE),
    ("NumpadClear", sys::Key::NUMPAD_CLEAR),
    ("NumpadClearEntry", sys::Key::NUMPAD_CLEAR_ENTRY),
    ("NumpadComma", sys::Key::NUMPAD_COMMA),
    ("NumpadDecimal", sys::Key::NUMPAD_DECIMAL),
    ("NumpadDivide", sys::Key::NUMPAD_DIVIDE),
    ("NumpadEnter", sys::Key::NUMPAD_ENTER),
    ("NumpadEqual", sys::Key::NUMPAD_EQUAL),
    ("NumpadMemoryAdd", sys::Key::NUMPAD_MEMORY_ADD),
    ("NumpadMemoryClear", sys::Key::NUMPAD_MEMORY_CLEAR),
    ("NumpadMemoryRecall", sys::Key::NUMPAD_MEMORY_RECALL),
    ("NumpadMemoryStore", sys::Key::NUMPAD_MEMORY_STORE),
    ("NumpadMemorySubtract", sys::Key::NUMPAD_MEMORY_SUBTRACT),
    ("NumpadMultiply", sys::Key::NUMPAD_MULTIPLY),
    ("NumpadParenLeft", sys::Key::NUMPAD_PAREN_LEFT),
    ("NumpadParenRight", sys::Key::NUMPAD_PAREN_RIGHT),
    ("NumpadSubtract", sys::Key::NUMPAD_SUBTRACT),
    ("NumpadSeparator", sys::Key::NUMPAD_SEPARATOR),
    ("NumpadUp", sys::Key::NUMPAD_UP),
    ("NumpadDown", sys::Key::NUMPAD_DOWN),
    ("NumpadRight", sys::Key::NUMPAD_RIGHT),
    ("NumpadLeft", sys::Key::NUMPAD_LEFT),
    ("NumpadBegin", sys::Key::NUMPAD_BEGIN),
    ("NumpadHome", sys::Key::NUMPAD_HOME),
    ("NumpadEnd", sys::Key::NUMPAD_END),
    ("NumpadInsert", sys::Key::NUMPAD_INSERT),
    ("NumpadDelete", sys::Key::NUMPAD_DELETE),
    ("NumpadPageUp", sys::Key::NUMPAD_PAGE_UP),
    ("NumpadPageDown", sys::Key::NUMPAD_PAGE_DOWN),
    ("Escape", sys::Key::ESCAPE),
    ("F1", sys::Key::F1),
    ("F2", sys::Key::F2),
    ("F3", sys::Key::F3),
    ("F4", sys::Key::F4),
    ("F5", sys::Key::F5),
    ("F6", sys::Key::F6),
    ("F7", sys::Key::F7),
    ("F8", sys::Key::F8),
    ("F9", sys::Key::F9),
    ("F10", sys::Key::F10),
    ("F11", sys::Key::F11),
    ("F12", sys::Key::F12),
    ("F13", sys::Key::F13),
    ("F14", sys::Key::F14),
    ("F15", sys::Key::F15),
    ("F16", sys::Key::F16),
    ("F17", sys::Key::F17),
    ("F18", sys::Key::F18),
    ("F19", sys::Key::F19),
    ("F20", sys::Key::F20),
    ("F21", sys::Key::F21),
    ("F22", sys::Key::F22),
    ("F23", sys::Key::F23),
    ("F24", sys::Key::F24),
    ("F25", sys::Key::F25),
    ("Fn", sys::Key::FN),
    ("FnLock", sys::Key::FN_LOCK),
    ("PrintScreen", sys::Key::PRINT_SCREEN),
    ("ScrollLock", sys::Key::SCROLL_LOCK),
    ("Pause", sys::Key::PAUSE),
    ("BrowserBack", sys::Key::BROWSER_BACK),
    ("BrowserFavorites", sys::Key::BROWSER_FAVORITES),
    ("BrowserForward", sys::Key::BROWSER_FORWARD),
    ("BrowserHome", sys::Key::BROWSER_HOME),
    ("BrowserRefresh", sys::Key::BROWSER_REFRESH),
    ("BrowserSearch", sys::Key::BROWSER_SEARCH),
    ("BrowserStop", sys::Key::BROWSER_STOP),
    ("Eject", sys::Key::EJECT),
    ("LaunchApp1", sys::Key::LAUNCH_APP_1),
    ("LaunchApp2", sys::Key::LAUNCH_APP_2),
    ("LaunchMail", sys::Key::LAUNCH_MAIL),
    ("MediaPlayPause", sys::Key::MEDIA_PLAY_PAUSE),
    ("MediaSelect", sys::Key::MEDIA_SELECT),
    ("MediaStop", sys::Key::MEDIA_STOP),
    ("MediaTrackNext", sys::Key::MEDIA_TRACK_NEXT),
    ("MediaTrackPrevious", sys::Key::MEDIA_TRACK_PREVIOUS),
    ("Power", sys::Key::POWER),
    ("Sleep", sys::Key::SLEEP),
    ("AudioVolumeDown", sys::Key::AUDIO_VOLUME_DOWN),
    ("AudioVolumeMute", sys::Key::AUDIO_VOLUME_MUTE),
    ("AudioVolumeUp", sys::Key::AUDIO_VOLUME_UP),
    ("WakeUp", sys::Key::WAKE_UP),
    ("Copy", sys::Key::COPY),
    ("Cut", sys::Key::CUT),
    ("Paste", sys::Key::PASTE),
];
