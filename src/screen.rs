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

use std::collections::HashMap;
use std::path::Path;
use std::ptr::NonNull;
use std::time::{Duration, SystemTime};

use crate::commands::Outcome;
use crate::holds::HoldLimit;
use crate::markers::MarksKey;
use crate::recording::{self, Entry, Recorder, Timed};
use crate::view::publish::{Outlet, Publication, ViewId};
use crate::view::wire::{MAX_MESSAGE_BYTES, Position, Rows, Span};
use crate::vt::images::{ImageRead, PlacementRead};
use crate::vt::input::{self, Encoders, Key, Pasted, Pointer};
use crate::vt::reads;
use crate::vt::rows::{RowRead, Space};
use crate::vt::{Effect, Failed, Placed, Reported, Terminal};

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

/// How far back a view can scroll: the lines of history a session keeps for whatever draws it.
///
/// **A hundred thousand, which is what a long build or a day of work in one shell is, and affordable
/// because of what it costs when nobody is looking.** Measured on 2026-11-10 with a release build: a
/// hundred thousand 80-column lines are 66 MiB while they are being written and 1.3 MiB once compressed,
/// which takes 9 ms and which `housekeeping` does to a session that has gone quiet; the oldest row is
/// still read in 60 µs afterwards. That text repeated, so what a real session compresses to is more —
/// the claim is the order of magnitude, not the figure.
///
/// `HISTORY_LINES` is a different number on purpose: it is what a client asking for a screen as text is
/// promised, and it does not grow because a view can scroll further.
pub const VIEW_HISTORY_LINES: usize = 100_000;

/// How many lines of history a recording's keyframe carries above the screen (`recording`).
pub const KEYFRAME_HISTORY_LINES: u32 = 1_000;

/// The most memory one session's history may hold, whatever its line limit allows.
///
/// **Lines bound a narrow terminal and bytes bound a wide one.** A row costs what its width does, so a
/// hundred thousand rows of a very wide grid is several times the memory of the same rows at 80 columns —
/// and a program can fill them as fast as it can write. This is what such a session holds at most while
/// it is being written to; quiet, it is compressed like any other.
const HISTORY_BYTES: usize = 256 << 20;

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
/// **Every field is something that was said, not a measurement of the machine**, which is why there is
/// no `pid` or `user` here: those the broker knows for itself. Who could have said it depends on the
/// session (`Vouching`). In one spawned without a key a program in the pty can emit any of these
/// sequences, so all of it is a claim. In one spawned with a key, `command`, `running`, `outcome` and
/// `finished` come from marks carrying the key and from nothing else; `cwd` is a claim in both, being
/// what a tab is titled with before any command has run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// The working directory the shell last reported.
    pub cwd: Option<String>,
    /// The command line the shell last said it was about to run.
    pub command: Option<String>,
    /// Between a command starting and finishing, so a caller can tell a busy session from one at a prompt
    /// **without reading the screen**, which is the whole point of the markers.
    pub running: bool,
    /// How the last command to end ended, as far as anything said. `None` when none has ended; never
    /// `Running`.
    pub outcome: Option<Outcome>,
    /// How many commands the shell has reported starting and then ending.
    ///
    /// **A count rather than a flag, because a caller asks after the fact.** A command that starts and ends
    /// between two reads leaves `running` false at both, so nothing else here says one ran; this has moved.
    /// It moves once per command, whichever marker ended it, and not for a prompt drawn with nothing
    /// running. **It says that commands ended, not how many**: a shell emitting both marker families
    /// reports each command's start and end twice, a report that finds nothing running counts nothing,
    /// and the count wraps. It is compared for a difference and never for an order or an amount.
    pub finished: u32,
}

/// Where a session's account of its commands comes from, which is the broker's own to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vouching {
    /// The session was spawned with a key: its commands, and whether one is running, are taken from
    /// marks carrying it and from nothing else.
    pub keyed: bool,
    /// How many marks spelled as keyed ones were refused. A hook out of step with its session and a
    /// stream forging marks both show here, where a shell that marks nothing shows nothing.
    pub refused: u32,
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

/// What the library reports to its host, at an address that does not move: the address is what the
/// library is handed, and what each of its callbacks is handed back.
///
/// **A pointer and never a `Box`.** The callbacks run inside a write to the terminal and each makes a
/// reference to this from that pointer; a `Box` held beside it would be a second owner asserting it is the
/// only way in while they do. Everything here goes through the one pointer, a borrow at a time, and none
/// is alive across a call that parses.
struct Collector(NonNull<Reported>);

// The allocation is this value's alone, and it moves between threads only with the screen that owns it
unsafe impl Send for Collector {}

impl Collector {
    fn new() -> Self {
        Self(NonNull::from(Box::leak(Box::<Reported>::default())))
    }

    fn get(&self) -> &Reported {
        // SAFETY: allocated in `new`, freed in `drop`, and no callback is running: they run only inside
        // a call on the terminal, which this borrow's holder is not in
        unsafe { self.0.as_ref() }
    }

    fn get_mut(&mut self) -> &mut Reported {
        // SAFETY: as `get`, and `&mut self` makes this the only borrow
        unsafe { self.0.as_mut() }
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        // SAFETY: the pointer `new` leaked, freed once. `Screen` declares the terminal before this, so
        // the terminal — and with it every callback that could be handed the pointer — is gone already
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

/// One session's VT state.
pub struct Screen {
    /// Declared first so it is dropped first: the collector below is reachable from the library while this
    /// handle lives, and freeing it behind a live terminal would leave a dangling callback.
    terminal: Terminal,
    /// What the library reports — the replies it makes for a program, and the facts its own callbacks read.
    reported: Collector,
    cols: u16,
    rows: u16,
    /// One cell in pixels, as the window drawing this session measured it. Zero until one has.
    cell: (u16, u16),
    /// The core's input encoders, from the first input a view sends.
    encoders: Option<Encoders>,
    /// The buttons each view is holding down, as bits. A view not here holds none.
    pointers: HashMap<ViewId, u8>,
    /// What this session's views have been sent, for as long as it has a view. A session nothing views
    /// keeps none, so being viewable costs a session that is not viewed nothing.
    publication: Option<Publication>,
    /// Where the next publication's epochs start: past every epoch a view of this session was told.
    epochs: u32,
    /// The hold the program has on the screen (mode 2026), as its number, while there is one and it has
    /// not run past its limit.
    hold: Option<u64>,
    /// How many holds there have been, which is what numbers them.
    holds: u64,
    /// What says when a hold must be over. A screen with none is drawn when its program lets go and not
    /// before, which is what a screen nothing views needs.
    hold_limit: Option<HoldLimit>,
    /// The screen is being rebuilt from a recording: what the replay provokes is not acted on.
    replaying: bool,
    /// Where what is fed here is also written, for a session that asked to be recorded (`recording`).
    recorder: Option<Recorder>,
    /// A keyframe is due and is waiting for the parser to be between sequences (`keyframe_when_due`).
    keyframe_waits: bool,
    /// What the program last named its terminal, for a caller that was not there to be told.
    title: Option<String>,
    /// The session is over and its views have been told (`end_views`). The screen itself may be read a
    /// little longer — its reader holds it until the pty closes — and nothing may attach to it.
    ended: bool,
}

/// The epoch a session's lines are in, one cell in pixels, and each image with the line it is on.
pub type ImagesAt = (u32, (u16, u16), Vec<(u64, PlacementRead)>);

/// The most text one answer carries: what is left of a message once the rest of the answer is in it.
pub const TEXT_BYTES: usize = MAX_MESSAGE_BYTES - 64;

/// Why a view's question about lines was not answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unanswered {
    /// The core refused a call.
    Core(Failed),
    /// Nothing is viewing the session, and it is a view's frames that number its lines.
    Unviewed,
    /// The session is over: its views were told so, and there is nothing to attach to.
    Ended,
    /// The lines were numbered again since the question was put: it names lines of an epoch that is over.
    Renumbered { asked: u32, now: u32 },
    /// The answer is longer than one message carries.
    TooLong { bytes: usize },
}

impl From<Failed> for Unanswered {
    fn from(failed: Failed) -> Self {
        Self::Core(failed)
    }
}

impl std::fmt::Display for Unanswered {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(failed) => write!(out, "{failed}"),
            Self::Unviewed => write!(out, "no view is attached to this session, so its lines have no numbers"),
            Self::Ended => write!(out, "the session has ended"),
            Self::Renumbered { asked, now } => {
                write!(
                    out,
                    "the lines were numbered again since this was asked: it is of epoch {asked}, and this is {now}"
                )
            }
            Self::TooLong { bytes } => {
                write!(out, "the text is {bytes} bytes, and one answer carries {TEXT_BYTES} at most")
            }
        }
    }
}

/// The answer to a fetch: the lines it covers, and the rows among them the session holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub first: u64,
    /// How many lines from `first` this answers for (`wire::ToView::Rows`)
    pub count: u32,
    pub rows: Rows,
}

/// Where a run of bytes should be cut so that the terminal is looked at right after a sequence that
/// begins or ends a hold: one past the first `2026h` or `2026l` in it, or its whole length.
///
/// **Only where to look, never what was said.** The parser is fed every byte either way and reports a
/// hold through its own callback; a cut in the wrong place costs nothing, and a hold spelled some other
/// way is still learnt of, at the end of the read it was in.
fn hold_piece(bytes: &[u8]) -> usize {
    bytes
        .windows(5)
        .position(|window| window.starts_with(b"2026") && matches!(window[4], b'h' | b'l'))
        .map_or(bytes.len(), |at| at + 5)
}

/// A command the shell reported, with the line each of its moments is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandAt {
    pub id: u64,
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// Milliseconds since the Unix epoch
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub outcome: Outcome,
    /// The line its prompt began on
    pub prompt: Option<u64>,
    /// The line its output began on
    pub output: Option<u64>,
    /// The line the cursor was on when it ended
    pub end: Option<u64>,
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
        Self::with_history(cols, rows, VIEW_HISTORY_LINES)
    }

    /// A screen whose commands are taken from marks carrying `key` and from nothing else (`markers`).
    /// **Which it is is said here and nowhere later**: a session is keyed for its whole life or not at
    /// all.
    pub fn keyed(cols: u16, rows: u16, key: MarksKey) -> Self {
        let mut screen = Self::new(cols, rows);
        screen.reported.get_mut().vouch_with(key);
        screen
    }

    /// A screen that keeps `lines` of history rather than what a session keeps. For a case about what
    /// happens when history runs out, which at a session's size takes a hundred thousand lines to reach.
    pub fn with_history(cols: u16, rows: u16, lines: usize) -> Self {
        Self::build(cols, rows, lines).expect("libghostty-vt could not make a terminal")
    }

    /// **Five settings, each a default the core and this broker do not share.**
    ///
    /// The core keeps *no* scrollback limit of its own, where a session's history is bounded twice — in
    /// lines, which is what a narrow terminal reaches first, and in bytes, which is what a wide one does;
    /// and the advertised terminal has to be the one the pty says, because a program decides what to
    /// emit from `TERM` and answers a query differently when told something else.
    ///
    /// **The last two are about the window, which is what draws the session.** An emoji built from
    /// several code points is one character there, so it has to be one here: with clustering off this
    /// screen put the cursor four columns further right after a family emoji than the window drew it, and
    /// the cursor position a program is told comes from this screen. And the window draws no images, so
    /// the image protocol is off rather than answered `OK` on behalf of a renderer that would show
    /// nothing.
    fn build(cols: u16, rows: u16, history: usize) -> Result<Self, Failed> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        // **Before the terminal, so it goes after it.** A terminal that has been handed the collector
        // may reach it for as long as it lives, its own freeing included; were routing to fail part
        // way, these two are let go of in the reverse of the order they were made
        let reported = Collector::new();
        let mut terminal = Terminal::new(cols, rows)?;
        terminal.default_cursor_is_a_blinking_bar()?;
        // Never less than what a client reading the screen as text is promised (`SCROLLBACK_ASKED`)
        terminal.set_scrollback_lines(history.max(SCROLLBACK_ASKED))?;
        terminal.set_scrollback_bytes(HISTORY_BYTES)?;
        terminal.set_terminfo_name(TERMINFO_NAME)?;
        terminal.set_mode(GRAPHEME_CLUSTERING, true)?;
        terminal.refuse_images()?;

        // SAFETY: the collector outlives the handle — here by the order the two were made in, and from
        // then on by this struct's field order — and is reached only through this pointer
        unsafe { terminal.route_reports(reported.0.as_ptr()) }?;

        Ok(Self {
            terminal,
            reported,
            cols,
            rows,
            cell: (0, 0),
            encoders: None,
            pointers: HashMap::new(),
            publication: None,
            epochs: 0,
            hold: None,
            holds: 0,
            hold_limit: None,
            replaying: false,
            recorder: None,
            keyframe_waits: false,
            title: None,
            ended: false,
        })
    }

    /// Feeds bytes as they arrived.
    ///
    /// **Chunk boundaries are the parser's problem and it carries state across them**, which is why
    /// `tests/parser.rs` splits the same stream at every boundary and demands the same screen: a wrapper
    /// that buffered a chunk, or reset anything per call, would pass every test that feeds a whole stream.
    ///
    /// Everything the core reports — replies, the shell's markers, what the program asked of the world
    /// outside — is collected by callbacks that run inside this write, and taken afterwards.
    ///
    /// **The parse is stopped where something has to be looked at**, and nowhere else changes what is
    /// parsed: after each sequence a shell's marker can be, so the cell a command's prompt, output or end
    /// was at is marked while the cursor is still on it (`commands`); after a sequence that begins or
    /// ends a hold, so the screen a program let go of is the one sent (`settle_holds`); and, when a
    /// keyframe is waiting, at the first byte that leaves the parser between sequences.
    pub fn feed(&mut self, bytes: &[u8]) {
        // How much of `bytes` has been parsed, and how much of it the recording has been given
        let (mut fed, mut recorded) = (0, 0);
        while fed < bytes.len() {
            let piece = fed + hold_piece(&bytes[fed..]);
            while fed < piece {
                let (taken, ground) = self.terminal.feed_to(&bytes[fed..piece], self.keyframe_waits);
                fed += taken;
                self.reported.get_mut().place_marks(&self.terminal);
                if ground {
                    // What was parsed before the keyframe is recorded before it, and the rest after
                    self.record(|recorder| recorder.output(&bytes[recorded..fed]));
                    recorded = fed;
                    self.keyframe_waits = false;
                    self.keyframe_when_due();
                }
            }
            self.settle_holds();
        }
        if let Some(publication) = &mut self.publication {
            publication.touch();
        }
        self.record(|recorder| recorder.output(&bytes[recorded..]));
        self.keyframe_when_due();
    }

    /// Acts on each hold the program began or ended since this was last called.
    ///
    /// **A hold beginning starts its own clock**, whatever came before it: a program that lets go and
    /// takes hold again inside one read is holding a new frame, with the whole limit ahead of it.
    /// **A hold ending is when the frame is whole**, so that is when it is sent — here, with the terminal
    /// exactly as the program left it, rather than after whatever else the same read went on to draw.
    fn settle_holds(&mut self) {
        for began in self.reported.get_mut().take_holds() {
            if self.replaying {
                continue;
            }
            if began {
                self.holds += 1;
                self.hold = Some(self.holds);
                if let Some(limit) = &self.hold_limit {
                    limit.began(self.holds);
                }
            } else {
                self.hold = None;
                if let Some(publication) = &mut self.publication {
                    publication.touch();
                }
                self.publish();
            }
        }
    }

    /// Says what ends a hold of this screen's that its program does not (`holds`).
    pub fn limit_holds(&mut self, limit: HoldLimit) {
        self.hold_limit = Some(limit);
    }

    /// The hold numbered `hold` has gone on as long as one may: if it is still the one in force, the
    /// screen is drawn as it stands and the program's hold on it is over.
    pub fn end_hold(&mut self, hold: u64) {
        if self.hold == Some(hold) {
            self.hold = None;
            self.publish();
        }
    }

    /// Records every byte fed and every resize from here on at `path`, starting with the screen as it is.
    pub fn start_recording(&mut self, path: &Path) -> std::io::Result<()> {
        self.start_recording_with(Recorder::start(path)?)
    }

    /// Picks up the recording at `path`: the screen rebuilt from all of it and made ready for a new shell,
    /// and everything from here on appended to it. Answers whether there was anything to rebuild from.
    ///
    /// **Appended to rather than started again**, so the history a recording covers is not cut down to a
    /// keyframe's each time the app restarts.
    pub fn resume_recording(&mut self, path: &Path) -> std::io::Result<bool> {
        let (cols, rows) = (self.cols, self.rows);
        let (recorder, entries) = Recorder::resume(path)?;
        let restored = self.replay_all(&entries);
        if !restored {
            self.start_recording_with(recorder)?;
            return Ok(false);
        }
        self.recorder = Some(recorder);
        // What follows is another shell's stream: said when either it or the one before it is keyed, so
        // a later replay reads each with the key it was written under
        if self.reported.get().vouched_by().is_some() || recording::ends_vouched(&entries) {
            self.record_marks();
        }
        // Recorded like anything else fed: a later replay passes through this moment too
        self.settle_restored();
        self.resize(cols, rows, 0, 0);
        Ok(true)
    }

    /// Writes into the recording which key vouches for the stream that follows, and the number of the
    /// last start accepted under it — or that none does.
    ///
    /// **After every keyframe of a keyed session**, because a replay may begin at any of them: read
    /// from there with no key, a keyed session's commands would not be rebuilt at all, and read with the
    /// key but no number, marks from before the keyframe shown again after it would be taken for new.
    fn record_marks(&mut self) {
        let vouched = self.reported.get().vouched_by().map(|(key, last)| (key.clone(), last));
        self.record(|recorder| match &vouched {
            Some((key, last)) => recorder.marks(Some(key), *last),
            None => recorder.marks(None, 0),
        });
    }

    /// Stops recording, without waiting for the file: what was recorded is still on its way to it, and
    /// whatever records there next waits for that (`recording::claim`). For a session that is being
    /// ended: whatever its program still prints on its way out must not be written into a file the
    /// session replacing it has already picked up.
    pub fn stop_recording(&mut self) {
        self.recorder = None;
        self.keyframe_waits = false;
    }

    /// Waits until the recording's file holds everything recorded so far, for whoever is about to read it.
    pub fn flush_recording(&mut self) {
        if let Some(recorder) = &mut self.recorder {
            recorder.flush();
        }
    }

    /// Something to wait on until the recording's file holds everything recorded so far, or `None` when
    /// nothing is being recorded. **For a caller that holds this screen locked**: it takes this, lets
    /// the screen go, and waits then — the wait is for a disk, and the screen is what its reader needs.
    pub fn recording_reached(&mut self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.recorder.as_mut().and_then(Recorder::reached)
    }

    /// Whether what is fed here is being recorded. A recording whose file could not be written has
    /// stopped, and is not one.
    pub fn recorded(&self) -> bool {
        self.recorder.as_ref().is_some_and(|recorder| !recorder.failed())
    }

    /// The grid's height.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// What the program last named its terminal, if it has.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// The same into a recorder of the caller's making.
    pub fn start_recording_with(&mut self, mut recorder: Recorder) -> std::io::Result<()> {
        let redraw = self.redraw().map_err(|failed| std::io::Error::other(failed.to_string()))?;
        recorder.keyframe(self.cols, self.rows, &redraw)?;
        self.recorder = Some(recorder);
        if self.reported.get().vouched_by().is_some() {
            self.record_marks();
        }
        Ok(())
    }

    /// Writes to the recording, when there is one.
    ///
    /// **A recording that cannot be written stops**, rather than failing the session it is of: the disk
    /// filling must not be what ends a shell.
    fn record(&mut self, write: impl FnOnce(&mut Recorder) -> std::io::Result<()>) {
        if let Some(recorder) = &mut self.recorder
            && write(recorder).is_err()
        {
            self.recorder = None;
        }
    }

    /// Takes the keyframe the recording is due, if it is due one and one can be taken.
    ///
    /// **A keyframe waits for the primary screen**, because one taken while `vim` is showing would hold the
    /// editor and not the shell history under it — and a replay starting there would have lost it. The
    /// bytes are still recorded, so nothing is missing; the keyframe is only later. It stops waiting when
    /// the recording is past its budget, where a keyframe is the only way its oldest part can go.
    ///
    /// **And it waits for the parser to be between sequences.** A keyframe is the screen as it stands,
    /// and says nothing of a sequence the parser is part way through: a replay that began at one taken
    /// there would redraw the screen and then be fed the rest of that sequence as if it were text. So a
    /// keyframe that falls due mid-sequence is taken at the byte that ends the sequence (`feed`). A
    /// sequence that never ends would put it off for ever and the recording would grow without bound, so
    /// at twice its budget the keyframe is taken regardless.
    fn keyframe_when_due(&mut self) {
        let Some(recorder) = &self.recorder else { return };
        let primary = !self.terminal.alt_screen().unwrap_or(false);
        if !(recorder.keyframe_due() && (primary || recorder.over_budget())) {
            return;
        }
        if !recorder.far_over_budget() && !self.terminal.at_ground() {
            self.keyframe_waits = true;
            return;
        }
        let (cols, rows) = (self.cols, self.rows);
        let redraw = self.redraw();
        self.record(|recorder| {
            recorder.keyframe(cols, rows, &redraw.map_err(|failed| std::io::Error::other(failed.to_string()))?)
        });
        if self.reported.get().vouched_by().is_some() {
            self.record_marks();
        }
    }

    /// The screen and the `KEYFRAME_HISTORY_LINES` above it, as the sequences that redraw them.
    ///
    /// **Not the whole history**, which a keyframe would then write out again every `KEYFRAME_EVERY` bytes:
    /// at a hundred thousand lines that is tens of megabytes a time. What a keyframe leaves out is still
    /// in the recording, as the bytes that printed it, for as far back as the recording goes.
    fn redraw(&self) -> Result<Vec<u8>, Failed> {
        let scrollback = self.terminal.scrollback_rows()?;
        let from = scrollback.saturating_sub(KEYFRAME_HISTORY_LINES);
        let last = (self.cols.saturating_sub(1), scrollback + u32::from(self.rows) - 1);
        self.terminal.redraw(from, last)
    }

    /// Rebuilds this screen as a recording had it at `until`: the last keyframe at or before then, and
    /// what was fed after it. `false` when the recording holds nothing that early, and the screen is as it
    /// was.
    ///
    /// **Nothing the replay provokes leaves here.** A program that asked the terminal a question was
    /// answered when it asked; its clipboard write, its bell and its title were told then. Told again they
    /// would be a second answer to a program that is no longer there and a clipboard overwritten by the
    /// past.
    pub fn replay(&mut self, entries: &[Timed], until: u64) -> bool {
        let Some((start, end)) = recording::span(entries, until) else { return false };
        self.rebuild(&entries[start..end]);
        true
    }

    /// Rebuilds this screen from everything a recording holds: its first keyframe and all that was fed
    /// after it. `false` for a recording with no keyframe, and the screen is as it was.
    ///
    /// **From the first keyframe rather than the last**, which is what a session picking up where one left
    /// off wants and a rewind does not: a keyframe carries a bounded history, and the bytes after the
    /// first one rebuild all the history the recording covers.
    pub fn replay_all(&mut self, entries: &[Timed]) -> bool {
        let Some(first) = entries.iter().position(|timed| matches!(timed.entry, Entry::Keyframe { .. })) else {
            return false;
        };
        self.rebuild(&entries[first..]);
        true
    }

    /// Makes this screen what `entries` leave a terminal as, the first of them being a keyframe. A
    /// keyframe after the first is passed over: it says again what the bytes before it already drew.
    ///
    /// **The commands are the recording's, with the recording's times.** The ones this screen held are
    /// forgotten first — the replay reports them again, and kept they would each be listed twice — and
    /// each entry is parsed as of when it was recorded, so a command is stamped with when it ran rather
    /// than with when somebody looked back at it.
    ///
    /// **And they are the commands that were.** A recording says which key each stretch of its stream
    /// was vouched with (`Entry::Marks`), and the replay reads each stretch with that key and that
    /// numbering: a mark the session refused is refused again, and a command it recorded ends as it
    /// ended. This screen's own key and numbering are set aside for the replay and put back after it.
    fn rebuild(&mut self, entries: &[Timed]) {
        // The recording is of another moment: writing the replay into it would record the past as now
        let recorder = self.recorder.take();
        self.replaying = true;
        let live = self.reported.get_mut().begin_replay();
        self.reported.get_mut().forget_commands();
        // A full reset, and the history it leaves erased: the keyframe redraws both
        self.feed(b"\x1bc\x1b[3J");
        for (index, timed) in entries.iter().enumerate() {
            let at = SystemTime::UNIX_EPOCH + Duration::from_millis(timed.at);
            self.reported.get_mut().set_clock(Some(at));
            match &timed.entry {
                Entry::Keyframe { cols, rows, redraw } if index == 0 => {
                    self.resize(*cols, *rows, 0, 0);
                    self.feed(redraw);
                }
                Entry::Keyframe { .. } => {}
                Entry::Output(bytes) => self.feed(bytes),
                Entry::Resize { cols, rows } => self.resize(*cols, *rows, 0, 0),
                Entry::Marks { key, last } => {
                    self.reported.get_mut().replay_vouched_with(key.clone(), *last);
                    // A command the shell before this one left running ended here, where the cursor is
                    self.reported.get_mut().place_marks(&self.terminal);
                }
            }
        }
        self.forget_the_shell();
        self.reported.get_mut().end_replay(live);
        self.reported.get_mut().set_clock(None);
        self.replaying = false;
        // Whatever the recorded program was holding when the recording stops, nothing is holding now
        self.hold = None;
        self.recorder = recorder;
    }

    /// Forgets what the shell said about itself (`Reported::forget_the_shell`), and gives the command it
    /// left running the place it ended at: where the cursor is.
    fn forget_the_shell(&mut self) {
        self.reported.get_mut().forget_the_shell();
        self.reported.get_mut().place_marks(&self.terminal);
    }

    /// Leaves a screen rebuilt from a recording ready for a new shell: out of whatever application was
    /// showing, the modes a program set put back, and the cursor on a line of its own.
    ///
    /// The shell about to start knows none of what the last one's programs asked of the terminal — that
    /// the mouse was being reported, that the keypad was in application mode — and would be typed at
    /// through all of it.
    pub fn settle_restored(&mut self) {
        // **Out of the alternate screen only when on it.** Leaving it also puts the cursor back where it
        // was saved, which is right for an application that was showing — that is where the shell's cursor
        // was — and wrong for a shell that was not in one: the cursor would go to wherever some program
        // last saved it, and the new shell would draw over what the last one printed.
        if self.terminal.alt_screen().unwrap_or(false) {
            self.feed(b"\x1b[?1049l");
        }
        // No mouse reporting, bracketed paste, focus reports or hold on the screen; a soft reset for the
        // rest (keypad, cursor keys, margins, attributes); the cursor shown, on a fresh line
        self.feed(
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?2026l\x1b[!p\x1b[?25h\x1b[0m\r\n",
        );
        self.forget_the_shell();
    }

    /// Whether the program has asked to be told about the mouse.
    pub fn mouse_reported(&self) -> Result<bool, Failed> {
        self.terminal.mouse_tracking()
    }

    /// What the program did that is not on its screen, since this was last taken.
    pub fn take_effects(&mut self) -> Vec<Effect> {
        let effects = self.reported.get_mut().take_effects();
        for effect in &effects {
            if let Effect::Title(title) = effect {
                self.title = (!title.is_empty()).then(|| title.clone());
            }
        }
        effects
    }

    /// What the shell has said about itself.
    pub fn facts(&self) -> &Facts {
        self.reported.get().facts()
    }

    /// Whether this session's commands are taken from keyed marks alone, and how many such marks it
    /// has refused.
    pub fn vouching(&self) -> Vouching {
        let reported = self.reported.get();
        Vouching { keyed: reported.vouched_by().is_some(), refused: reported.refused() }
    }

    /// The session's process is gone: a command a keyed session's shell left running is over, where
    /// the cursor is, with nothing having said how.
    pub fn session_ended(&mut self) {
        self.reported.get_mut().session_ended();
        self.reported.get_mut().place_marks(&self.terminal);
    }

    /// What the program asked for and must be told, taken rather than read: each reply is written once.
    pub fn take_replies(&mut self) -> Vec<String> {
        self.reported.get_mut().take_replies()
    }

    /// The grid changed, and perhaps the size of a cell with it.
    ///
    /// **A cell size of zero means the caller does not know one, and the last one known stands.** Only a
    /// window measures a cell, and not every resize comes from one.
    ///
    /// **A size the core refuses is not the size.** The grid recorded here is what a row is read against
    /// and what a recording says the terminal was, so it moves only when the terminal did.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let (cell_width, cell_height) =
            if cell_width > 0 && cell_height > 0 { (cell_width, cell_height) } else { self.cell };
        if self.terminal.resize(cols, rows, cell_width, cell_height).is_err() {
            return;
        }
        (self.cols, self.rows, self.cell) = (cols, rows, (cell_width, cell_height));
        if let Some(publication) = &mut self.publication {
            publication.touch();
        }
        // A resize ends a hold: the frame the program was drawing is of a grid that is gone
        self.settle_holds();
        self.record(|recorder| recorder.resize(cols, rows));
        self.reported.get_mut().set_size((cell_width > 0).then_some(crate::vt::Size {
            cols: self.cols,
            rows: self.rows,
            cell_width,
            cell_height,
        }));
        // **Its views are sent the new grid here**, like anything else that changed the screen: a
        // program told its terminal was resized may print nothing in answer, and a view would otherwise
        // go on drawing the old grid until it did. Not while a recording is replayed, which passes
        // through every size the recording had on its way to the one it shows
        if !self.replaying {
            self.publish();
        }
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

    /// What the pointer sends the program now, which is nothing unless it asked for the mouse. `view` is
    /// the view it came from, which is whose buttons are down (`Encoders::pointer`).
    pub fn pointer(&mut self, view: ViewId, pointer: &Pointer) -> Result<Vec<u8>, Failed> {
        self.encoders()?;
        let held = self.pointers.entry(view).or_default();
        let encoded = match &mut self.encoders {
            Some(encoders) => encoders.pointer(&self.terminal, pointer, held),
            None => Ok(Vec::new()),
        };
        if *held == 0 {
            self.pointers.remove(&view);
        }
        encoded
    }

    /// A view holds no button down any more, whatever it last said: it lost focus, so the release went
    /// somewhere else, or it has gone.
    pub fn release_pointer(&mut self, view: ViewId) {
        self.pointers.remove(&view);
    }

    /// Pasted text as the program should receive it, or a refusal when it could run a command.
    pub fn paste(&self, text: &str, allow_unsafe: bool) -> Result<Pasted, Failed> {
        input::paste(&self.terminal, text, allow_unsafe)
    }

    /// What the program is told about its terminal gaining or losing focus.
    pub fn focus(&self, gained: bool) -> Result<Vec<u8>, Failed> {
        input::focus(&self.terminal, gained)
    }

    /// Starts sending a view this session's frames through `outlet`, beginning with the whole screen.
    ///
    /// **Refused once the session is over.** Its views have been told it ended and nothing will tell a
    /// later one: a view that attached then would be drawing a screen whose end it never hears of.
    pub fn attach_view(&mut self, id: ViewId, outlet: impl Outlet + 'static) -> Result<(), Unanswered> {
        if self.ended {
            return Err(Unanswered::Ended);
        }
        if self.publication.is_none() {
            self.publication = Some(Publication::new(&self.terminal, self.epochs)?);
        }
        let commands = self.reported.get().commands().generation();
        let held = self.hold.is_some();
        let attached = match &mut self.publication {
            Some(publication) => publication.attach(&self.terminal, commands, held, id, Box::new(outlet)),
            None => Ok(()),
        };
        self.drop_unviewed();
        Ok(attached?)
    }

    /// Lets go of what the views were told once the last of them has gone.
    ///
    /// **The next view's lines are numbered past these.** A publication counts its epochs from where it
    /// was started, and a view that comes back holds rows from the epochs it was told; starting again at
    /// the same number would tell it those rows are still good.
    fn drop_unviewed(&mut self) {
        if let Some(publication) = self.publication.take_if(|publication| publication.unviewed()) {
            self.epochs = publication.epoch().wrapping_add(1);
        }
    }

    pub fn detach_view(&mut self, id: ViewId) {
        if let Some(publication) = &mut self.publication {
            publication.detach(id);
        }
        self.release_pointer(id);
        self.drop_unviewed();
    }

    /// Every view a connection had, when the connection goes.
    pub fn detach_connection(&mut self, connection: u64) {
        if let Some(publication) = &mut self.publication {
            publication.detach_connection(connection);
        }
        self.pointers.retain(|view, _| view.connection != connection);
        self.drop_unviewed();
    }

    /// A view drew the frame with this sequence.
    pub fn acknowledge(&mut self, id: ViewId, sequence: u64) -> Result<(), Failed> {
        let commands = self.reported.get().commands().generation();
        let held = self.hold.is_some();
        let acknowledged = match &mut self.publication {
            Some(publication) => publication.acknowledge(&self.terminal, commands, held, id, sequence),
            None => Ok(()),
        };
        self.drop_unviewed();
        acknowledged
    }

    /// Sends each view that is waiting for one the frame it is owed. Called where bytes land, where a
    /// view acknowledges, where the grid changes and where a hold ends, which between them are every
    /// moment a frame can become due.
    pub fn publish(&mut self) {
        let commands = self.reported.get().commands().generation();
        let held = self.hold.is_some();
        if let Some(publication) = &mut self.publication {
            let _ = publication.pump(&self.terminal, commands, held);
        }
        self.drop_unviewed();
    }

    /// What numbers this session's lines, brought up to the terminal: what its views are told.
    ///
    /// **Only while something views it.** A line's number is a view's way of keeping what it was sent,
    /// and means nothing to anything else; a session nothing views keeps no numbering, so a question
    /// about lines put to one is refused rather than answered in numbers made up for the occasion.
    fn positioned(&mut self) -> Result<&mut Publication, Unanswered> {
        let publication = self.publication.as_mut().ok_or(Unanswered::Unviewed)?;
        publication.advance(&self.terminal)?;
        Ok(publication)
    }

    /// The same, for a question that names lines: refused when the lines have been numbered again since
    /// `epoch`, which is the numbering the question was put in. Answered in the new numbering it would
    /// be about whichever lines now have those numbers.
    fn numbered(&mut self, epoch: u32) -> Result<&mut Publication, Unanswered> {
        let publication = self.positioned()?;
        let now = publication.epoch();
        if epoch == now { Ok(publication) } else { Err(Unanswered::Renumbered { asked: epoch, now }) }
    }

    /// Lines by number, for a view that does not hold them. `epoch` is the numbering the view asked in.
    pub fn fetch(&mut self, epoch: u32, first: u64, count: u32) -> Result<Fetched, Unanswered> {
        self.numbered(epoch)?;
        let Some(publication) = &mut self.publication else { return Err(Unanswered::Unviewed) };
        let (count, rows) = publication.fetch(&self.terminal, first, count)?;
        Ok(Fetched { first, count, rows })
    }

    fn span_of(publication: &Publication, cells: (reads::Cell, reads::Cell)) -> Span {
        let position = |cell: reads::Cell| Position { line: publication.line_of(cell.1), column: cell.0 };
        Span { start: position(cells.0), end: position(cells.1) }
    }

    /// The word at a cell. `None` where there is no word, or no such line any more.
    pub fn word_at(&mut self, epoch: u32, at: Position) -> Result<Option<Span>, Unanswered> {
        let Some(row) = self.numbered(epoch)?.screen_row(at.line) else { return Ok(None) };
        let found = self.terminal.word_at((at.column, row))?;
        let publication = self.positioned()?;
        Ok(found.map(|cells| Self::span_of(publication, cells)))
    }

    /// The whole line a cell is on, across the rows it wrapped onto.
    pub fn line_at(&mut self, epoch: u32, at: Position) -> Result<Option<Span>, Unanswered> {
        let Some(row) = self.numbered(epoch)?.screen_row(at.line) else { return Ok(None) };
        let found = self.terminal.line_at((at.column, row))?;
        let publication = self.positioned()?;
        Ok(found.map(|cells| Self::span_of(publication, cells)))
    }

    /// The text of a span as a program wrote it. Empty when either end is a line no longer held, and
    /// refused when it is more than one answer carries.
    pub fn text_of(&mut self, epoch: u32, span: Span) -> Result<String, Unanswered> {
        let publication = self.numbered(epoch)?;
        let (Some(first), Some(last)) =
            (publication.screen_row(span.start.line), publication.screen_row(span.end.line))
        else {
            return Ok(String::new());
        };
        self.terminal
            .text_within((span.start.column, first), (span.end.column, last), TEXT_BYTES)?
            .map_err(|bytes| Unanswered::TooLong { bytes })
    }

    /// Every place `needle` appears, newest first, with the epoch the line numbers are in.
    pub fn find(&mut self, needle: &str) -> Result<(u32, Vec<Span>), Unanswered> {
        let found = self.terminal.find(needle)?;
        let publication = self.positioned()?;
        Ok((publication.epoch(), found.into_iter().map(|cells| Self::span_of(publication, cells)).collect()))
    }

    /// What this session's screens and history hold in memory now.
    pub fn resident_bytes(&self) -> u64 {
        self.terminal.resident_bytes().unwrap_or(0)
    }

    /// A number that moves whenever history becomes worth compressing again (`housekeeping`).
    pub fn compression_activity(&self) -> u64 {
        self.terminal.compression_activity().unwrap_or(0)
    }

    /// Compresses some of the history nobody is looking at. Answers whether there is more to do.
    pub fn compress_step(&mut self) -> bool {
        self.terminal.compress_step().unwrap_or(false)
    }

    /// Lets programs in this session send images (`vt::images`). Off unless asked for, so a program
    /// asking whether it may is not told yes by a session nothing is drawing.
    pub fn allow_images(&mut self) -> Result<(), Failed> {
        self.terminal.allow_images()
    }

    /// Every image in this session with the line it is on, the epoch those lines are in, and one cell
    /// in pixels as a window last measured it.
    pub fn images(&mut self) -> Result<ImagesAt, Unanswered> {
        let placements = self.terminal.placements()?;
        let cell = self.cell;
        let publication = self.positioned()?;
        let placed = placements.into_iter().map(|placement| (publication.line_of(placement.row), placement)).collect();
        Ok((publication.epoch(), cell, placed))
    }

    /// Part of an image's pixels, or `None` for an image that is not there.
    pub fn image(&self, id: u32, offset: u32) -> Option<ImageRead> {
        self.terminal.image(id, offset as usize)
    }

    /// Moves whenever a command is reported, so something watching knows to ask for them again.
    pub fn command_generation(&self) -> u32 {
        self.reported.get().commands().generation()
    }

    /// Where a mark is now — its column, and its row counted from the oldest line held — or `None` for
    /// one whose row has left the history, or that is on the screen that is not showing.
    ///
    /// **A mark answers in the numbering of the screen it was made on.** The primary screen and an
    /// alternate-screen application each count their rows from their own top, so a mark on the shell's
    /// screen read while `vim` is showing would name a row of `vim`.
    fn place_of(placed: &Placed, alternate: bool) -> Option<(u16, u32)> {
        if placed.alternate == alternate { placed.mark.screen_cell() } else { None }
    }

    /// Every command the shell has reported, with the line each of its moments is on now, and the epoch
    /// those numbers are in. A moment whose row has left the history has no line; nor has any moment
    /// while nothing views the session, when no line has a number.
    pub fn commands(&mut self) -> Result<(u32, Vec<CommandAt>), Failed> {
        let alternate = self.terminal.alt_screen()?;
        let numbering = match &mut self.publication {
            Some(publication) => {
                publication.advance(&self.terminal)?;
                Some(&*publication)
            }
            None => None,
        };
        let line = |placed: &Option<Placed>| {
            let (_, row) = Self::place_of(placed.as_ref()?, alternate)?;
            Some(numbering?.line_of(row))
        };
        let since_epoch = |time: Option<SystemTime>| {
            time.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        };
        let commands = self
            .reported
            .get()
            .commands()
            .iter()
            .map(|command| CommandAt {
                id: command.id,
                command: command.command.clone(),
                cwd: command.cwd.clone(),
                started_ms: since_epoch(command.started),
                finished_ms: since_epoch(command.finished),
                outcome: command.outcome,
                prompt: line(&command.prompt),
                output: line(&command.output),
                end: line(&command.end),
            })
            .collect();
        Ok((numbering.map_or(0, Publication::epoch), commands))
    }

    /// What a command printed, as text, or `None` for a command that is not kept, whose output has
    /// left the history, or that ran on the screen that is not showing.
    ///
    /// From the row its output began on to the last cell it wrote — or, while it runs, to the cursor. A
    /// command that ended part way along a row printed that row too: a last line with no newline is
    /// still output. Both ends are marks the core carries with the text, so they are still its ends
    /// after a resize has wrapped the lines somewhere else.
    pub fn command_output(&self, id: u64) -> Result<Option<String>, Failed> {
        let Some(command) = self.reported.get().commands().get(id) else { return Ok(None) };
        let alternate = self.terminal.alt_screen()?;
        let Some((_, first)) = command.output.as_ref().and_then(|placed| Self::place_of(placed, alternate)) else {
            return Ok(None);
        };
        let columns = self.terminal.cols()?;
        let last: Option<(u16, u32)> = match &command.end {
            Some(end) => match Self::place_of(end, alternate) {
                Some(cell) if end.closes => Some(cell),
                // It ended at the start of a row, having written nothing on it: the row above is its last
                Some((_, row)) => row.checked_sub(1).map(|above| (columns.saturating_sub(1), above)),
                None => return Ok(None),
            },
            // Still running, or its end was never marked: everything down to the cursor
            None => {
                let row = self.terminal.scrollback_rows()? + u32::from(self.terminal.cursor_row()?);
                Some((columns.saturating_sub(1), row))
            }
        };
        match last {
            Some(last) if last.1 >= first => self.terminal.text_between((0, first), last).map(Some),
            // It ended where it began, having printed nothing
            _ => Ok(Some(String::new())),
        }
    }

    /// The epoch line numbers are in and the line the screen's first row is, as of now.
    pub fn view_position(&mut self) -> Result<(u32, u64), Unanswered> {
        let publication = self.positioned()?;
        Ok((publication.epoch(), publication.first_active()))
    }

    /// Tells every view the session is over, after the screen as it ended.
    pub fn end_views(&mut self) {
        let commands = self.reported.get().commands().generation();
        if let Some(mut publication) = self.publication.take() {
            publication.end(&self.terminal, commands);
            self.epochs = publication.epoch().wrapping_add(1);
        }
        self.pointers.clear();
        self.ended = true;
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
