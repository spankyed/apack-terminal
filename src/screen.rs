//! The terminal capability's screen: the same bytes the renderer draws, parsed here as the authority.
//!
//! **Both sides parse, and only one is right** (D9). The renderer keeps xterm.js because it owns interaction
//! — selection, reflow, IME — and nothing in the app may read that copy as a fact. This is the copy facts
//! come from, and it exists whether or not a window is alive to hold one, which is the whole gain the
//! refactor was for: a hidden session still has a screen.
//!
//! **What this is not.** It holds no agent status rules, no command semantics and no policy; those are
//! Phase A's, over the OSC markers this parses. It is a grid, a cursor, which screen is showing, and a
//! bounded run of lines above the viewport.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

/// Lines of scrollback a session keeps.
///
/// **Declared rather than derived, and it is a ceiling on memory per session**: 1,000 lines of 80 columns is
/// about 80k cells, and a cell is wider than a byte. The renderer's own scrollback is 10,000 lines and is
/// not this number's business — that copy is a bounded replica serving one window's scrollbar, while this one
/// serves a reattach and a bounded read, and the two have no reason to agree.
pub const HISTORY_LINES: usize = 1_000;

/// A screen's dimensions, which is all `Term` needs of a window.
#[derive(Clone, Copy)]
struct Size {
    columns: usize,
    screen_lines: usize,
    history: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines + self.history
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// What a bounded read answers: lines of text, where the cursor is, and which screen is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The requested history, then the viewport, oldest first. Trailing blanks on a line are dropped,
    /// because a terminal row is space-padded to its width and those spaces are the grid's shape rather
    /// than the program's output.
    pub lines: Vec<String>,
    /// Within the viewport, so `0` is its first line however much history came with it.
    pub cursor_row: u16,
    pub cursor_column: u16,
    /// **An alternate-screen app is showing** — `vim`, `less`, `htop`, a TUI agent. It has no scrollback of
    /// its own, so a history request over one answers the viewport alone, which is the terminal's behaviour
    /// and not a limitation of this read.
    pub alt_screen: bool,
}

/// One session's VT state.
pub struct Screen {
    term: Term<VoidListener>,
    parser: Processor,
    size: Size,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = Size {
            columns: cols.max(1) as usize,
            screen_lines: rows.max(1) as usize,
            history: HISTORY_LINES,
        };
        Self {
            term: Term::new(
                Config {
                    scrolling_history: HISTORY_LINES,
                    ..Config::default()
                },
                &size,
                VoidListener,
            ),
            parser: Processor::new(),
            size,
        }
    }

    /// Feeds bytes as they arrived.
    ///
    /// **Chunk boundaries are the parser's problem and it carries state across them**, which is why
    /// `tests/parser.rs` splits the same stream at every boundary and demands the same screen: a wrapper
    /// that buffered a chunk, or reset anything per call, would pass every test that feeds a whole stream.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.size.columns = cols.max(1) as usize;
        self.size.screen_lines = rows.max(1) as usize;
        self.term.resize(self.size);
    }

    /// The viewport, with up to `history` lines above it.
    pub fn snapshot(&self, history: u32) -> Snapshot {
        let grid = self.term.grid();
        let above = (history as usize).min(grid.history_size()) as i32;
        let lines = (-above..self.size.screen_lines as i32)
            .map(|line| row_text(&self.term, Line(line), self.size.columns))
            .collect();
        let cursor = grid.cursor.point;
        Snapshot {
            lines,
            cursor_row: cursor.line.0.max(0) as u16,
            cursor_column: cursor.column.0 as u16,
            alt_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
        }
    }
}

/// One row as text.
///
/// **A wide character occupies two cells and the second holds a space**, which the first snapshot of this
/// showed as `日 本 語`. Drawn that is right — the glyph is two columns wide — but read as text it is wrong,
/// and reading as text is what this is for: a bounded screen read an agent matches against must see what was
/// written. So the spacers are skipped, and the trailing padding a row carries to its full width goes with
/// them, being the grid's shape rather than the program's output.
fn row_text(term: &Term<VoidListener>, line: Line, columns: usize) -> String {
    let row = &term.grid()[line];
    let spacers = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
    let mut text: String = (0..columns)
        .map(|column| &row[Column(column)])
        .filter(|cell| !cell.flags.intersects(spacers))
        .map(|cell| cell.c)
        .collect();
    while text.ends_with(' ') {
        text.pop();
    }
    text
}
