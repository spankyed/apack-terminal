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
