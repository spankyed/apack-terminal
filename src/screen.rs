//! The terminal capability's screen: the same bytes the window draws, parsed here as the authority.
//!
//! **Both sides parse, and only one is right.** The window keeps an emulator of its own because it owns
//! interaction — selection, scrolling, text input — and nothing in the app may read that copy as a fact.
//! This is the copy facts come from, and it exists whether or not a window is alive to hold one: a hidden
//! session still has a screen, and a program that asks the terminal a question is answered from here.
//!
//! **What this is not.** It holds no agent status rules and no policy — no notion of an agent being busy or
//! idle, no rule that reads a drawn screen and decides what a program is doing. It is a grid, a cursor,
//! which screen is showing, a bounded run of lines above the viewport, and the facts a shell *reported*
//! about itself (`markers.rs`). The difference is provenance: a fact here was stated by the shell, and a
//! status rule would be this file's guess about one.

use std::sync::mpsc::Sender;

use crate::view::publish::{Publication, ViewId};
use crate::view::wire::{Block, Position, Span};
use crate::vt::input::{self, Encoders, Key, Pasted, Pointer};
use crate::vt::reads;
use crate::vt::rows::{RowRead, Space};
use crate::vt::{Effect, Failed, Reported, Terminal};

/// Lines of scrollback a session keeps.
///
/// **Declared rather than derived, and it is a ceiling on memory per session**: 1,000 lines of 80 columns is
/// about 80k cells, and a cell is wider than a byte. It is also the number a client is told in the
/// handshake, so a window sizes its own scrollback from this rather than from a constant of its own.
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
    /// The row the matching line starts on, into the lines the same request's `snapshot` would answer.
    pub line: u32,
    /// Where in that line the match starts, in characters, counted from the line's start — which is past
    /// the grid's width for a match on a row the line wrapped onto.
    pub column: u32,
    /// The whole line, wrapped rows joined, so a caller matching a prompt or an error sees its context
    /// without a second read.
    pub text: String,
}

/// What a bounded read answers: lines of text, where the cursor is, and which screen is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The requested history, then the viewport, oldest first. Trailing blanks on a line are dropped,
    /// because a terminal row is space-padded to its width and those spaces are the grid's shape rather
    /// than the program's output.
    pub lines: Vec<String>,
    /// For each line, whether it runs on into the next: a line longer than the grid is wide is drawn over
    /// several rows, and this is what says they are one line. Always as long as `lines`.
    pub wrapped: Vec<bool>,
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
    cols: u16,
    rows: u16,
    /// One cell in pixels, as the window drawing this session measured it. Zero until one has.
    cell: (u16, u16),
    /// The core's input encoders, from the first input a view sends.
    encoders: Option<Encoders>,
    /// What this session's views have been sent, from the first one that attaches. A session nothing
    /// views keeps none, so being viewable costs a session that is not viewed nothing.
    publication: Option<Publication>,
}

/// DEC private mode 2027: a character made of several code points occupies the cells of one character.
const GRAPHEME_CLUSTERING: u16 = 2027;

/// The colours a session is drawn in, which is what it answers when a program asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Colors {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: Option<[u8; 3]>,
    /// The first entries of the palette — the sixteen a theme names. Shorter leaves the rest alone.
    pub ansi: Vec<[u8; 3]>,
}

impl Screen {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::build(cols, rows).expect("libghostty-vt could not make a terminal")
    }

    /// **Five settings, each a default the core and this broker do not share.**
    ///
    /// The core keeps *no* scrollback limit of its own, where `HISTORY_LINES` is a ceiling on memory per
    /// session; its byte ceiling has no counterpart here, so the line limit is left as the only rule
    /// bounding history; and the advertised terminal has to be the one the pty says, because a program
    /// decides what to emit from `TERM` and answers a query differently when told something else.
    ///
    /// **The last two are about the window, which is what draws the session.** An emoji built from
    /// several code points is one character there, so it has to be one here: with clustering off this
    /// screen put the cursor four columns further right after a family emoji than the window drew it, and
    /// the cursor position a program is told comes from this screen. And the window draws no images, so
    /// the image protocol is off rather than answered `OK` on behalf of a renderer that would show
    /// nothing.
    fn build(cols: u16, rows: u16) -> Result<Self, Failed> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let terminal = Terminal::new(cols, rows)?;
        terminal.set_scrollback_lines(SCROLLBACK_ASKED)?;
        terminal.set_scrollback_unbounded_bytes()?;
        terminal.set_terminfo_name(TERMINFO_NAME)?;
        terminal.set_mode(GRAPHEME_CLUSTERING, true)?;
        terminal.refuse_images()?;

        let mut reported = Box::new(Reported::default());
        // SAFETY: the box outlives the handle, which this struct's field order guarantees
        unsafe { terminal.route_reports(&raw mut *reported) }?;

        Ok(Self { terminal, reported, cols, rows, cell: (0, 0), encoders: None, publication: None })
    }

    /// Feeds bytes as they arrived.
    ///
    /// **Chunk boundaries are the parser's problem and it carries state across them**, which is why
    /// `tests/parser.rs` splits the same stream at every boundary and demands the same screen: a wrapper
    /// that buffered a chunk, or reset anything per call, would pass every test that feeds a whole stream.
    ///
    /// Everything the core reports — replies, the shell's markers, what the program asked of the world
    /// outside — is collected by callbacks that run inside this write, and taken afterwards.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
        if let Some(publication) = &mut self.publication {
            publication.touch();
        }
    }

    /// What the program did that is not on its screen, since this was last taken.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        self.reported.take_effects()
    }

    /// What the shell has said about itself.
    pub fn facts(&self) -> &Facts {
        self.reported.facts()
    }

    /// What the program asked for and must be told, taken rather than read: each reply is written once.
    pub fn take_replies(&mut self) -> Vec<String> {
        self.reported.take_replies()
    }

    /// The grid changed, and perhaps the size of a cell with it.
    ///
    /// **A cell size of zero means the caller does not know one, and the last one known stands.** Only a
    /// window measures a cell, and not every resize comes from one.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        if cell_width > 0 && cell_height > 0 {
            self.cell = (cell_width, cell_height);
        }
        let (cell_width, cell_height) = self.cell;
        let _ = self.terminal.resize(self.cols, self.rows, cell_width, cell_height);
        if let Some(publication) = &mut self.publication {
            publication.touch();
        }
        self.reported.set_size((cell_width > 0).then_some(crate::vt::Size {
            cols: self.cols,
            rows: self.rows,
            cell_width,
            cell_height,
        }));
    }

    fn encoders(&mut self) -> Result<&mut Encoders, Failed> {
        if self.encoders.is_none() {
            self.encoders = Some(Encoders::new()?);
        }
        // Just filled, so the error is unreachable; it keeps this from being a panic if that changes
        self.encoders.as_mut().ok_or(Failed { call: "ghostty_key_encoder_new", code: 0 })
    }

    /// What a key sends the program now, given the modes it has set.
    pub fn key(&mut self, key: &Key<'_>) -> Result<Vec<u8>, Failed> {
        self.encoders()?;
        match &mut self.encoders {
            Some(encoders) => encoders.key(&self.terminal, key),
            None => Ok(Vec::new()),
        }
    }

    /// Whether the option key is alt for this session's keys, which on a Mac it is not unless chosen.
    pub fn set_option_as_alt(&mut self, as_alt: bool) -> Result<(), Failed> {
        self.encoders()?.set_option_as_alt(as_alt);
        Ok(())
    }

    /// What the pointer sends the program now, which is nothing unless it asked for the mouse.
    pub fn pointer(&mut self, pointer: &Pointer) -> Result<Vec<u8>, Failed> {
        self.encoders()?;
        match &mut self.encoders {
            Some(encoders) => encoders.pointer(&self.terminal, pointer),
            None => Ok(Vec::new()),
        }
    }

    /// Pasted text as the program should receive it, or a refusal when it could run a command.
    pub fn paste(&self, text: &str, allow_unsafe: bool) -> Result<Pasted, Failed> {
        input::paste(&self.terminal, text, allow_unsafe)
    }

    /// What the program is told about its terminal gaining or losing focus.
    pub fn focus(&self, gained: bool) -> Result<Vec<u8>, Failed> {
        input::focus(&self.terminal, gained)
    }

    /// Starts sending a view this session's frames through `sink`, beginning with the whole screen.
    pub fn attach_view(&mut self, id: ViewId, sink: Sender<Vec<u8>>) -> Result<(), Failed> {
        if self.publication.is_none() {
            self.publication = Some(Publication::new(&self.terminal)?);
        }
        match &mut self.publication {
            Some(publication) => publication.attach(&self.terminal, id, sink),
            None => Ok(()),
        }
    }

    pub fn detach_view(&mut self, id: ViewId) {
        if let Some(publication) = &mut self.publication {
            publication.detach(id);
        }
    }

    /// Every view a connection had, when the connection goes.
    pub fn detach_connection(&mut self, connection: u64) {
        if let Some(publication) = &mut self.publication {
            publication.detach_connection(connection);
        }
    }

    /// A view drew the frame with this sequence.
    pub fn acknowledge(&mut self, id: ViewId, sequence: u64) -> Result<(), Failed> {
        match &mut self.publication {
            Some(publication) => publication.acknowledge(&self.terminal, id, sequence),
            None => Ok(()),
        }
    }

    /// Sends each view that is waiting for one the frame it is owed. Called where bytes land and where a
    /// view acknowledges, which between them are every moment a frame can become due.
    pub fn publish(&mut self) {
        if let Some(publication) = &mut self.publication {
            let _ = publication.pump(&self.terminal);
        }
    }

    /// Lines by number, for a view that does not hold them, with the epoch the numbers belong to.
    pub fn fetch(&mut self, first: u64, count: u32) -> Result<(u32, Block), Failed> {
        if self.publication.is_none() {
            self.publication = Some(Publication::new(&self.terminal)?);
        }
        match &mut self.publication {
            Some(publication) => {
                let block = publication.fetch(&self.terminal, first, count)?;
                Ok((publication.epoch(), block))
            }
            None => Ok((0, Block::default())),
        }
    }

    /// The publication brought up to the terminal, which is what turns a line number into a row of it.
    fn positioned(&mut self) -> Result<&Publication, Failed> {
        if self.publication.is_none() {
            self.publication = Some(Publication::new(&self.terminal)?);
        }
        match &mut self.publication {
            Some(publication) => {
                publication.advance(&self.terminal)?;
                Ok(publication)
            }
            None => Err(Failed { call: "ghostty_terminal_grid_ref_track", code: 0 }),
        }
    }

    fn span_of(publication: &Publication, cells: (reads::Cell, reads::Cell)) -> Span {
        let position = |cell: reads::Cell| Position { line: publication.line_of(cell.1), column: cell.0 };
        Span { start: position(cells.0), end: position(cells.1) }
    }

    /// The word at a cell, with the epoch its line numbers are in. `None` where there is no word, or no
    /// such line any more.
    pub fn word_at(&mut self, at: Position) -> Result<(u32, Option<Span>), Failed> {
        let publication = self.positioned()?;
        let epoch = publication.epoch();
        let Some(row) = publication.screen_row(at.line) else { return Ok((epoch, None)) };
        let found = self.terminal.word_at((at.column, row))?;
        let publication = self.positioned()?;
        Ok((epoch, found.map(|cells| Self::span_of(publication, cells))))
    }

    /// The whole line a cell is on, across the rows it wrapped onto.
    pub fn line_at(&mut self, at: Position) -> Result<(u32, Option<Span>), Failed> {
        let publication = self.positioned()?;
        let epoch = publication.epoch();
        let Some(row) = publication.screen_row(at.line) else { return Ok((epoch, None)) };
        let found = self.terminal.line_at((at.column, row))?;
        let publication = self.positioned()?;
        Ok((epoch, found.map(|cells| Self::span_of(publication, cells))))
    }

    /// The text of a span as a program wrote it. Empty when either end is a line no longer held.
    pub fn text_of(&mut self, span: Span) -> Result<String, Failed> {
        let publication = self.positioned()?;
        let (Some(first), Some(last)) =
            (publication.screen_row(span.start.line), publication.screen_row(span.end.line))
        else {
            return Ok(String::new());
        };
        self.terminal.text_between((span.start.column, first), (span.end.column, last))
    }

    /// Every place `needle` appears, newest first, with the epoch the line numbers are in.
    pub fn find(&mut self, needle: &str) -> Result<(u32, Vec<Span>), Failed> {
        let found = self.terminal.find(needle)?;
        let publication = self.positioned()?;
        Ok((publication.epoch(), found.into_iter().map(|cells| Self::span_of(publication, cells)).collect()))
    }

    /// The epoch line numbers are in and the line the screen's first row is, as of now.
    pub fn view_position(&mut self) -> Result<(u32, u64), Failed> {
        if self.publication.is_none() {
            self.publication = Some(Publication::new(&self.terminal)?);
        }
        match &mut self.publication {
            Some(publication) => {
                publication.advance(&self.terminal)?;
                Ok((publication.epoch(), publication.first_active()))
            }
            None => Ok((0, 0)),
        }
    }

    /// Tells every view the session is over, after the screen as it ended.
    pub fn end_views(&mut self) {
        if let Some(mut publication) = self.publication.take() {
            publication.end(&self.terminal);
        }
    }

    /// A row as the grid holds it, in the core's own coordinates.
    pub fn row(&self, space: Space, row: u32) -> Result<Option<RowRead>, Failed> {
        self.terminal.read_row(space, row)
    }

    /// How many rows of history the terminal is holding.
    pub fn scrollback_rows(&self) -> u32 {
        self.terminal.scrollback_rows().unwrap_or(0)
    }

    /// The grid's width, which bounds how many rows a run of bytes can have added.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// What this session is drawn in, so a program asking for a colour hears the one on the screen.
    pub fn set_colors(&mut self, colors: &Colors) {
        let _ = self.terminal.set_colors(colors.foreground, colors.background, colors.cursor, &colors.ansi);
    }

    /// The viewport, with up to `history` lines above it.
    ///
    /// **Addressed in screen coordinates**, where row zero is the oldest line still held, so the history
    /// and the viewport are one range rather than two reads that could disagree about where they meet. An
    /// alternate screen has no scrollback, so a history request over one answers the viewport alone, which
    /// is the terminal's behaviour rather than a limitation of this read.
    pub fn snapshot(&self, history: u32) -> Snapshot {
        self.read(history, false).unwrap_or_else(|_| Snapshot {
            lines: vec![String::new(); self.rows as usize],
            wrapped: vec![false; self.rows as usize],
            cursor_row: 0,
            cursor_column: 0,
            alt_screen: false,
        })
    }

    /// `whole_lines` reaches further up than `history` when the first row asked for is the middle of a
    /// wrapped line, so that line is read from its start.
    fn read(&self, history: u32, whole_lines: bool) -> Result<Snapshot, Failed> {
        let scrollback = self.terminal.scrollback_rows()?;
        // **Never more than was promised.** The core holds more than `HISTORY_LINES` most of the time, by
        // the page arithmetic above; answering with it would show a window lines its own scrollbar was
        // never sized for, and the broker's handshake is the one number both sides agree on.
        let held = scrollback.min(u32::try_from(HISTORY_LINES).unwrap_or(u32::MAX));
        let mut above = history.min(held);
        while whole_lines && above < held && self.terminal.row_wraps(scrollback - above - 1)? {
            above += 1;
        }
        let mut lines = Vec::new();
        let mut wrapped = Vec::new();
        for row in scrollback - above..scrollback + u32::from(self.rows) {
            let wraps = self.terminal.row_wraps(row)?;
            lines.push(self.row_text(row, wraps)?);
            wrapped.push(wraps);
        }
        Ok(Snapshot {
            lines,
            wrapped,
            cursor_row: self.terminal.cursor_row()?,
            cursor_column: self.terminal.cursor_column()?,
            alt_screen: self.terminal.alt_screen()?,
        })
    }

    /// One row as text.
    ///
    /// **A wide character occupies two cells and the second holds no character of its own**, which read
    /// as text gives `日 本 語`. Drawn that is right — the glyph is two columns wide — but reading as text is
    /// what this is for: a bounded screen read an agent matches against must see what was written. So both
    /// spacer kinds are skipped, and the trailing padding a row carries to its full width goes with them,
    /// being the grid's shape rather than the program's output.
    ///
    /// **A row that wraps keeps its trailing spaces**, because there they are the program's: the row is
    /// full, the line goes on below, and dropping them would join two words that had a space between.
    fn row_text(&self, row: u32, wraps: bool) -> Result<String, Failed> {
        let mut text = String::with_capacity(self.cols as usize);
        for column in 0..self.cols {
            self.terminal.push_cell_text(column, row, &mut text)?;
        }
        while !wraps && text.ends_with(' ') {
            text.pop();
        }
        Ok(text)
    }
}

impl Facts {
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
        Self::matches(text, &self.snapshot(history))
    }

    /// Whether `text` is in the viewport or the `recent` rows above it, reading a wrapped line whole.
    ///
    /// **For a wait, which asks after every read and must not pay for the whole history each time.** A
    /// read can only have changed the viewport and whatever it scrolled off, so the caller passes how many
    /// rows that could be and this looks no further. It reaches up to the start of a wrapped line because
    /// a match may begin above the rows that changed.
    pub fn shows_recently(&self, text: &str, recent: u32) -> bool {
        if text.is_empty() {
            return false;
        }
        self.read(recent, true).is_ok_and(|snapshot| !Self::matches(text, &snapshot).is_empty())
    }

    /// **Matched against lines as they were written, not rows as they were drawn.** Rows a line wrapped
    /// onto are joined before the search, so a path that reached the right edge is found whole. `line` is
    /// the row the line starts on, `column` counts from there, and `text` is the whole line.
    fn matches(text: &str, snapshot: &Snapshot) -> Vec<Match> {
        let mut found = Vec::new();
        let mut row = 0;
        while row < snapshot.lines.len() {
            let start = row;
            let mut line = snapshot.lines[row].clone();
            // A row that runs on is joined to the one below, for as long as there is one below
            while snapshot.wrapped.get(row).copied().unwrap_or(false)
                && let Some(below) = snapshot.lines.get(row + 1)
            {
                line.push_str(below);
                row += 1;
            }
            row += 1;
            let mut from = 0;
            while let Some(at) = line[from..].find(text) {
                let byte = from + at;
                found.push(Match {
                    line: start as u32,
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
