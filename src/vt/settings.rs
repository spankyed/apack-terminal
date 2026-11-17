//! What a terminal is told about where it runs, kept so that a terminal put in another's place is told
//! the same.
//!
//! **A setting is not a terminal's state.** What a program has done to a terminal — its screen, its
//! modes, the cursor it asked for — is the terminal's, and goes wherever the terminal goes
//! (`keyframe`). What the broker told it is the session's: how much history to keep, what to answer
//! when asked its name or its colours, the cursor a reset returns to, whether it takes images, where
//! its reports go. A recording restored into a session is another terminal's state under this
//! session's settings, so each setting is kept as it is made and made again on the terminal a restore
//! puts in place (`Terminal::restore`).
//!
//! **There is one way to make a setting, and it is the way it is kept.** `Terminal::set` tells the core
//! and keeps what it was told, and `Setting::tell` is the one place a setting reaches the core. A
//! setting a restore forgets is therefore one that was never made here, and `tests/keyframes.rs` holds
//! every kind to an answer a terminal gives only when it was told.

use ghostty_vt_rs_sys as sys;

use super::{Failed, Reported, Terminal};
use crate::markers::PAYLOAD_CAP;

/// One thing a terminal is told about where it runs.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Setting {
    /// The most lines of history kept.
    ScrollbackLines(usize),
    /// The most bytes of history kept, whatever the line limit would allow at the grid's width.
    ScrollbackBytes(usize),
    /// The terminal name a program reads from `TERM`.
    TerminfoName(String),
    /// What a reset returns a mode to.
    ModeDefault { mode: u16, value: bool },
    /// The cursor a program gets when it asks for the default one (`CSI 0 q`).
    CursorDefaults { style: sys::CursorStyle::Type, blinks: bool },
    /// How many bytes of images are kept, which is none for a terminal that takes no images.
    ImageStorage(u64),
    /// Whether the core reads a file or a shared memory object a program names.
    NamedImages(bool),
    /// How much one Kitty graphics command may carry, or the core's own most.
    KittyCommandBytes(Option<usize>),
    /// The colours the terminal is drawn in, which is what it answers when a program asks.
    Colors { foreground: [u8; 3], background: [u8; 3], cursor: Option<[u8; 3]>, ansi: Vec<[u8; 3]> },
    /// Where what the terminal reports is collected (`Terminal::route_reports`).
    Reports(*mut Reported),
}

impl Setting {
    /// Whether `self` is the same setting as `other`, made again: the same kind, and for a mode's default
    /// the same mode.
    fn replaces(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::ModeDefault { mode, .. }, Self::ModeDefault { mode: held, .. }) => mode == held,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }

    /// Tells the terminal `handle` this setting.
    ///
    /// `again` is for a terminal that has a state of its own already — one a restore decoded — which a
    /// setting must leave as it is: the core sets a mode's current value with its default, so the
    /// current value is read first and put back.
    fn tell(&self, handle: sys::Terminal, again: bool) -> Result<(), Failed> {
        match self {
            Self::ScrollbackLines(lines) => set(
                handle,
                sys::TerminalOption::SCROLLBACK_MAX_LINES,
                lines,
                "ghostty_terminal_set(SCROLLBACK_MAX_LINES)",
            ),
            Self::ScrollbackBytes(bytes) => set(
                handle,
                sys::TerminalOption::SCROLLBACK_MAX_BYTES,
                bytes,
                "ghostty_terminal_set(SCROLLBACK_MAX_BYTES)",
            ),
            Self::TerminfoName(name) => {
                let text = sys::String::from(name.as_str());
                set(handle, sys::TerminalOption::TERMINFO_NAME, &text, "ghostty_terminal_set(TERMINFO_NAME)")
            }
            Self::ModeDefault { mode, value } => {
                let now = again.then(|| mode_of(handle, *mode)).transpose()?;
                let config = sys::TerminalModeConfig { mode: *mode, value: *value };
                set(handle, sys::TerminalOption::MODE_DEFAULT, &config, "ghostty_terminal_set(MODE_DEFAULT)")?;
                match now {
                    Some(now) if now != *value => {
                        let config = sys::TerminalModeConfig { mode: *mode, value: now };
                        set(handle, sys::TerminalOption::MODE, &config, "ghostty_terminal_set(MODE)")
                    }
                    _ => Ok(()),
                }
            }
            Self::CursorDefaults { style, blinks } => {
                set(
                    handle,
                    sys::TerminalOption::DEFAULT_CURSOR_STYLE,
                    style,
                    "ghostty_terminal_set(DEFAULT_CURSOR_STYLE)",
                )?;
                set(
                    handle,
                    sys::TerminalOption::DEFAULT_CURSOR_BLINK,
                    blinks,
                    "ghostty_terminal_set(DEFAULT_CURSOR_BLINK)",
                )
            }
            Self::ImageStorage(limit) => set(
                handle,
                sys::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT,
                limit,
                "ghostty_terminal_set(KITTY_IMAGE_STORAGE_LIMIT)",
            ),
            Self::NamedImages(files) => {
                set(
                    handle,
                    sys::TerminalOption::KITTY_IMAGE_MEDIUM_FILE,
                    files,
                    "ghostty_terminal_set(KITTY_IMAGE_MEDIUM_FILE)",
                )?;
                set(
                    handle,
                    sys::TerminalOption::KITTY_IMAGE_MEDIUM_SHARED_MEM,
                    files,
                    "ghostty_terminal_set(KITTY_IMAGE_MEDIUM_SHARED_MEM)",
                )
            }
            Self::KittyCommandBytes(bytes) => {
                let value = bytes
                    .as_ref()
                    .map_or(std::ptr::null(), |bytes| std::ptr::from_ref(bytes).cast::<std::ffi::c_void>());
                // SAFETY: APC_MAX_BYTES_KITTY takes a `size_t*`, or NULL for the core's own
                let result =
                    unsafe { sys::ghostty_terminal_set(handle, sys::TerminalOption::APC_MAX_BYTES_KITTY, value) };
                Failed::check("ghostty_terminal_set(APC_MAX_BYTES_KITTY)", result)
            }
            Self::Colors { foreground, background, cursor, ansi } => {
                tell_colors(handle, *foreground, *background, *cursor, ansi)
            }
            Self::Reports(collector) => tell_reports(handle, *collector),
        }
    }
}

/// Sets an option to the value `value` points at.
///
/// The type an option takes is the caller's to get right, which is why this is private to the one place
/// that names every option: `Setting::tell`.
fn set<T>(
    handle: sys::Terminal,
    option: sys::TerminalOption::Type,
    value: &T,
    what: &'static str,
) -> Result<(), Failed> {
    // SAFETY: the handle is live, and each caller passes the type its option documents
    let result =
        unsafe { sys::ghostty_terminal_set(handle, option, std::ptr::from_ref(value).cast::<std::ffi::c_void>()) };
    Failed::check(what, result)
}

/// Whether `mode` is set on `handle` now.
fn mode_of(handle: sys::Terminal, mode: u16) -> Result<bool, Failed> {
    let mut config = sys::TerminalModeConfig { mode, value: false };
    // SAFETY: MODE reads `mode` from and writes `value` to a `GhosttyTerminalModeConfig`
    let result = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::MODE, (&raw mut config).cast()) };
    Failed::check("ghostty_terminal_get(MODE)", result)?;
    Ok(config.value)
}

/// `ansi` replaces the first entries of the palette — the sixteen a theme names — and leaves the rest at
/// the library's, which are the standard cube and greys no theme redefines.
fn tell_colors(
    handle: sys::Terminal,
    foreground: [u8; 3],
    background: [u8; 3],
    cursor: Option<[u8; 3]>,
    ansi: &[[u8; 3]],
) -> Result<(), Failed> {
    let rgb = |[r, g, b]: [u8; 3]| sys::ColorRgb { r, g, b };
    set(handle, sys::TerminalOption::COLOR_FOREGROUND, &rgb(foreground), "ghostty_terminal_set(COLOR_FOREGROUND)")?;
    set(handle, sys::TerminalOption::COLOR_BACKGROUND, &rgb(background), "ghostty_terminal_set(COLOR_BACKGROUND)")?;
    if let Some(cursor) = cursor {
        set(handle, sys::TerminalOption::COLOR_CURSOR, &rgb(cursor), "ghostty_terminal_set(COLOR_CURSOR)")?;
    }
    if ansi.is_empty() {
        return Ok(());
    }
    let mut palette = [sys::ColorRgb { r: 0, g: 0, b: 0 }; 256];
    // SAFETY: COLOR_PALETTE_DEFAULT writes exactly 256 `GhosttyColorRgb`, which is what `palette` is
    let result = unsafe {
        sys::ghostty_terminal_get(
            handle,
            sys::TerminalData::COLOR_PALETTE_DEFAULT,
            palette.as_mut_ptr().cast::<std::ffi::c_void>(),
        )
    };
    Failed::check("ghostty_terminal_get(COLOR_PALETTE_DEFAULT)", result)?;
    for (slot, color) in palette.iter_mut().zip(ansi) {
        *slot = rgb(*color);
    }
    // COLOR_PALETTE takes a pointer to exactly 256 `GhosttyColorRgb`
    set(handle, sys::TerminalOption::COLOR_PALETTE, &palette, "ghostty_terminal_set(COLOR_PALETTE)")
}

/// Routes what the terminal reports to `collector` (`Terminal::route_reports`, which says what must be
/// true of it).
fn tell_reports(handle: sys::Terminal, collector: *mut Reported) -> Result<(), Failed> {
    // SAFETY: userdata is an untyped pointer the library only ever hands back to the callbacks below
    let result = unsafe { sys::ghostty_terminal_set(handle, sys::TerminalOption::USERDATA, collector.cast()) };
    Failed::check("ghostty_terminal_set(USERDATA)", result)?;

    // **The function pointer itself, not a pointer to it.** A callback option is documented that way and
    // the difference is not subtle: passing `&callback` had the library call the address of a stack local,
    // which is a SIGBUS rather than a wrong answer.
    //
    // The first is what a terminal answers a program with. The second is the shell's own account of
    // itself, in the standard family (OSC 133): the core reports each step of a command as an event and
    // decodes the command line; what it does not do is interpret, so the mapping is the broker's contract
    // rather than the core's reading of it. The last six are what a program asks of the world outside
    // its screen — a title, a bell, the clipboard, a notification, how far along it is — which the core
    // reads and reports, and none of which is drawn here.
    for (option, callback, what) in [
        (
            sys::TerminalOption::WRITE_PTY,
            super::collect_reply as *const std::ffi::c_void,
            "ghostty_terminal_set(WRITE_PTY)",
        ),
        (
            sys::TerminalOption::SEMANTIC_PROMPT,
            super::note_semantic_prompt as *const std::ffi::c_void,
            "ghostty_terminal_set(SEMANTIC_PROMPT)",
        ),
        (
            sys::TerminalOption::PWD_CHANGED,
            super::note_pwd as *const std::ffi::c_void,
            "ghostty_terminal_set(PWD_CHANGED)",
        ),
        (sys::TerminalOption::SIZE, super::report_size as *const std::ffi::c_void, "ghostty_terminal_set(SIZE)"),
        (
            sys::TerminalOption::RENDER_HOLD,
            super::note_render_hold as *const std::ffi::c_void,
            "ghostty_terminal_set(RENDER_HOLD)",
        ),
        (
            sys::TerminalOption::TITLE_CHANGED,
            super::note_title as *const std::ffi::c_void,
            "ghostty_terminal_set(TITLE_CHANGED)",
        ),
        (sys::TerminalOption::BELL, super::note_bell as *const std::ffi::c_void, "ghostty_terminal_set(BELL)"),
        (
            sys::TerminalOption::CLIPBOARD_WRITE,
            super::note_clipboard_write as *const std::ffi::c_void,
            "ghostty_terminal_set(CLIPBOARD_WRITE)",
        ),
        (
            sys::TerminalOption::DESKTOP_NOTIFICATION,
            super::note_notification as *const std::ffi::c_void,
            "ghostty_terminal_set(DESKTOP_NOTIFICATION)",
        ),
        (
            sys::TerminalOption::PROGRESS_REPORT,
            super::note_progress as *const std::ffi::c_void,
            "ghostty_terminal_set(PROGRESS_REPORT)",
        ),
        (
            sys::TerminalOption::UNKNOWN_SEQUENCE,
            super::note_unknown_sequence as *const std::ffi::c_void,
            "ghostty_terminal_set(UNKNOWN_SEQUENCE)",
        ),
    ] {
        // SAFETY: each function has the signature its option documents, and the library only calls it
        // with the userdata set above
        Failed::check(what, unsafe { sys::ghostty_terminal_set(handle, option, callback) })?;
    }
    // Without a limit the core keeps nothing of a sequence it does not read, and reports none
    set(handle, sys::TerminalOption::UNKNOWN_MAX_BYTES, &PAYLOAD_CAP, "ghostty_terminal_set(UNKNOWN_MAX_BYTES)")
}

impl Terminal {
    /// Makes a setting: tells the core, and keeps what it was told.
    pub(super) fn set(&mut self, setting: Setting) -> Result<(), Failed> {
        setting.tell(self.handle, false)?;
        match self.settings.iter_mut().find(|held| setting.replaces(held)) {
            Some(held) => *held = setting,
            None => self.settings.push(setting),
        }
        Ok(())
    }

    /// Tells the terminal `handle`, which has a state of its own, every setting this one was told.
    pub(super) fn tell_again(&self, handle: sys::Terminal) -> Result<(), Failed> {
        self.settings.iter().try_for_each(|setting| setting.tell(handle, true))
    }

    /// How many bytes of images this terminal was told to keep, where it was told.
    pub(super) fn image_storage(&self) -> Option<u64> {
        self.settings.iter().find_map(|setting| match setting {
            Setting::ImageStorage(limit) => Some(*limit),
            _ => None,
        })
    }
}
