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

use crate::markers::{Marker, Markers};
use crate::vt::{Failed, Reported, Terminal};

/// Lines of scrollback a session keeps.
///
/// **Declared rather than derived, and it is a ceiling on memory per session**: 1,000 lines of 80 columns is
/// about 80k cells, and a cell is wider than a byte. The renderer's own scrollback is 10,000 lines and is
/// not this number's business — that copy is a bounded replica serving one window's scrollbar, while this one
/// serves a reattach and a bounded read, and the two have no reason to agree.
pub const HISTORY_LINES: usize = 1_000;

/// What the core is *asked* for, which is deliberately more than is promised.
///
/// **The core's line limit is a page-granular heuristic and its floor is the ask minus one page.** It
/// prunes whole historical pages and never the page holding the active area
/// (`PageList.Limits.enforce`), so the rows retained oscillate between roughly the ask and the ask less a
/// page. Measured at 80x24: asking 1,000 held between 441 and 793 rows, which would answer a reattach
/// with less than half what the window was told it could scroll back; asking 2,000 held between 1,625 and
/// 1,977.
///
/// So the promise is kept by asking for twice it and reading back no more than it. A page holds at most
/// about 750 rows at any width a terminal is plausibly opened at — `page.std_capacity` is 215 rows at 215
/// columns and grows as columns shrink — so a margin of `HISTORY_LINES` clears one page comfortably at
/// every width. `tests/screen.rs` asserts both halves rather than trusting this arithmetic.
const SCROLLBACK_ASKED: usize = HISTORY_LINES * 2;

/// The terminal advertised to a program, which has to be what the pty says.
pub const TERMINFO_NAME: &str = "xterm-256color";

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
    /// How many commands the shell has reported starting and then ending.
    ///
    /// **A count rather than a flag, because a caller asks after the fact.** A command that starts and ends
    /// between two reads leaves `running` false at both, so nothing else here says one ran; this has moved.
    /// It moves once per command, whichever marker ended it, and not for a prompt drawn with nothing
    /// running. **It says that commands ended, not how many**: the two marker families are read as two
    /// passes over each chunk, so a shell emitting both has each command read twice, and it wraps. It is
    /// compared for a difference and never for an order or an amount.
    pub finished: u32,
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
    /// Declared first so it is dropped first: the collector below is reachable from the library while this
    /// handle lives, and freeing it behind a live terminal would leave a dangling callback.
    terminal: Terminal,
    /// What the library reports — the replies it makes for a program, and the facts its own callbacks read.
    /// Boxed because its address is what the library was handed.
    reported: Box<Reported>,
    /// **The sequences the core does not read.** It reports OSC 133 and the working directory; this scans
    /// OSC 633, which is VS Code's shell integration and which no version of the core reads. Measured in
    /// Phase 0a: the two families are disjoint, so the union is strictly more than either and neither
    /// double-reports a fact the other already has.
    markers: Markers,
    cols: u16,
    rows: u16,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::build(cols, rows).expect("libghostty-vt could not make a terminal")
    }

    /// **Three settings, each a default the core and this broker do not share.**
    ///
    /// The core keeps *no* scrollback limit of its own, where `HISTORY_LINES` is a ceiling on memory per
    /// session; its byte ceiling has no counterpart here, so the line limit is left as the only rule
    /// bounding history; and the advertised terminal has to be the one the pty says, because a program
    /// decides what to emit from `TERM` and answers a query differently when told something else.
    fn build(cols: u16, rows: u16) -> Result<Self, Failed> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let terminal = Terminal::new(cols, rows)?;
        terminal.set_scrollback_lines(SCROLLBACK_ASKED)?;
        terminal.set_scrollback_unbounded_bytes()?;
        terminal.set_terminfo_name(TERMINFO_NAME)?;

        let mut reported = Box::new(Reported::default());
        // SAFETY: the box outlives the handle, which this struct's field order guarantees
        unsafe { terminal.route_reports(&raw mut *reported) }?;

        Ok(Self { terminal, reported, markers: Markers::default(), cols, rows })
    }

    /// Feeds bytes as they arrived.
    ///
    /// **Chunk boundaries are the parser's problem and it carries state across them**, which is why
    /// `tests/parser.rs` splits the same stream at every boundary and demands the same screen: a wrapper
    /// that buffered a chunk, or reset anything per call, would pass every test that feeds a whole stream.
    ///
    /// The core first, because its callbacks run inside the write; then the 633 scan over the same
    /// untouched bytes, so neither pass can take anything from the other.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
        for marker in self.markers.feed(bytes) {
            self.reported.apply(marker);
        }
    }

    /// What the shell has said about itself.
    pub fn facts(&self) -> &Facts {
        self.reported.facts()
    }

    /// What the program asked for and must be told, taken rather than read: each reply is written once.
    pub fn take_replies(&mut self) -> Vec<String> {
        self.reported.take_replies()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let _ = self.terminal.resize(self.cols, self.rows);
    }

    /// The viewport, with up to `history` lines above it.
    ///
    /// **Addressed in screen coordinates**, where row zero is the oldest line still held, so the history
    /// and the viewport are one range rather than two reads that could disagree about where they meet. An
    /// alternate screen has no scrollback, so a history request over one answers the viewport alone, which
    /// is the terminal's behaviour rather than a limitation of this read.
    pub fn snapshot(&self, history: u32) -> Snapshot {
        self.read(history).unwrap_or_else(|_| Snapshot {
            lines: vec![String::new(); self.rows as usize],
            cursor_row: 0,
            cursor_column: 0,
            alt_screen: false,
        })
    }

    fn read(&self, history: u32) -> Result<Snapshot, Failed> {
        let scrollback = self.terminal.scrollback_rows()?;
        // **Never more than was promised.** The core holds more than `HISTORY_LINES` most of the time, by
        // the page arithmetic above; answering with it would show a window lines its own scrollbar was
        // never sized for, and the broker's handshake is the one number both sides agree on.
        let held = scrollback.min(u32::try_from(HISTORY_LINES).unwrap_or(u32::MAX));
        let above = history.min(held);
        let mut lines = Vec::with_capacity((above + u32::from(self.rows)) as usize);
        for row in scrollback - above..scrollback + u32::from(self.rows) {
            lines.push(self.row_text(row)?);
        }
        Ok(Snapshot {
            lines,
            cursor_row: self.terminal.cursor_row()?,
            cursor_column: self.terminal.cursor_column()?,
            alt_screen: self.terminal.alt_screen()?,
        })
    }

    /// One row as text.
    ///
    /// **A wide character occupies two cells and the second holds no codepoint of its own**, which read as
    /// text gives `日 本 語`. Drawn that is right — the glyph is two columns wide — but reading as text is
    /// what this is for: a bounded screen read an agent matches against must see what was written. So both
    /// spacer kinds are skipped, and the trailing padding a row carries to its full width goes with them,
    /// being the grid's shape rather than the program's output.
    fn row_text(&self, row: u32) -> Result<String, Failed> {
        let mut text = String::with_capacity(self.cols as usize);
        for column in 0..self.cols {
            let Some(cell) = self.terminal.cell(column, row)? else { continue };
            if self.terminal.cell_is_spacer(cell)? {
                continue;
            }
            let codepoint = self.terminal.cell_codepoint(cell)?;
            text.push(char::from_u32(codepoint).filter(|found| *found != '\0').unwrap_or(' '));
        }
        while text.ends_with(' ') {
            text.pop();
        }
        Ok(text)
    }
}

impl Facts {
    /// Takes one marker's word for it.
    pub(crate) fn apply(&mut self, marker: Marker) {
        match marker {
            // A prompt being drawn means whatever ran has finished, whether or not a `D` said so
            Marker::PromptStart => self.ended(),
            Marker::PromptEnd => {}
            Marker::CommandStart => self.running = true,
            Marker::CommandDone(status) => {
                self.ended();
                self.exit_status = status;
            }
            Marker::CommandLine(line) => self.command = Some(line),
            Marker::Cwd(path) => self.cwd = Some(path),
        }
    }

    /// Whatever was running has ended.
    ///
    /// **Counted here and nowhere else**, so a shell that reports an end twice — a `D` and then the
    /// prompt — has ended one command and not two.
    pub(crate) fn ended(&mut self) {
        if std::mem::take(&mut self.running) {
            self.finished = self.finished.wrapping_add(1);
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
