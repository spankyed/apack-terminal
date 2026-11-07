//! The terminal capability's screen: the same bytes the renderer draws, parsed here as the authority.
//!
//! **Both sides parse, and only one is right** (D9). The renderer keeps xterm.js because it owns interaction
//! — selection, reflow, IME — and nothing in the app may read that copy as a fact. This is the copy facts
//! come from, and it exists whether or not a window is alive to hold one, which is the whole gain the
//! refactor was for: a hidden session still has a screen.
//!
//! **What this is not.** It holds no agent status rules and no policy — no notion of an agent being busy or
//! idle, no rule that reads a drawn screen and decides what a program is doing. It is a grid, a cursor,
//! which screen is showing, a bounded run of lines above the viewport, and the facts a shell *reported*
//! about itself (`markers.rs`). The difference is provenance: a fact here was stated by the shell, and a
//! status rule would be this file's guess about one.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

use crate::markers::{Marker, Markers};

/// Lines of scrollback a session keeps.
///
/// **Declared rather than derived, and it is a ceiling on memory per session**: 1,000 lines of 80 columns is
/// about 80k cells, and a cell is wider than a byte. The renderer's own scrollback is 10,000 lines and is
/// not this number's business — that copy is a bounded replica serving one window's scrollbar, while this one
/// serves a reattach and a bounded read, and the two have no reason to agree.
pub const HISTORY_LINES: usize = 1_000;

/// What the terminal wants said back to the program, collected rather than discarded.
///
/// **A terminal answers questions, and a session that answers none does not start on Windows.** ConPTY's
/// first act is a cursor-position report request (`ESC [ 6 n`) and it waits for the reply before
/// initialising; a POSIX pty asks nothing, so this was invisible until the suite first ran on Windows and
/// every session sat there having emitted exactly those four bytes.
///
/// `alacritty_terminal` already computes these replies and hands them over as `Event::PtyWrite` — the
/// listener this replaced was `VoidListener`, which drops them. **It has to be the broker that answers**,
/// not the window: a session nobody is drawing still has to run, which is the whole of why the broker owns
/// one, and the renderer's own parse is a replica that must not be in the loop for a program's control
/// queries.
#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<String>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && let Ok(mut replies) = self.0.lock()
        {
            replies.push(text);
        }
    }
}

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

/// How many matches a search answers.
///
/// **A ceiling, because the question a caller asks is "where is this" and not "every place it occurs"**: a
/// one-character search over a full history would otherwise answer tens of thousands of positions, each
/// carrying its line. A caller that hits the cap has asked too broad a question and the first hits are the
/// ones it wanted.
pub const SEARCH_MATCHES: usize = 100;

/// What a shell has reported about itself, accumulated from the markers it emitted.
///
/// **Every field is a claim the shell made, not a measurement of the machine**, which is why there is no
/// `pid` or `user` here: those the broker knows for itself. A program in the pty can emit any of these
/// sequences, so what this carries is what was *said* — the app decides what a claimed path is good for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// The working directory the shell last reported.
    pub cwd: Option<String>,
    /// The command line the shell last said it was about to run.
    pub command: Option<String>,
    /// Between a command starting and finishing, so a caller can tell a busy session from one at a prompt
    /// **without reading the screen**, which is the whole point of the markers.
    pub running: bool,
    /// The status the last finished command reported. `None` when none has finished, or when the shell
    /// reported a finish without one — a shell that says nothing about how a command ended is not saying
    /// it succeeded.
    pub exit_status: Option<i32>,
}

/// Where a search found what it was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Into the lines the same request would answer, oldest first.
    pub line: u32,
    /// Where in that line the match starts, in characters.
    pub column: u32,
    /// The whole line, so a caller matching a prompt or an error sees its context without a second read.
    pub text: String,
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
    term: Term<Replies>,
    parser: Processor,
    size: Size,
    replies: Replies,
    /// Scans the markers out of the same bytes, because the grid drops the sequences they travel in
    markers: Markers,
    facts: Facts,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let size = Size {
            columns: cols.max(1) as usize,
            screen_lines: rows.max(1) as usize,
            history: HISTORY_LINES,
        };
        let replies = Replies::default();
        Self {
            term: Term::new(
                Config {
                    scrolling_history: HISTORY_LINES,
                    ..Config::default()
                },
                &size,
                replies.clone(),
            ),
            parser: Processor::new(),
            size,
            replies,
            markers: Markers::default(),
            facts: Facts::default(),
        }
    }

    /// Feeds bytes as they arrived.
    ///
    /// **Chunk boundaries are the parser's problem and it carries state across them**, which is why
    /// `tests/parser.rs` splits the same stream at every boundary and demands the same screen: a wrapper
    /// that buffered a chunk, or reset anything per call, would pass every test that feeds a whole stream.
    pub fn feed(&mut self, bytes: &[u8]) {
        // The markers first, from the untouched stream: the grid is handed the same bytes and ignores the
        // sequences they travel in, so neither pass can take anything from the other
        for marker in self.markers.feed(bytes) {
            self.facts.apply(marker);
        }
        self.parser.advance(&mut self.term, bytes);
    }

    /// What the shell has said about itself.
    pub fn facts(&self) -> &Facts {
        &self.facts
    }

    /// What the program asked for and must be told, taken rather than read: each reply is written once.
    pub fn take_replies(&mut self) -> Vec<String> {
        self.replies
            .0
            .lock()
            .map(|mut replies| std::mem::take(&mut *replies))
            .unwrap_or_default()
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

impl Facts {
    /// Takes one marker's word for it.
    fn apply(&mut self, marker: Marker) {
        match marker {
            // A prompt being drawn means whatever ran has finished, whether or not a `D` said so
            Marker::PromptStart => self.running = false,
            Marker::PromptEnd => {}
            Marker::CommandStart => self.running = true,
            Marker::CommandDone(status) => {
                self.running = false;
                self.exit_status = status;
            }
            Marker::CommandLine(line) => self.command = Some(line),
            Marker::Cwd(path) => self.cwd = Some(path),
        }
    }
}

impl Screen {
    /// Where `text` appears, over the same lines a `snapshot(history)` would answer.
    ///
    /// **Bounded twice**: by the history asked for, and by `SEARCH_MATCHES`. A search is a read of what is
    /// already parsed, so it costs no more than the snapshot it searches.
    pub fn search(&self, text: &str, history: u32) -> Vec<Match> {
        if text.is_empty() {
            return Vec::new();
        }
        let snapshot = self.snapshot(history);
        let mut found = Vec::new();
        for (index, line) in snapshot.lines.iter().enumerate() {
            let mut from = 0;
            while let Some(at) = line[from..].find(text) {
                let byte = from + at;
                found.push(Match {
                    line: index as u32,
                    // Characters rather than bytes: a caller counting columns on a screen counts what was
                    // drawn, and a multi-byte character is one column of it
                    column: line[..byte].chars().count() as u32,
                    text: line.clone(),
                });
                if found.len() >= SEARCH_MATCHES {
                    return found;
                }
                from = byte + text.len();
            }
        }
        found
    }
}

/// One row as text.
///
/// **A wide character occupies two cells and the second holds a space**, which the first snapshot of this
/// showed as `日 本 語`. Drawn that is right — the glyph is two columns wide — but read as text it is wrong,
/// and reading as text is what this is for: a bounded screen read an agent matches against must see what was
/// written. So the spacers are skipped, and the trailing padding a row carries to its full width goes with
/// them, being the grid's shape rather than the program's output.
fn row_text(term: &Term<Replies>, line: Line, columns: usize) -> String {
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
