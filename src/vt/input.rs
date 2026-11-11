//! What a view's input becomes on its way to the program.
//!
//! **A key is not bytes until the terminal's modes are known.** An arrow is `ESC [ A` or `ESC O A`
//! depending on a mode the program set; with the Kitty keyboard protocol on it is something else again, and
//! a release is reported at all only there. A window that encodes keys itself has to mirror every such mode
//! and be right about each — so a view sends what the keyboard said and the encoding happens here, against
//! the one terminal that knows.
//!
//! The same holds for the pointer (which reports exist, in which of five formats), for a paste (whether
//! it is bracketed, which decides whether a line ending in it is dangerous) and for focus (whether the
//! program asked to be told).

use ghostty_vt_rs_sys as sys;

use super::{Failed, Terminal};

/// A program asked to be told when its terminal gains and loses focus.
const FOCUS_EVENTS: u16 = 1004;
/// A paste is wrapped in markers, so the program knows it was not typed.
const BRACKETED_PASTE: u16 = 2004;
/// What ends a bracketed paste: text holding it would close the bracket early.
const PASTE_END: &[u8] = b"\x1b[201~";

/// A key as a keyboard reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key<'a> {
    /// `0` press, `1` repeat, `2` release
    pub action: u8,
    /// The physical key, as `KeyboardEvent.code` names it
    pub code: &'a str,
    /// Shift 1, control 2, alt 4, super 8, caps lock 16, num lock 32: the core's own bits
    pub modifiers: u16,
    /// What the key types with the modifiers applied
    pub text: &'a str,
    /// What it types with none, as a code point, or `0`
    pub unshifted: u32,
    /// Alt went into producing `text` rather than modifying the key: the option key on a Mac, where it
    /// types characters unless the person chose it as alt. Only the view knows — it is a fact about the
    /// keyboard in front of the person, which need not be on the machine the broker runs on.
    pub composed: bool,
}

/// The pointer, for a program that asked for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pointer {
    /// `0` press, `1` release, `2` move, `3` wheel up, `4` wheel down
    pub action: u8,
    /// `0` left, `1` middle, `2` right; anything else is no button
    pub button: u8,
    pub modifiers: u16,
    /// Pixels from the grid's top-left corner, which may be outside the grid: a drag that left the window
    pub x: f32,
    pub y: f32,
    pub cell_width: f32,
    pub cell_height: f32,
}

/// What became of a paste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pasted {
    /// These bytes go to the program
    Encoded(Vec<u8>),
    /// The text could run a command and was not allowed to
    Unsafe,
}

/// Every key `KeyboardEvent.code` names that the core has a name for.
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

fn key_named(code: &str) -> sys::Key::Type {
    KEYS.iter().find(|(name, _)| *name == code).map_or(sys::Key::UNIDENTIFIED, |(_, key)| *key)
}

/// The modifiers that went into producing a key's text rather than modifying it.
///
/// Shift is spent when it changed what was typed — `A` from `a`, `!` from `1`. **Alt is spent only where
/// the view says it composed the character** (`Key::composed`): on Linux and Windows alt never types
/// anything, so `Alt+Shift+A` is the key `A` with alt held and is sent as `ESC A`. Reported as spent, the
/// encoder sends the text; reported as held, it sends the key with the modifier, which is a different key.
///
/// **What a view sends for a character typed through AltGr** — which a browser reports as control and alt
/// held — is the character as `text` with those two modifiers left out: they were how it was typed, and
/// with control reported nothing here could tell it from a control chord.
fn consumed(key: &Key<'_>) -> u16 {
    let mut typed = key.text.chars();
    let (Some(first), None) = (typed.next(), typed.next()) else { return 0 };
    if key.unshifted == 0 || u32::from(first) == key.unshifted || first.is_control() {
        return 0;
    }
    // Control never composes a character: with it down the text is whatever the layout made of the rest
    if key.modifiers & sys::MODS_CTRL != 0 {
        return 0;
    }
    let composing = if key.composed { sys::MODS_SHIFT | sys::MODS_ALT } else { sys::MODS_SHIFT };
    key.modifiers & composing
}

/// A key's text as the encoder may be given it: nothing for a control character.
///
/// The core asks for the character a key types and derives a control chord from the key and its
/// modifiers; handed the control character itself — what a browser reports for `Ctrl+C`, or for Enter —
/// it would send that and the chord's encoding both.
fn typed(text: &str) -> &str {
    // The control characters, and the private-use code points a Mac reports its function keys as
    let names_a_key = |character: char| matches!(character, '\0'..='\u{1f}' | '\u{7f}' | '\u{f700}'..='\u{f8ff}');
    if text.chars().any(names_a_key) { "" } else { text }
}

/// The buttons the core has a name for, as the bits a view's held buttons are kept in.
const NAMED_BUTTONS: u8 = 3;

/// The widest and tallest a cell is taken to be, in pixels. A view measures one in the tens.
const MAX_CELL_PIXELS: f32 = 1024.0;

/// The core's encoders, kept for a session: the mouse one remembers the last cell it reported.
pub struct Encoders {
    key: sys::KeyEncoder,
    key_event: sys::KeyEvent,
    mouse: sys::MouseEncoder,
    mouse_event: sys::MouseEvent,
    /// The mouse modes the encoder was last set from. Setting it again forgets the cell it last
    /// reported, so it is set only when one of these has changed
    mouse_modes: Option<[bool; MOUSE_MODES.len()]>,
    /// The grid in pixels the encoder was last given, for the same reason
    mouse_size: Option<[u32; 4]>,
}

/// Every mode that decides whether, and how, the pointer is reported: the four kinds of tracking and the
/// four formats a report can take.
const MOUSE_MODES: [u16; 8] = [9, 1000, 1002, 1003, 1005, 1006, 1015, 1016];

// As `Terminal`: allocated and freed by the library, and held by one session under its lock
unsafe impl Send for Encoders {}

impl Encoders {
    pub fn new() -> Result<Self, Failed> {
        let mut encoders = Self {
            key: std::ptr::null_mut(),
            key_event: std::ptr::null_mut(),
            mouse: std::ptr::null_mut(),
            mouse_event: std::ptr::null_mut(),
            mouse_modes: None,
            mouse_size: None,
        };
        // SAFETY: each is a valid out-pointer, and a null allocator asks for the library's own. A failure
        // part way leaves the rest null, which `Drop` passes to frees that take one
        unsafe {
            Failed::check(
                "ghostty_key_encoder_new",
                sys::ghostty_key_encoder_new(std::ptr::null(), &raw mut encoders.key),
            )?;
            Failed::check(
                "ghostty_key_event_new",
                sys::ghostty_key_event_new(std::ptr::null(), &raw mut encoders.key_event),
            )?;
            Failed::check(
                "ghostty_mouse_encoder_new",
                sys::ghostty_mouse_encoder_new(std::ptr::null(), &raw mut encoders.mouse),
            )?;
            Failed::check(
                "ghostty_mouse_event_new",
                sys::ghostty_mouse_event_new(std::ptr::null(), &raw mut encoders.mouse_event),
            )?;
        }
        Ok(encoders)
    }

    /// What a key sends the program now, which for many keys in many modes is nothing.
    pub fn key(&mut self, terminal: &Terminal, key: &Key<'_>) -> Result<Vec<u8>, Failed> {
        let action = match key.action {
            1 => sys::KeyAction::REPEAT,
            2 => sys::KeyAction::RELEASE,
            _ => sys::KeyAction::PRESS,
        };
        // A release types nothing, whatever the keyboard said the key was
        let text = if action == sys::KeyAction::RELEASE { "" } else { typed(key.text) };
        // SAFETY: every handle is live; `text` outlives the encode below, after which the event's borrow
        // of it is taken back
        unsafe {
            Failed::check(
                "ghostty_key_encoder_set_from_terminal",
                sys::ghostty_key_encoder_set_from_terminal(self.key, terminal.handle),
            )?;
            // Set from the terminal, this is reset every time: it is not something a terminal knows.
            // **Alt is alt unless the view said it composed the character.** The core built for a Mac
            // otherwise assumes the option key types characters and never sends alt at all — which is a
            // rule about the machine the broker runs on, where the keyboard is the view's
            let option = if key.composed { sys::OptionAsAlt::FALSE } else { sys::OptionAsAlt::TRUE };
            Failed::check(
                "ghostty_key_encoder_set(MACOS_OPTION_AS_ALT)",
                sys::ghostty_key_encoder_set(
                    self.key,
                    sys::KeyEncoderOption::MACOS_OPTION_AS_ALT,
                    (&raw const option).cast(),
                ),
            )?;
            Failed::check("ghostty_key_event_set_action", sys::ghostty_key_event_set_action(self.key_event, action))?;
            Failed::check(
                "ghostty_key_event_set_key",
                sys::ghostty_key_event_set_key(self.key_event, key_named(key.code)),
            )?;
            Failed::check(
                "ghostty_key_event_set_mods",
                sys::ghostty_key_event_set_mods(self.key_event, key.modifiers),
            )?;
            Failed::check(
                "ghostty_key_event_set_consumed_mods",
                sys::ghostty_key_event_set_consumed_mods(self.key_event, consumed(key)),
            )?;
            Failed::check(
                "ghostty_key_event_set_composing",
                sys::ghostty_key_event_set_composing(self.key_event, false),
            )?;
            Failed::check(
                "ghostty_key_event_set_utf8",
                sys::ghostty_key_event_set_utf8(self.key_event, sys::String::from(text)),
            )?;
            Failed::check(
                "ghostty_key_event_set_unshifted_codepoint",
                sys::ghostty_key_event_set_unshifted_codepoint(self.key_event, key.unshifted),
            )?;
        }
        let (encoder, event) = (self.key, self.key_event);
        // SAFETY: the buffer is as long as the length passed
        let encoded = sized("ghostty_key_encoder_encode", |buffer, length, written| unsafe {
            sys::ghostty_key_encoder_encode(encoder, event, buffer, length, written)
        });
        // **The event keeps a pointer to the text and not a copy**, and the text is the caller's: left
        // set, the event would hold a pointer into a message that has been freed until the next key
        // SAFETY: the handle is live, and an empty string is one whose pointer the core does not read
        unsafe { sys::ghostty_key_event_set_utf8(self.key_event, sys::String::from("")) };
        encoded
    }

    /// What the pointer sends the program now: nothing unless it asked, and nothing for a move within a
    /// cell it was already told about.
    ///
    /// `held` is the buttons the view sending this is holding down, as bits, which decides whether a move
    /// is a drag. **It is the view's and not the session's**: a release that never arrives — the button
    /// let go outside the window, the view closed mid-drag — would otherwise leave every other view of
    /// the session dragging for ever.
    pub fn pointer(&mut self, terminal: &Terminal, pointer: &Pointer, held: &mut u8) -> Result<Vec<u8>, Failed> {
        // **Numbers a view sent, so any float at all.** One that is not a number, or is infinite, is no
        // position; the core is given none of it
        if ![pointer.x, pointer.y, pointer.cell_width, pointer.cell_height].iter().all(|value| value.is_finite()) {
            return Ok(Vec::new());
        }
        let cell_width = pointer.cell_width.clamp(1.0, MAX_CELL_PIXELS);
        let cell_height = pointer.cell_height.clamp(1.0, MAX_CELL_PIXELS);
        let width = f32::from(terminal.cols()?) * cell_width;
        let height = f32::from(terminal.rows()?) * cell_height;
        // **Held to a cell's width outside the grid, and no further.** Outside is something the core has
        // a rule for — a drag is reported at the nearest edge, and a pointer with nothing held is
        // nobody's — so it is still told when the pointer has left; what it is not given is a distance
        // a view made up, which it would turn into a cell number
        let position = sys::MousePosition {
            x: pointer.x.clamp(-cell_width, width + cell_width),
            y: pointer.y.clamp(-cell_height, height + cell_height),
        };
        let (action, button) = match pointer.action {
            0 => (sys::MouseAction::PRESS, button_named(pointer.button)),
            1 => (sys::MouseAction::RELEASE, button_named(pointer.button)),
            // A wheel is a press of the fourth or fifth button, with no release
            3 => (sys::MouseAction::PRESS, Some(sys::MouseButton::FOUR)),
            4 => (sys::MouseAction::PRESS, Some(sys::MouseButton::FIVE)),
            _ => (sys::MouseAction::MOTION, button_named(pointer.button)),
        };
        // Only a button with a name is held: anything else is no button, and has no release to clear it
        if pointer.button < NAMED_BUTTONS {
            match pointer.action {
                0 => *held |= 1 << pointer.button,
                1 => *held &= !(1 << pointer.button),
                _ => {}
            }
        }
        let size = sys::MouseEncoderSize {
            size: std::mem::size_of::<sys::MouseEncoderSize>(),
            screen_width: width as u32,
            screen_height: height as u32,
            cell_width: cell_width as u32,
            cell_height: cell_height as u32,
            ..Default::default()
        };
        // A press counts as held for the event that reports it, and a release no longer does
        let pressed = *held != 0;
        let mut modes = [false; MOUSE_MODES.len()];
        for (on, mode) in modes.iter_mut().zip(MOUSE_MODES) {
            *on = terminal.mode(mode)?;
        }
        if self.mouse_modes != Some(modes) {
            // SAFETY: both handles are live
            Failed::check("ghostty_mouse_encoder_set_from_terminal", unsafe {
                sys::ghostty_mouse_encoder_set_from_terminal(self.mouse, terminal.handle)
            })?;
            let track = true;
            // SAFETY: TRACK_LAST_CELL takes a `bool`
            Failed::check("ghostty_mouse_encoder_set(TRACK_LAST_CELL)", unsafe {
                sys::ghostty_mouse_encoder_set(
                    self.mouse,
                    sys::MouseEncoderOption::TRACK_LAST_CELL,
                    (&raw const track).cast(),
                )
            })?;
            self.mouse_modes = Some(modes);
        }
        let measured = [size.screen_width, size.screen_height, size.cell_width, size.cell_height];
        if self.mouse_size != Some(measured) {
            // SAFETY: SIZE takes a sized `GhosttyMouseEncoderSize`
            Failed::check("ghostty_mouse_encoder_set(SIZE)", unsafe {
                sys::ghostty_mouse_encoder_set(self.mouse, sys::MouseEncoderOption::SIZE, (&raw const size).cast())
            })?;
            self.mouse_size = Some(measured);
        }
        // SAFETY: every handle is live, and each option is given the type it documents
        unsafe {
            Failed::check(
                "ghostty_mouse_encoder_set(ANY_BUTTON_PRESSED)",
                sys::ghostty_mouse_encoder_set(
                    self.mouse,
                    sys::MouseEncoderOption::ANY_BUTTON_PRESSED,
                    (&raw const pressed).cast(),
                ),
            )?;
            Failed::check(
                "ghostty_mouse_event_set_action",
                sys::ghostty_mouse_event_set_action(self.mouse_event, action),
            )?;
            match button {
                Some(button) => {
                    Failed::check(
                        "ghostty_mouse_event_set_button",
                        sys::ghostty_mouse_event_set_button(self.mouse_event, button),
                    )?;
                }
                None => {
                    Failed::check(
                        "ghostty_mouse_event_clear_button",
                        sys::ghostty_mouse_event_clear_button(self.mouse_event),
                    )?;
                }
            }
            Failed::check(
                "ghostty_mouse_event_set_mods",
                sys::ghostty_mouse_event_set_mods(self.mouse_event, pointer.modifiers),
            )?;
            Failed::check(
                "ghostty_mouse_event_set_position",
                sys::ghostty_mouse_event_set_position(self.mouse_event, position),
            )?;
        }
        let (encoder, event) = (self.mouse, self.mouse_event);
        // SAFETY: the buffer is as long as the length passed
        sized("ghostty_mouse_encoder_encode", |buffer, length, written| unsafe {
            sys::ghostty_mouse_encoder_encode(encoder, event, buffer, length, written)
        })
    }
}

impl Drop for Encoders {
    fn drop(&mut self) {
        // SAFETY: each is freed once, by the value that owns it, and each free takes a null
        unsafe {
            sys::ghostty_key_event_free(self.key_event);
            sys::ghostty_key_encoder_free(self.key);
            sys::ghostty_mouse_event_free(self.mouse_event);
            sys::ghostty_mouse_encoder_free(self.mouse);
        }
    }
}

fn button_named(button: u8) -> Option<sys::MouseButton::Type> {
    match button {
        0 => Some(sys::MouseButton::LEFT),
        1 => Some(sys::MouseButton::MIDDLE),
        2 => Some(sys::MouseButton::RIGHT),
        _ => None,
    }
}

/// Runs an encode that writes into a buffer, growing the buffer once when the first is too small.
fn sized(
    call: &'static str,
    mut encode: impl FnMut(*mut u8, usize, *mut usize) -> sys::Result::Type,
) -> Result<Vec<u8>, Failed> {
    let mut buffer = vec![0u8; 128];
    let mut written: usize = 0;
    let mut result = encode(buffer.as_mut_ptr(), buffer.len(), &raw mut written);
    if result == sys::Result::OUT_OF_SPACE {
        buffer.resize(written, 0);
        result = encode(buffer.as_mut_ptr(), buffer.len(), &raw mut written);
    }
    Failed::check(call, result)?;
    buffer.truncate(written);
    Ok(buffer)
}

/// Pasted text as the program should receive it, or a refusal when it could run a command.
///
/// **What is dangerous depends on whether the program brackets its pastes.** Where it does, a line ending
/// is just text and the one thing that can run a command is the marker that ends the bracket. Where it
/// does not, every line ending is the Enter key. Either is refused unless the caller says the person was
/// asked; allowed, the text still goes through the core's encoder, which blanks the control bytes that
/// made it dangerous.
pub fn paste(terminal: &Terminal, text: &str, allow_unsafe: bool) -> Result<Pasted, Failed> {
    let bracketed = terminal.mode(BRACKETED_PASTE)?;
    let bytes = text.as_bytes();
    let dangerous = if bracketed {
        bytes.windows(PASTE_END.len()).any(|window| window == PASTE_END)
    } else {
        bytes.iter().any(|byte| *byte == b'\n' || *byte == b'\r')
    };
    if dangerous && !allow_unsafe {
        return Ok(Pasted::Unsafe);
    }
    // The encoder rewrites its input in place, so it is given a copy
    let mut data = bytes.to_vec();
    let (pointer, length) = (data.as_mut_ptr().cast::<std::ffi::c_char>(), data.len());
    // SAFETY: `data` is live and `length` long for both calls `sized` may make
    let encoded = sized("ghostty_paste_encode", |buffer, room, written| unsafe {
        sys::ghostty_paste_encode(pointer, length, bracketed, buffer, room, written)
    })?;
    Ok(Pasted::Encoded(encoded))
}

/// What the program is told about focus, which is nothing unless it asked.
pub fn focus(terminal: &Terminal, gained: bool) -> Result<Vec<u8>, Failed> {
    if !terminal.mode(FOCUS_EVENTS)? {
        return Ok(Vec::new());
    }
    let event = if gained { sys::FocusEvent::GAINED } else { sys::FocusEvent::LOST };
    // SAFETY: the buffer is as long as the length passed
    sized("ghostty_focus_encode", |buffer, length, written| unsafe {
        sys::ghostty_focus_encode(event, buffer, length, written)
    })
}
