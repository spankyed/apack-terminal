//! `screen::Screen`'s surface over `libghostty-vt`, so the two can be driven by one harness.
//!
//! **It answers `screen`'s own types** — `Snapshot`, `Facts`, `Match` — rather than types of its own,
//! because the whole of Phase 0a is comparing the two and those already derive `PartialEq`. A second
//! vocabulary here would mean a conversion to argue about on top of the parse.
//!
//! **What it does not do is interpret.** `facts()` is empty until Phase B wires the core's callbacks
//! (`OPT_SEMANTIC_PROMPT`, `OPT_PWD_CHANGED`), and `search` is `screen`'s own algorithm over this
//! snapshot, not the core's `search.h`. Both are deliberate: 0a's question is whether the grids agree, and
//! answering it with half a port of the semantics on top would confuse a parse difference with a wiring
//! difference.
use ghostty_vt_rs_sys as sys;

use crate::screen::{Facts, HISTORY_LINES, Snapshot};
use crate::vt::{Failed, Reported, Terminal};

/// The terminal advertised to the program, which has to match what the broker's pty says.
///
/// A program decides what to emit from `TERM`, so a core told it is something else can answer a query
/// differently and the difference would read as a parse difference rather than a configuration one.
const TERMINFO_NAME: &str = "xterm-256color";

/// `screen::Screen`'s surface, over the core.
pub struct GhosttyScreen {
    /// Declared first so it is dropped first: the replies below are reachable from the library while this
    /// handle lives, and a freed collector behind a live terminal would be a dangling callback.
    terminal: Terminal,
    reported: Box<Reported>,
    cols: u16,
    rows: u16,
}

impl GhosttyScreen {
    /// A terminal of `cols` by `rows`, **configured to match the wrapper it is compared against**.
    ///
    /// Three settings, and none is a preference: each is a default the two implementations do not share,
    /// so leaving any of them would make the comparison report a configuration mismatch as a parser
    /// difference.
    ///
    /// - **Scrollback lines.** The core keeps *no limit* by default; this broker declares `HISTORY_LINES`
    ///   and that number is a memory ceiling per session rather than a taste.
    /// - **Scrollback bytes.** The core has a byte ceiling that `alacritty_terminal` has no equivalent
    ///   for. Left unlimited, so the line limit is the only thing that bounds history and the two
    ///   implementations are bounded by the same rule.
    /// - **The terminfo name**, as above.
    pub fn new(cols: u16, rows: u16) -> Result<Self, Failed> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let terminal = Terminal::new(cols, rows)?;

        let lines: usize = HISTORY_LINES;
        terminal.set_usize(sys::TerminalOption::SCROLLBACK_MAX_LINES, lines, "SCROLLBACK_MAX_LINES")?;
        // NULL removes the limit, which the option's own documentation says
        terminal.set_null(sys::TerminalOption::SCROLLBACK_MAX_BYTES, "SCROLLBACK_MAX_BYTES")?;
        terminal.set_str(sys::TerminalOption::TERMINFO_NAME, TERMINFO_NAME, "TERMINFO_NAME")?;

        let mut reported = Box::new(Reported::default());
        // SAFETY: the box outlives the handle, which `Self`'s field order guarantees
        unsafe { terminal.route_reports(&raw mut *reported) }?;

        Ok(Self { terminal, reported, cols, rows })
    }

    /// Feeds bytes as they arrived, chunk boundaries included.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), Failed> {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.terminal.resize(self.cols, self.rows)
    }

    /// What the shell has said about itself, from the core's own events rather than a second scan.
    pub fn facts(&self) -> &Facts {
        self.reported.facts()
    }

    /// What the program asked for and must be told, taken rather than read: each reply is written once.
    pub fn take_replies(&mut self) -> Vec<String> {
        self.reported.take_replies()
    }

    /// The viewport, with up to `history` lines above it.
    ///
    /// **Addressed in screen coordinates**, where row zero is the oldest line the terminal still holds, so
    /// the history and the viewport are one range rather than two reads that could disagree about where
    /// they meet. An alternate screen has no scrollback, so a history request over one answers the
    /// viewport alone — which is the terminal's behaviour and matches the wrapper beside it.
    pub fn snapshot(&self, history: u32) -> Result<Snapshot, Failed> {
        let scrollback = self.terminal.scrollback_rows()?;
        let above = history.min(scrollback);
        let from = scrollback - above;

        let mut lines = Vec::with_capacity((above + u32::from(self.rows)) as usize);
        for row in from..scrollback + u32::from(self.rows) {
            lines.push(self.row_text(row)?);
        }

        Ok(Snapshot {
            lines,
            cursor_row: self.terminal.cursor_row()?,
            cursor_column: self.terminal.cursor_column()?,
            alt_screen: self.terminal.alt_screen()?,
        })
    }

    /// One row's text, by the same rule the wrapper uses.
    ///
    /// A wide character occupies two cells and the second carries no codepoint of its own, so both spacer
    /// kinds are skipped rather than read as blanks; trailing spaces come off because a terminal row is
    /// space-padded to its width and that padding is the grid's shape rather than the program's output.
    fn row_text(&self, row: u32) -> Result<String, Failed> {
        let mut text = String::with_capacity(self.cols as usize);
        for column in 0..self.cols {
            let Some(cell) = self.terminal.cell(column, row)? else { continue };
            match self.terminal.cell_wide(cell)? {
                sys::CellWide::SPACER_TAIL | sys::CellWide::SPACER_HEAD => continue,
                _ => {}
            }
            let codepoint = self.terminal.cell_codepoint(cell)?;
            // Zero is an empty cell, and the wrapper reads one as the space a row is padded with
            text.push(char::from_u32(codepoint).filter(|found| *found != '\0').unwrap_or(' '));
        }
        while text.ends_with(' ') {
            text.pop();
        }
        Ok(text)
    }
}
