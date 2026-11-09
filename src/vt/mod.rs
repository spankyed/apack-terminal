//! The `libghostty-vt` side of Phase 0a's comparison: a terminal held safely, so the conformance runner
//! can drive it the way it drives `screen::Screen`.
//!
//! **This exists to be measured, not to ship.** The goal it serves (`goal-one-vt-core.md`) adopts one VT
//! core on both sides of the broker/renderer split, and its first phase is the question of whether this
//! core produces the same screen as the `alacritty_terminal` wrapper beside it. Phase B promotes what is
//! here into `screen.rs`; a *no* takes the whole module and its dependency back out.
//!
//! **The handle is owned and the pointer never leaves.** A `GhosttyTerminal` is a raw pointer the library
//! allocates, and reading a cell hands back a reference into its page list that the next write invalidates
//! — so every read here copies out before returning, and nothing in the module's surface is a borrow of
//! library memory.
use ghostty_vt_rs_sys as sys;

use crate::markers::cwd_from_url;
use crate::screen::Facts;

pub mod screen;
pub use screen::GhosttyScreen;

/// One terminal, freed with the value.
pub struct Terminal {
    handle: sys::Terminal,
}

// The library allocates and frees this handle and nothing here shares it between threads: the broker gives
// each session its own, which is the arrangement `screen::Screen` already has under a mutex.
unsafe impl Send for Terminal {}

impl Terminal {
    /// A terminal of `cols` by `rows`, with the library's default allocator.
    pub fn new(cols: u16, rows: u16) -> Result<Self, Failed> {
        let mut handle: sys::Terminal = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer, and a null allocator asks for the library's own
        let result = unsafe { sys::ghostty_terminal_new(std::ptr::null(), &raw mut handle, cols, rows) };
        Failed::check("ghostty_terminal_new", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_terminal_new", code: sys::Result::SUCCESS });
        }
        Ok(Self { handle })
    }

    /// Parses `bytes`, exactly as the pty delivered them.
    ///
    /// Takes whatever chunk arrived, because that is the shape terminal output has: an escape sequence, a
    /// UTF-8 code point or an OSC string is routinely split across reads, and holding the pieces is the
    /// parser's job rather than the caller's.
    pub fn feed(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // SAFETY: the handle is live for `&mut self`, and the slice outlives the call
        unsafe { sys::ghostty_terminal_vt_write(self.handle, bytes.as_ptr(), bytes.len()) };
    }

    /// A `u16` the terminal answers for, by key.
    fn read_u16(&self, key: sys::TerminalData::Type, what: &'static str) -> Result<u16, Failed> {
        let mut value: u16 = 0;
        // SAFETY: every key passed here is documented as writing a `uint16_t`, which is what `value` is.
        // Passing a key of another width would write past it, which is why the keys are not a parameter of
        // a public function.
        let result =
            unsafe { sys::ghostty_terminal_get(self.handle, key, (&raw mut value).cast::<std::ffi::c_void>()) };
        Failed::check(what, result)?;
        Ok(value)
    }

    /// The grid's width in cells, as the terminal has it.
    pub fn cols(&self) -> Result<u16, Failed> {
        self.read_u16(sys::TerminalData::COLS, "ghostty_terminal_get(COLS)")
    }

    /// The grid's height in cells.
    pub fn rows(&self) -> Result<u16, Failed> {
        self.read_u16(sys::TerminalData::ROWS, "ghostty_terminal_get(ROWS)")
    }

    /// Where the cursor is **within the active area**, which is what the key answers and what the wrapper
    /// beside this reports: a snapshot's history does not move it.
    pub fn cursor_row(&self) -> Result<u16, Failed> {
        self.read_u16(sys::TerminalData::CURSOR_Y, "ghostty_terminal_get(CURSOR_Y)")
    }

    pub fn cursor_column(&self) -> Result<u16, Failed> {
        self.read_u16(sys::TerminalData::CURSOR_X, "ghostty_terminal_get(CURSOR_X)")
    }

    /// Whether an alternate-screen application is showing.
    pub fn alt_screen(&self) -> Result<bool, Failed> {
        let mut screen: sys::TerminalScreen::Type = sys::TerminalScreen::PRIMARY;
        // SAFETY: ACTIVE_SCREEN writes a `GhosttyTerminalScreen`, which is what `screen` is
        let result = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::TerminalData::ACTIVE_SCREEN,
                (&raw mut screen).cast::<std::ffi::c_void>(),
            )
        };
        Failed::check("ghostty_terminal_get(ACTIVE_SCREEN)", result)?;
        Ok(screen == sys::TerminalScreen::ALTERNATE)
    }

    /// How many rows of scrollback the terminal is holding, which is where a screen row zero is.
    pub fn scrollback_rows(&self) -> Result<u32, Failed> {
        let mut rows: usize = 0;
        // SAFETY: SCROLLBACK_ROWS writes a `size_t`
        let result = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::TerminalData::SCROLLBACK_ROWS,
                (&raw mut rows).cast::<std::ffi::c_void>(),
            )
        };
        Failed::check("ghostty_terminal_get(SCROLLBACK_ROWS)", result)?;
        Ok(u32::try_from(rows).unwrap_or(u32::MAX))
    }

    /// Resizes the grid. The primary screen reflows if wraparound is on; the alternate screen does not.
    ///
    /// **The cell size in pixels is zero on purpose.** The core takes it so that a renderer can report
    /// what a program asks for in pixels, and this broker has no renderer: it owns the pty and the grid,
    /// and a window's cell metrics are that window's. `screen::Screen` has no equivalent parameter, so
    /// passing a made-up figure here would be inventing a fact to make a signature match.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), Failed> {
        // SAFETY: the handle is live for `&mut self`
        let result = unsafe { sys::ghostty_terminal_resize(self.handle, cols.max(1), rows.max(1), 0, 0) };
        Failed::check("ghostty_terminal_resize", result)
    }

    /// The cell at a **screen** coordinate, where row zero is the oldest line still held.
    ///
    /// `None` for a coordinate the terminal has no cell at, which a caller reading a rectangle will hit:
    /// a row shorter than the grid's width is not an error.
    ///
    /// **The handle is read and dropped inside this call.** A grid reference points into the library's
    /// page list and the next write invalidates it, so one must never be held across a `feed`; returning
    /// the cell value rather than the reference is what makes that impossible here.
    pub fn cell(&self, column: u16, row: u32) -> Result<Option<sys::Cell>, Failed> {
        let point = sys::Point {
            tag: sys::PointTag::SCREEN,
            value: sys::PointValue { coordinate: sys::PointCoordinate { x: column, y: row } },
        };
        let mut reference = sys::GridRef::default();
        // SAFETY: `reference` is a valid out-pointer and the point is by value
        let found = unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut reference) };
        if found != sys::Result::SUCCESS {
            return Ok(None);
        }
        let mut cell: sys::Cell = 0;
        // SAFETY: the reference was just filled by the call above and nothing has written since
        let result = unsafe { sys::ghostty_grid_ref_cell(&raw const reference, &raw mut cell) };
        if result != sys::Result::SUCCESS {
            return Ok(None);
        }
        Ok(Some(cell))
    }

    /// A cell's primary codepoint. Zero is an empty cell.
    pub fn cell_codepoint(&self, cell: sys::Cell) -> Result<u32, Failed> {
        let mut codepoint: u32 = 0;
        // SAFETY: CODEPOINT writes a `uint32_t`
        let result = unsafe {
            sys::ghostty_cell_get(cell, sys::CellData::CODEPOINT, (&raw mut codepoint).cast::<std::ffi::c_void>())
        };
        Failed::check("ghostty_cell_get(CODEPOINT)", result)?;
        Ok(codepoint)
    }

    /// Whether a cell is narrow, wide, or one of the two spacers a wide character leaves beside itself.
    pub fn cell_wide(&self, cell: sys::Cell) -> Result<sys::CellWide::Type, Failed> {
        let mut wide: sys::CellWide::Type = sys::CellWide::NARROW;
        // SAFETY: WIDE writes a `GhosttyCellWide`
        let result =
            unsafe { sys::ghostty_cell_get(cell, sys::CellData::WIDE, (&raw mut wide).cast::<std::ffi::c_void>()) };
        Failed::check("ghostty_cell_get(WIDE)", result)?;
        Ok(wide)
    }

    /// Bounds the scrollback to `lines`, which the core does not do by default.
    pub fn set_scrollback_lines(&self, lines: usize) -> Result<(), Failed> {
        self.set_usize(sys::TerminalOption::SCROLLBACK_MAX_LINES, lines, "ghostty_terminal_set(SCROLLBACK_MAX_LINES)")
    }

    /// Lifts the byte ceiling, so the line limit is the only rule bounding history.
    ///
    /// **The core's default is 10,000 bytes, and a null value does not remove it** — measured by reading
    /// the limits back, after a null was assumed to work because the *line* option documents one as
    /// removing its limit. Left at the default, a session held 757 lines of short output where this broker
    /// promises `HISTORY_LINES`, and fewer the longer the lines were; a reattach would then read back less
    /// than the window was told it could have. `alacritty_terminal` had no byte rule at all, so this is
    /// what keeps the bound the one the snapshots in `tests/parser.rs` were recorded under.
    pub fn set_scrollback_unbounded_bytes(&self) -> Result<(), Failed> {
        self.set_usize(
            sys::TerminalOption::SCROLLBACK_MAX_BYTES,
            usize::MAX,
            "ghostty_terminal_set(SCROLLBACK_MAX_BYTES)",
        )
    }

    /// The terminal name a program reads from `TERM`, which decides what it emits.
    pub fn set_terminfo_name(&self, name: &str) -> Result<(), Failed> {
        self.set_str(sys::TerminalOption::TERMINFO_NAME, name, "ghostty_terminal_set(TERMINFO_NAME)")
    }

    /// Whether a cell is one of the two spacers a wide character leaves beside itself, holding no
    /// codepoint of its own.
    pub fn cell_is_spacer(&self, cell: sys::Cell) -> Result<bool, Failed> {
        let wide = self.cell_wide(cell)?;
        Ok(wide == sys::CellWide::SPACER_TAIL || wide == sys::CellWide::SPACER_HEAD)
    }

    /// How many rows the terminal holds in all, viewport included.
    pub fn total_rows(&self) -> Result<u32, Failed> {
        let mut rows: usize = 0;
        // SAFETY: TOTAL_ROWS writes a `size_t`
        let result = unsafe {
            sys::ghostty_terminal_get(
                self.handle,
                sys::TerminalData::TOTAL_ROWS,
                (&raw mut rows).cast::<std::ffi::c_void>(),
            )
        };
        Failed::check("ghostty_terminal_get(TOTAL_ROWS)", result)?;
        Ok(u32::try_from(rows).unwrap_or(u32::MAX))
    }

    /// What the terminal says its own limits are, for a diagnostic.
    pub fn configured_limits(&self) -> (Option<usize>, Option<usize>) {
        let read = |key: sys::TerminalData::Type| {
            let mut value: usize = 0;
            // SAFETY: both keys write a `size_t`
            let result =
                unsafe { sys::ghostty_terminal_get(self.handle, key, (&raw mut value).cast::<std::ffi::c_void>()) };
            (result == sys::Result::SUCCESS).then_some(value)
        };
        (read(sys::TerminalData::SCROLLBACK_MAX_LINES), read(sys::TerminalData::SCROLLBACK_MAX_BYTES))
    }

    /// Sets an option whose value is a `size_t`.
    fn set_usize(&self, option: sys::TerminalOption::Type, value: usize, what: &'static str) -> Result<(), Failed> {
        // SAFETY: the caller names an option documented as taking a `size_t*`
        let result =
            unsafe { sys::ghostty_terminal_set(self.handle, option, (&raw const value).cast::<std::ffi::c_void>()) };
        Failed::check(what, result)
    }

    /// Sets an option to no value, which is how its limit is removed.
    fn set_null(&self, option: sys::TerminalOption::Type, what: &'static str) -> Result<(), Failed> {
        // SAFETY: a null value is what the option's documentation says removes a limit
        let result = unsafe { sys::ghostty_terminal_set(self.handle, option, std::ptr::null()) };
        Failed::check(what, result)
    }

    /// Routes the replies the terminal makes on a program's behalf to `collector`.
    ///
    /// **A terminal answers questions, and a session that answers none does not start on Windows**:
    /// ConPTY's first act is a cursor-position request and it waits for the reply before initialising.
    /// `screen::Screen` collects these too, and the broker is what has to answer — not the window, whose
    /// parse is a replica that must not be in the loop for a program's control queries.
    ///
    /// # Safety
    ///
    /// `collector` must point at a live `Replies` for as long as this terminal can parse, which means it
    /// must outlive the handle. `GhosttyScreen` owns both and declares the terminal first so it is freed
    /// first.
    pub(crate) unsafe fn route_reports(&self, collector: *mut Reported) -> Result<(), Failed> {
        // SAFETY: userdata is an untyped pointer the library only ever hands back to the callback below
        let result = unsafe { sys::ghostty_terminal_set(self.handle, sys::TerminalOption::USERDATA, collector.cast()) };
        Failed::check("ghostty_terminal_set(USERDATA)", result)?;

        // **The function pointer itself, not a pointer to it.** A callback option is documented that way
        // and the difference is not subtle: passing `&callback` had the library call the address of a stack
        // local, which is a SIGBUS rather than a wrong answer.
        let callback = collect_reply as *const std::ffi::c_void;
        // SAFETY: `collect_reply` has the signature `GhosttyTerminalWritePtyFn` declares, and the library
        // only calls it with the userdata set above
        let result = unsafe { sys::ghostty_terminal_set(self.handle, sys::TerminalOption::WRITE_PTY, callback) };
        Failed::check("ghostty_terminal_set(WRITE_PTY)", result)?;

        // **The shell's own account of itself**, which is what `markers.rs` scans the byte stream for
        // today. The core reports each step of a command as an event and decodes the command line for us;
        // what it does not do is interpret, so the mapping below is the broker's contract rather than the
        // core's reading of it.
        let semantic = note_semantic_prompt as *const std::ffi::c_void;
        // SAFETY: the signature matches `GhosttyTerminalSemanticPromptFn`
        let result = unsafe { sys::ghostty_terminal_set(self.handle, sys::TerminalOption::SEMANTIC_PROMPT, semantic) };
        Failed::check("ghostty_terminal_set(SEMANTIC_PROMPT)", result)?;

        let pwd = note_pwd as *const std::ffi::c_void;
        // SAFETY: the signature matches `GhosttyTerminalPwdChangedFn`
        let result = unsafe { sys::ghostty_terminal_set(self.handle, sys::TerminalOption::PWD_CHANGED, pwd) };
        Failed::check("ghostty_terminal_set(PWD_CHANGED)", result)
    }

    /// Sets an option whose value is a borrowed string.
    fn set_str(&self, option: sys::TerminalOption::Type, value: &str, what: &'static str) -> Result<(), Failed> {
        let text = sys::String::from(value);
        // SAFETY: `text` borrows `value`, which outlives the call
        let result =
            unsafe { sys::ghostty_terminal_set(self.handle, option, (&raw const text).cast::<std::ffi::c_void>()) };
        Failed::check(what, result)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: freed once, by the value that owns it
        unsafe { sys::ghostty_terminal_free(self.handle) };
    }
}

/// A call the library refused, with the call's name rather than only its code.
///
/// **`UNSUPPORTED` is the one worth telling apart**, and it is why this carries the name: the library
/// returns it for a key a build does not know *and* for a feature it was compiled without, and the
/// functions stay exported either way. A capability this broker relies on that answers `UNSUPPORTED` is a
/// finding about the build, not a bug in the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub call: &'static str,
    pub code: sys::Result::Type,
}

impl Failed {
    fn check(call: &'static str, code: sys::Result::Type) -> Result<(), Self> {
        if code == sys::Result::SUCCESS {
            return Ok(());
        }
        Err(Self { call, code })
    }
}

impl std::fmt::Display for Failed {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // **`UNSUPPORTED` is called out rather than left as a number**, because it means something a
        // reader would otherwise have to look up: the library returns it for a key an older build does not
        // know and for a feature compiled out, with the function exported either way. Phase B's feature
        // assertion is what will refuse that for a capability the broker relies on (the goal's D7); here it
        // is the difference between "this build lacks it" and "the arguments were wrong".
        let note = if self.code == sys::Result::UNSUPPORTED { " (this build does not provide it)" } else { "" };
        write!(out, "{} failed with {}{note}", self.call, self.code)
    }
}

impl std::error::Error for Failed {}

/// Everything the library reports to its host, collected as it arrives.
///
/// Boxed and owned by the screen that routed it, because its address is what the library is handed.
#[derive(Default)]
pub struct Reported {
    /// What the terminal wants said back to the program, kept rather than discarded.
    replies: Vec<String>,
    /// What the shell has claimed about itself, in `screen`'s own vocabulary.
    facts: Facts,
}

impl Reported {
    /// Each reply is written once, so they are taken rather than read.
    pub fn take_replies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.replies)
    }

    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    /// Takes a marker the core does not read — the OSC 633 family — into the same facts its own callbacks
    /// fill, so a caller has one account of the shell rather than two to merge.
    pub fn apply(&mut self, marker: crate::markers::Marker) {
        self.facts.apply(marker);
    }
}

/// The library's callback, which copies before returning because the data is only valid for the call.
///
/// A reply is a control sequence rather than text, but `screen::Screen` collects these as strings and the
/// comparison is against that, so the conversion is the same one: lossy, since a sequence is ASCII in
/// practice and a replacement character is a visible difference rather than a silent one.
unsafe extern "C" fn collect_reply(
    _terminal: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || data.is_null() || len == 0 {
        return;
    }
    // SAFETY: `userdata` is the pointer `route_replies` was given, which its caller keeps alive for the
    // terminal's life; `data` and `len` describe a slice the library promises for the duration of the call
    let (reported, bytes) = unsafe { (&mut *userdata.cast::<Reported>(), std::slice::from_raw_parts(data, len)) };
    reported.replies.push(String::from_utf8_lossy(bytes).into_owned());
}

/// One step of a command, as the shell reported it.
///
/// **The mapping is the broker's, not the core's.** A prompt being drawn means whatever ran has finished
/// whether or not the shell said so, and an exit status is absent rather than zero when the shell reported
/// none — a shell that says nothing about how a command ended is not saying it succeeded. Both of those
/// are `screen::Facts`' contract and `markers.rs` already holds them; this reads the core's event into the
/// same shape instead of inventing a second one.
unsafe extern "C" fn note_semantic_prompt(
    _terminal: sys::Terminal,
    userdata: *mut std::ffi::c_void,
    event: *const sys::TerminalSemanticPrompt,
) {
    if userdata.is_null() || event.is_null() {
        return;
    }
    // SAFETY: both pointers are the library's, valid for this call
    let (reported, event) = unsafe { (&mut *userdata.cast::<Reported>(), &*event) };
    match event.kind {
        sys::SemanticPromptKind::GHOSTTY_SEMANTIC_PROMPT_PROMPT_START => reported.facts.running = false,
        sys::SemanticPromptKind::GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => {
            reported.facts.running = true;
            // SAFETY: the library owns the bytes for this call, and this copies them
            if let Some(line) = unsafe { text_of(&event.command) } {
                reported.facts.command = Some(line);
            }
        }
        sys::SemanticPromptKind::GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => {
            reported.facts.running = false;
            reported.facts.exit_status = event.has_exit_code.then_some(event.exit_code);
        }
        _ => {}
    }
}

/// The working directory the shell reported, read back inside the callback as the option requires.
///
/// **The core stores the raw bytes and says so**: for OSC 7 that is a `file://` URI, which this decodes
/// with the broker's own decoder. A cleared pwd arrives as a zero-length string and is left alone rather
/// than recorded as an empty path.
unsafe extern "C" fn note_pwd(terminal: sys::Terminal, userdata: *mut std::ffi::c_void) {
    if userdata.is_null() {
        return;
    }
    let mut value = sys::String::default();
    // SAFETY: PWD writes a `GhosttyString`, and the call is inside the callback the option documents
    let result = unsafe {
        sys::ghostty_terminal_get(terminal, sys::TerminalData::PWD, (&raw mut value).cast::<std::ffi::c_void>())
    };
    if result != sys::Result::SUCCESS {
        return;
    }
    // SAFETY: `userdata` is the collector, and `value` was just filled by the library
    let (reported, text) = unsafe { (&mut *userdata.cast::<Reported>(), text_of(&value)) };
    let Some(text) = text else { return };
    // A bare path (OSC 9, OSC 1337) is already one; only OSC 7's URI needs taking apart
    reported.facts.cwd = Some(cwd_from_url(&text).unwrap_or(text));
}

/// A `GhosttyString` copied out, or `None` when it is empty.
///
/// # Safety
///
/// `text` must be a string the library filled and still owns for this call.
unsafe fn text_of(text: &sys::String) -> Option<String> {
    if text.ptr.is_null() || text.len == 0 {
        return None;
    }
    // SAFETY: the caller promises the pointer and length describe live bytes
    let bytes = unsafe { std::slice::from_raw_parts(text.ptr, text.len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}
