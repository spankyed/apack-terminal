//! What a view and the broker say to each other, as bytes.
//!
//! **A view is not a client.** A client asks about sessions and is told what happened to them, as JSON a
//! person can read; a view is sent the screen, many times a second, and a screen as JSON is mostly quotes.
//! So a connection that greets as a `view` speaks this instead, after the one JSON line each way that is
//! the handshake.
//!
//! **A message is a length, a kind and a payload**: a `u32` little-endian count of what follows, one byte
//! saying which message, and that message's fields in order. Integers are LEB128, so a line number costs a
//! byte until the session is long; a string is its length and its UTF-8.
//!
//! **Both directions are encoded and decoded here**, though the broker only ever does half of each. The
//! other half is what `tests/view_wire.rs` reads its own output back with, what the model view in
//! `tests/view_model.rs` is, and what the vectors `node/` decodes are written by.

use std::io::Read;

/// The largest message either side may send.
///
/// A ceiling on what is read before it is understood, as `frames::MAX_FRAME_BYTES` is for a client.
/// **Whatever builds a message keeps it under this** — a frame or a fetch carries the rows that fit and
/// says which it left (`Rows::takes`), a text too long to carry is refused — because a message past it is
/// one a view must hang up on, and re-attached it would be sent the same one again.
pub const MAX_MESSAGE_BYTES: usize = 8 << 20;

/// The most a frame can say beside its rows, which is more than a fetch can: every number at its
/// longest, a cursor, a line to forget from, and every colour a program can change.
///
/// Counted from what `frame` writes, in its order: the kind; channel, sequence, epoch, first line,
/// columns, rows, scrollback, commands and images; the flags; the cursor; the stale line; the colours'
/// own flags, the three that are not the palette, the palette's count and its 256 entries of an index
/// and a colour; and the count of rows.
pub const MAX_HEAD_BYTES: usize =
    1 + (5 + 10 + 5 + 10 + 3 + 3 + 5 + 5 + 5) + 1 + (3 + 3 + 1) + 10 + (1 + 9 + 2 + 256 * 4) + 5;

/// How much of a message its rows may be: what is left when the largest of everything else a frame or a
/// fetch carries beside them is taken off.
pub const MAX_ROWS_BYTES: usize = MAX_MESSAGE_BYTES - MAX_HEAD_BYTES;

/// A colour a cell asks for: the terminal's own, one of the 256 a palette names, or exactly this one.
///
/// **A palette index stays an index.** The colours a theme names are the view's to choose, so resolving
/// them here would freeze a session in the theme it started with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Color {
    #[default]
    Default,
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// How a character is drawn. `flags` is the core's own layout: an attribute per bit in the low eight, the
/// underline kind in the three above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline: Color,
    pub flags: u16,
}

/// The colours a program changed from the ones a session was given, which is what a view drawing in its
/// own theme has to be told: each is absent where the program left it alone.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColorChanges {
    /// The colour of text with no colour of its own
    pub foreground: Option<[u8; 3]>,
    pub background: Option<[u8; 3]>,
    pub cursor: Option<[u8; 3]>,
    /// The palette entries that are not what they were given as, by index, lowest first
    pub palette: Vec<(u8, [u8; 3])>,
}

/// One character on a row: the cell it starts in, how many it covers, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Glyph {
    pub column: u16,
    pub width: u8,
    pub text: String,
    /// Which of its row's styles, counted from one; `0` is the plain one
    pub style: u16,
    /// Which of its row's links, counted from one; `0` is none
    pub link: u16,
}

/// One line of the screen or its history, whole: the tables its glyphs index are its own.
///
/// **A row says everything about itself**, so the bytes of one are the same in every message that
/// carries it. The broker writes a row out once when it changes and hands those bytes to each view it is
/// owed to, and what a message holds is known to the byte before it is built.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    pub line: u64,
    pub wrapped: bool,
    pub prompt: bool,
    /// The styles on this row other than the plain one. A glyph's `style` of `n` is `styles[n - 1]`
    pub styles: Vec<Style>,
    /// The links on this row. A glyph's `link` of `n` is `links[n - 1]`
    pub links: Vec<String>,
    pub glyphs: Vec<Glyph>,
}

impl Row {
    /// How a glyph of this row is drawn.
    pub fn style_of(&self, glyph: &Glyph) -> Style {
        glyph.style.checked_sub(1).and_then(|at| self.styles.get(usize::from(at))).copied().unwrap_or_default()
    }

    /// What a glyph of this row links to, if anything.
    pub fn link_of(&self, glyph: &Glyph) -> Option<&str> {
        glyph.link.checked_sub(1).and_then(|at| self.links.get(usize::from(at))).map(String::as_str)
    }
}

/// Rows, as a message was read to hold them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Block {
    pub rows: Vec<Row>,
}

/// One character of a row being written, with its text borrowed from wherever the row keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell<'a> {
    pub column: u16,
    pub width: u8,
    pub text: &'a str,
    pub style: u16,
    pub link: u16,
}

/// A row's characters and the tables they index, however the row holds them: one a message was read
/// into, or one read out of a terminal (`vt::rows::RowRead`).
pub trait Cells {
    fn styles(&self) -> &[Style];
    fn links(&self) -> &[String];
    fn count(&self) -> usize;
    fn cell(&self, index: usize) -> Cell<'_>;
}

impl Cells for Row {
    fn styles(&self) -> &[Style] {
        &self.styles
    }

    fn links(&self) -> &[String] {
        &self.links
    }

    fn count(&self) -> usize {
        self.glyphs.len()
    }

    fn cell(&self, index: usize) -> Cell<'_> {
        let glyph = &self.glyphs[index];
        Cell { column: glyph.column, width: glyph.width, text: &glyph.text, style: glyph.style, link: glyph.link }
    }
}

/// Appends a row to `out` as a message carries one, less the line number in front of it: its flags, its
/// styles, its links, and its characters as runs.
///
/// **A run of plain characters is one string**, which is what most of a screen is: each code point one
/// narrow cell, the next in the next. Anything else — a wide character, a character of several code
/// points — goes in a run that says each one's width.
pub fn write_row(out: &mut Vec<u8>, wrapped: bool, prompt: bool, row: &impl Cells) {
    out.push(row_flags(wrapped, prompt));
    number(out, row.styles().len() as u64);
    for style in row.styles() {
        color(out, style.fg);
        color(out, style.bg);
        color(out, style.underline);
        number(out, style.flags);
    }
    number(out, row.links().len() as u64);
    for link in row.links() {
        text(out, link);
    }
    let mut runs: u64 = 0;
    let mut index = 0;
    while index < row.count() {
        index = run_end(row, index);
        runs += 1;
    }
    number(out, runs);
    let mut index = 0;
    while index < row.count() {
        let end = run_end(row, index);
        let first = row.cell(index);
        number(out, first.column);
        number(out, first.style);
        number(out, first.link);
        if is_simple(&first) {
            out.push(0);
            let length: usize = (index..end).map(|at| row.cell(at).text.len()).sum();
            number(out, length as u64);
            for at in index..end {
                out.extend_from_slice(row.cell(at).text.as_bytes());
            }
        } else {
            out.push(1);
            number(out, (end - index) as u64);
            for at in index..end {
                let cell = row.cell(at);
                out.push(cell.width);
                text(out, cell.text);
            }
        }
        index = end;
    }
}

/// One past the last cell of the run that starts at `start`: the characters side by side from there that
/// share its style and its link, and are simple or not as it is.
fn run_end(row: &impl Cells, start: usize) -> usize {
    let first = row.cell(start);
    let simple = is_simple(&first);
    let mut last = first;
    let mut end = start + 1;
    while end < row.count() {
        let next = row.cell(end);
        let adjacent = u32::from(last.column) + u32::from(last.width) == u32::from(next.column);
        if !(adjacent && next.style == first.style && next.link == first.link && is_simple(&next) == simple) {
            break;
        }
        last = next;
        end += 1;
    }
    end
}

/// The byte a row's body begins with: whether it runs on into the next, and whether the shell drew its
/// prompt there.
pub fn row_flags(wrapped: bool, prompt: bool) -> u8 {
    u8::from(wrapped) | (u8::from(prompt) << 1)
}

/// Whether a character is one code point in one cell, which is what a simple run is made of.
fn is_simple(cell: &Cell<'_>) -> bool {
    cell.width == 1 && cell.text.chars().count() == 1
}

/// Rows written out as a message carries them, ready to be put in one.
///
/// **What a frame and a fetch are built in**, a row at a time, so the size of what is being built is
/// known as it grows and it stops at what one message may hold.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Rows {
    count: u32,
    bytes: Vec<u8>,
}

impl Rows {
    /// Whether a row whose body is `body` bytes long still fits. The first row always does: a single row
    /// is far smaller than a message, and a message that could carry none would never make progress.
    pub fn takes(&self, body: usize) -> bool {
        // Ten bytes is the longest a line number is written
        self.count == 0 || self.bytes.len() + body + 10 <= MAX_ROWS_BYTES
    }

    /// Adds the row on `line` whose body `write_row` wrote.
    pub fn push(&mut self, line: u64, body: &[u8]) {
        number(&mut self.bytes, line);
        self.bytes.extend_from_slice(body);
        self.count += 1;
    }

    /// How many rows are held.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The rows read back, as a view reads them.
    pub fn block(&self) -> Result<Block, Malformed> {
        let mut bytes = Vec::with_capacity(self.bytes.len() + 5);
        number(&mut bytes, self.count);
        bytes.extend_from_slice(&self.bytes);
        let mut read = Reader { bytes: &bytes };
        let block = read.block()?;
        read.finish(block)
    }

    fn of(block: &Block) -> Self {
        let mut rows = Self::default();
        let mut body = Vec::new();
        for row in &block.rows {
            body.clear();
            write_row(&mut body, row.wrapped, row.prompt, row);
            rows.push(row.line, &body);
        }
        rows
    }

    fn write(&self, out: &mut Writer) {
        out.number(self.count);
        out.bytes.extend_from_slice(&self.bytes);
    }
}

/// Everything a frame says but its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHead<'a> {
    pub channel: u32,
    pub sequence: u64,
    pub epoch: u32,
    pub first_active: u64,
    pub columns: u16,
    pub screen_rows: u16,
    pub scrollback: u32,
    pub commands: u32,
    pub images: u32,
    pub cursor: Option<Cursor>,
    pub alternate_screen: bool,
    pub mouse: bool,
    pub reverse_video: bool,
    pub stale_from: Option<u64>,
    pub colors: Option<&'a ColorChanges>,
}

/// A frame as the message a view is sent, from its head and rows already written out.
pub fn frame(head: &FrameHead<'_>, rows: &Rows) -> Result<Vec<u8>, TooLong> {
    framed(kind::FRAME, |out| {
        out.number(head.channel);
        out.number(head.sequence);
        out.number(head.epoch);
        out.number(head.first_active);
        out.number(head.columns);
        out.number(head.screen_rows);
        out.number(head.scrollback);
        out.number(head.commands);
        out.number(head.images);
        let blinking = head.cursor.is_some_and(|cursor| cursor.blinking);
        out.byte(
            if head.cursor.is_some() { FRAME_CURSOR } else { 0 }
                | if head.alternate_screen { FRAME_ALTERNATE } else { 0 }
                | if head.mouse { FRAME_MOUSE } else { 0 }
                | if head.stale_from.is_some() { FRAME_STALE } else { 0 }
                | if blinking { FRAME_BLINKING } else { 0 }
                | if head.reverse_video { FRAME_REVERSE } else { 0 }
                | if head.colors.is_some() { FRAME_COLORS } else { 0 },
        );
        if let Some(cursor) = head.cursor {
            out.number(cursor.column);
            out.number(cursor.row);
            out.byte(cursor.shape);
        }
        if let Some(stale) = head.stale_from {
            out.number(stale);
        }
        if let Some(colors) = head.colors {
            // Which of the three are set, then those, then the palette entries by index
            let set = [colors.foreground, colors.background, colors.cursor];
            out.byte(
                if set[0].is_some() { COLOR_FOREGROUND } else { 0 }
                    | if set[1].is_some() { COLOR_BACKGROUND } else { 0 }
                    | if set[2].is_some() { COLOR_CURSOR } else { 0 },
            );
            for rgb in set.into_iter().flatten() {
                out.bytes.extend_from_slice(&rgb);
            }
            out.number(colors.palette.len() as u64);
            for (index, rgb) in &colors.palette {
                out.byte(*index);
                out.bytes.extend_from_slice(rgb);
            }
        }
        rows.write(out);
    })
}

/// The answer to a fetch as the message a view is sent, from rows already written out. `first` and
/// `count` are the lines the answer covers (`ToView::Rows`).
pub fn rows(channel: u32, request: u32, epoch: u32, first: u64, count: u32, rows: &Rows) -> Result<Vec<u8>, TooLong> {
    framed(kind::ROWS, |out| {
        out.number(channel);
        out.number(request);
        out.number(epoch);
        out.number(first);
        out.number(count);
        rows.write(out);
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub column: u16,
    pub row: u16,
    /// `0` block, `1` bar, `2` underline, `3` hollow
    pub shape: u8,
    pub blinking: bool,
}

/// The screen as it now stands, as a change from what this view was last sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub channel: u32,
    pub sequence: u64,
    pub epoch: u32,
    pub first_active: u64,
    pub columns: u16,
    pub screen_rows: u16,
    pub scrollback: u32,
    /// Moves whenever the shell reports a command, so a view knows to ask for them again
    pub commands: u32,
    /// Moves whenever an image is sent, placed, replaced or deleted, so a view knows to ask for them again
    pub images: u32,
    pub cursor: Option<Cursor>,
    pub alternate_screen: bool,
    pub mouse: bool,
    /// The program has the screen in reverse video (mode 5): default foreground and background swapped
    pub reverse_video: bool,
    pub stale_from: Option<u64>,
    /// The colours the program has changed from the ones the session was given, **all of them, when any
    /// of them changed** since this view's last frame, and in the first frame a view is sent: what is
    /// here replaces what the view held, and absent the view goes on with what it has.
    pub colors: Option<ColorChanges>,
    pub block: Block,
}

/// A command the shell reported, with the line each of its moments is on. A moment whose row has left
/// the history has no line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandRow {
    pub id: u64,
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// Milliseconds since the Unix epoch
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub exit_status: Option<i32>,
    pub prompt: Option<u64>,
    pub output: Option<u64>,
    pub end: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u64,
    pub column: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

/// The button a pointer event is about. `NONE` is a move with nothing held, or a wheel.
pub const NO_BUTTON: u8 = 255;

/// What a view sends.
#[derive(Debug, Clone, PartialEq)]
pub enum ToBroker {
    /// Start sending this session's frames, under a number of the view's choosing.
    Attach {
        channel: u32,
        session: String,
    },
    Detach {
        channel: u32,
    },
    /// The frame with this sequence has been drawn; the next may be sent.
    Acknowledge {
        channel: u32,
        sequence: u64,
    },
    /// Lines the view does not hold.
    Fetch {
        channel: u32,
        request: u32,
        epoch: u32,
        first: u64,
        count: u32,
    },
    /// A key as the keyboard reported it. `action` is `0` press, `1` repeat, `2` release. `composed` says
    /// alt went into producing `text` rather than modifying the key (`vt::input::Key`).
    Key {
        channel: u32,
        action: u8,
        modifiers: u16,
        code: String,
        text: String,
        unshifted: u32,
        composed: bool,
    },
    /// Text that did not come from a key.
    Text {
        channel: u32,
        text: String,
    },
    /// The pointer, in pixels from the grid's corner. `action` is `0` press, `1` release, `2` move, `3`
    /// wheel up, `4` wheel down.
    Pointer {
        channel: u32,
        action: u8,
        button: u8,
        modifiers: u16,
        x: f32,
        y: f32,
        cell_width: f32,
        cell_height: f32,
    },
    Paste {
        channel: u32,
        request: u32,
        allow_unsafe: bool,
        text: String,
    },
    Focus {
        channel: u32,
        gained: bool,
    },
    WordAt {
        channel: u32,
        request: u32,
        epoch: u32,
        at: Position,
    },
    LineAt {
        channel: u32,
        request: u32,
        epoch: u32,
        at: Position,
    },
    TextOf {
        channel: u32,
        request: u32,
        epoch: u32,
        span: Span,
    },
    Search {
        channel: u32,
        request: u32,
        needle: String,
    },
    /// The commands the shell has reported in this session.
    Commands {
        channel: u32,
        request: u32,
    },
    /// Where every image in this session is.
    Images {
        channel: u32,
        request: u32,
    },
    /// One image's pixels, from `offset` on. An image larger than one answer holds is asked for again
    /// from where the last answer ended.
    Image {
        channel: u32,
        request: u32,
        image: u32,
        offset: u32,
    },
}

/// What a view is sent.
#[derive(Debug, Clone, PartialEq)]
pub enum ToView {
    Frame(Frame),
    /// The answer to a `Fetch`, which covers the `count` lines from `first`: a line among them with no
    /// row is one the session does not hold — one that is gone, or one not written yet. Fewer than were
    /// asked for when the rest did not fit one message or one fetch; a line past them was not answered,
    /// and is asked for again.
    Rows {
        channel: u32,
        request: u32,
        epoch: u32,
        first: u64,
        count: u32,
        block: Block,
    },
    /// The session is over.
    Ended {
        channel: u32,
    },
    /// `0` pasted, `1` refused as unsafe.
    Pasted {
        channel: u32,
        request: u32,
        outcome: u8,
    },
    Span {
        channel: u32,
        request: u32,
        epoch: u32,
        span: Option<Span>,
    },
    /// The answer to a `TextOf`, with the epoch the span it was asked about was read in.
    Text {
        channel: u32,
        request: u32,
        epoch: u32,
        text: String,
    },
    Spans {
        channel: u32,
        request: u32,
        epoch: u32,
        spans: Vec<Span>,
    },
    /// The answer to `Commands`, oldest first.
    Commands {
        channel: u32,
        request: u32,
        epoch: u32,
        commands: Vec<CommandRow>,
    },
    /// The answer to `Images`: each image where it is, and one cell in pixels as the terminal was told
    /// it is — what an image's offset into its first cell is measured in. Zero when no window has said.
    Images {
        channel: u32,
        request: u32,
        epoch: u32,
        cell_width: u16,
        cell_height: u16,
        placements: Vec<Placement>,
    },
    /// The answer to `Image`: part of its pixels, rows top to bottom, `channels` bytes each pixel — `1`
    /// grey, `2` grey and alpha, `3` red green blue, `4` those and alpha. `total` is the whole image's
    /// size, and `offset` where these bytes begin in it.
    Image {
        channel: u32,
        request: u32,
        image: u32,
        generation: u64,
        width: u32,
        height: u32,
        channels: u8,
        total: u32,
        offset: u32,
        bytes: Vec<u8>,
    },
    /// A request that could not be answered, and why. An `Attach` that could not be carried out is
    /// answered with this too, as request `0`: it has no number of its own.
    Failed {
        channel: u32,
        request: u32,
        message: String,
    },
}

/// One image where a program put it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Placement {
    pub image: u32,
    /// Moves when the image's pixels do
    pub generation: u64,
    /// Orders images that overlap; below zero is under the text
    pub z: i32,
    /// The top-left cell
    pub line: u64,
    pub column: u16,
    /// How many cells it covers
    pub columns: u32,
    pub rows: u32,
    /// How far into its first cell it starts, and how large it is drawn, in pixels of the cell `Images`
    /// gives
    pub offset_x: u32,
    pub offset_y: u32,
    pub width: u32,
    pub height: u32,
    /// The part of the image drawn, in the image's own pixels
    pub source_x: u32,
    pub source_y: u32,
    pub source_width: u32,
    pub source_height: u32,
}

/// Why bytes were not a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Malformed {
    /// The payload ended before a field did
    Truncated,
    /// A kind byte no message has
    UnknownKind(u8),
    /// A string that is not UTF-8
    Text,
    /// An integer wider than its field
    Overflow,
    /// A tag or an index outside what the format defines
    Value(&'static str),
    /// Bytes left over after the last field
    Trailing,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(out, "the message ended before its last field"),
            Self::UnknownKind(kind) => write!(out, "no message has kind {kind}"),
            Self::Text => write!(out, "a string was not UTF-8"),
            Self::Overflow => write!(out, "an integer did not fit its field"),
            Self::Value(what) => write!(out, "{what} is outside what the format defines"),
            Self::Trailing => write!(out, "bytes followed the message's last field"),
        }
    }
}

/// What reading one message off a connection produced.
#[derive(Debug)]
pub enum Incoming {
    /// A message's kind and payload, not yet understood
    Message(Vec<u8>),
    /// Past the cap: where the next message starts is no longer known
    TooLong,
    /// Nothing more will arrive
    Ended,
}

/// Reads the next message's bytes — kind and payload — off a connection.
pub fn read_message<R: Read>(reader: &mut R) -> Incoming {
    let mut length = [0u8; 4];
    if reader.read_exact(&mut length).is_err() {
        return Incoming::Ended;
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Incoming::TooLong;
    }
    let mut body = vec![0u8; length];
    if reader.read_exact(&mut body).is_err() {
        return Incoming::Ended;
    }
    Incoming::Message(body)
}

/// A message for a view came to more than a view reads: how many bytes it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong(pub usize);

impl std::fmt::Display for TooLong {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "the answer is {} bytes, and a message carries {MAX_MESSAGE_BYTES} at most", self.0)
    }
}

/// A message for a view with its length in front, ready to be written — or how long it came to, when
/// that is more than a view reads.
///
/// **Refused rather than sent**: a view must hang up on a message past the cap, and would be sent the
/// same one again when it came back. Everything that grows with what a program printed is bounded
/// before it gets here — rows by `Rows::takes` and `ROW_LINK_BYTES`, a text by `Screen::text_of`, a
/// list of images by `main` — so this is what is left when one of those is wrong, and whoever asked is
/// told so instead of the broker stopping.
fn framed(kind: u8, write: impl FnOnce(&mut Writer)) -> Result<Vec<u8>, TooLong> {
    let message = framed_as_it_is(kind, write);
    let length = message.len() - 4;
    if length > MAX_MESSAGE_BYTES { Err(TooLong(length)) } else { Ok(message) }
}

/// A message with its length in front, however long it is. For what a view sends: the broker reads a
/// length before it reads a message, and refuses one past the cap there (`read_message`).
fn framed_as_it_is(kind: u8, write: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut writer = Writer { bytes: vec![0, 0, 0, 0, kind] };
    write(&mut writer);
    let length = u32::try_from(writer.bytes.len() - 4).unwrap_or(u32::MAX);
    writer.bytes[..4].copy_from_slice(&length.to_le_bytes());
    writer.bytes
}

fn number(out: &mut Vec<u8>, value: impl Into<u64>) {
    let mut value: u64 = value.into();
    loop {
        let low = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

fn text(out: &mut Vec<u8>, value: &str) {
    number(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn color(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Default => out.push(0),
        Color::Palette(index) => out.extend_from_slice(&[1, index]),
        Color::Rgb(r, g, b) => out.extend_from_slice(&[2, r, g, b]),
    }
}

struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    fn byte(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn flag(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    fn number(&mut self, value: impl Into<u64>) {
        number(&mut self.bytes, value);
    }

    fn float(&mut self, value: f32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn text(&mut self, value: &str) {
        text(&mut self.bytes, value);
    }

    fn position(&mut self, at: Position) {
        self.number(at.line);
        self.number(at.column);
    }

    /// A command, as a byte saying which of its fields it has and then those fields in order.
    fn command(&mut self, command: &CommandRow) {
        self.number(command.id);
        let has = [
            command.command.is_some(),
            command.cwd.is_some(),
            command.started_ms.is_some(),
            command.finished_ms.is_some(),
            command.exit_status.is_some(),
            command.prompt.is_some(),
            command.output.is_some(),
            command.end.is_some(),
        ];
        self.byte(has.iter().enumerate().fold(0u8, |flags, (bit, set)| flags | (u8::from(*set) << bit)));
        if let Some(text) = &command.command {
            self.text(text);
        }
        if let Some(text) = &command.cwd {
            self.text(text);
        }
        for time in [command.started_ms, command.finished_ms].into_iter().flatten() {
            self.number(time);
        }
        if let Some(status) = command.exit_status {
            // Zigzag, so a small negative status is as short as a small positive one
            self.number(((status << 1) ^ (status >> 31)) as u32);
        }
        for line in [command.prompt, command.output, command.end].into_iter().flatten() {
            self.number(line);
        }
    }

    fn span(&mut self, span: Span) {
        self.position(span.start);
        self.position(span.end);
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, Malformed> {
        let (first, rest) = self.bytes.split_first().ok_or(Malformed::Truncated)?;
        self.bytes = rest;
        Ok(*first)
    }

    fn flag(&mut self) -> Result<bool, Malformed> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Malformed::Value("a flag")),
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Malformed> {
        if self.bytes.len() < count {
            return Err(Malformed::Truncated);
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }

    fn number(&mut self) -> Result<u64, Malformed> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            let bits = u64::from(byte & 0x7F);
            // The tenth byte holds the one bit left; anything more does not fit
            if shift == 63 && bits > 1 {
                return Err(Malformed::Overflow);
            }
            value |= bits << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Malformed::Overflow)
    }

    fn u32(&mut self) -> Result<u32, Malformed> {
        u32::try_from(self.number()?).map_err(|_| Malformed::Overflow)
    }

    fn u16(&mut self) -> Result<u16, Malformed> {
        u16::try_from(self.number()?).map_err(|_| Malformed::Overflow)
    }

    /// A count of things that each take at least one byte, so a count the payload cannot hold is refused
    /// before anything is allocated for it
    fn count(&mut self) -> Result<usize, Malformed> {
        let count = usize::try_from(self.number()?).map_err(|_| Malformed::Overflow)?;
        if count > self.bytes.len() {
            return Err(Malformed::Truncated);
        }
        Ok(count)
    }

    fn float(&mut self) -> Result<f32, Malformed> {
        let bytes = self.take(4)?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn text(&mut self) -> Result<String, Malformed> {
        let length = self.count()?;
        let bytes = self.take(length)?;
        std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| Malformed::Text)
    }

    fn position(&mut self) -> Result<Position, Malformed> {
        Ok(Position { line: self.number()?, column: self.u16()? })
    }

    fn span(&mut self) -> Result<Span, Malformed> {
        Ok(Span { start: self.position()?, end: self.position()? })
    }

    fn command(&mut self) -> Result<CommandRow, Malformed> {
        let id = self.number()?;
        let has = self.byte()?;
        let mut command = CommandRow { id, ..CommandRow::default() };
        if has & 1 != 0 {
            command.command = Some(self.text()?);
        }
        if has & 2 != 0 {
            command.cwd = Some(self.text()?);
        }
        if has & 4 != 0 {
            command.started_ms = Some(self.number()?);
        }
        if has & 8 != 0 {
            command.finished_ms = Some(self.number()?);
        }
        if has & 16 != 0 {
            let zigzag = self.u32()?;
            command.exit_status = Some(((zigzag >> 1) as i32) ^ -((zigzag & 1) as i32));
        }
        if has & 32 != 0 {
            command.prompt = Some(self.number()?);
        }
        if has & 64 != 0 {
            command.output = Some(self.number()?);
        }
        if has & 128 != 0 {
            command.end = Some(self.number()?);
        }
        Ok(command)
    }

    fn color(&mut self) -> Result<Color, Malformed> {
        match self.byte()? {
            0 => Ok(Color::Default),
            1 => Ok(Color::Palette(self.byte()?)),
            2 => {
                let rgb = self.take(3)?;
                Ok(Color::Rgb(rgb[0], rgb[1], rgb[2]))
            }
            _ => Err(Malformed::Value("a colour's tag")),
        }
    }

    fn block(&mut self) -> Result<Block, Malformed> {
        let mut block = Block::default();
        for _ in 0..self.count()? {
            block.rows.push(self.row()?);
        }
        Ok(block)
    }

    fn row(&mut self) -> Result<Row, Malformed> {
        let line = self.number()?;
        let flags = self.byte()?;
        if flags > 3 {
            return Err(Malformed::Value("a row's flags"));
        }
        let mut styles = Vec::new();
        for _ in 0..self.count()? {
            styles.push(Style { fg: self.color()?, bg: self.color()?, underline: self.color()?, flags: self.u16()? });
        }
        let mut links = Vec::new();
        for _ in 0..self.count()? {
            links.push(self.text()?);
        }
        let mut glyphs = Vec::new();
        for _ in 0..self.count()? {
            let mut column = self.u16()?;
            let style = self.u16()?;
            let link = self.u16()?;
            if usize::from(style) > styles.len() {
                return Err(Malformed::Value("a style's index"));
            }
            if usize::from(link) > links.len() {
                return Err(Malformed::Value("a link's index"));
            }
            match self.byte()? {
                0 => {
                    for character in self.text()?.chars() {
                        glyphs.push(Glyph { column, width: 1, text: character.to_string(), style, link });
                        column = column.checked_add(1).ok_or(Malformed::Overflow)?;
                    }
                }
                1 => {
                    for _ in 0..self.count()? {
                        let width = self.byte()?;
                        let text = self.text()?;
                        glyphs.push(Glyph { column, width, text, style, link });
                        column = column.checked_add(u16::from(width)).ok_or(Malformed::Overflow)?;
                    }
                }
                _ => return Err(Malformed::Value("a run's kind")),
            }
        }
        Ok(Row { line, wrapped: flags & 1 != 0, prompt: flags & 2 != 0, styles, links, glyphs })
    }

    fn finish<T>(self, value: T) -> Result<T, Malformed> {
        if self.bytes.is_empty() { Ok(value) } else { Err(Malformed::Trailing) }
    }
}

mod kind {
    pub const ATTACH: u8 = 1;
    pub const DETACH: u8 = 2;
    pub const ACKNOWLEDGE: u8 = 3;
    pub const FETCH: u8 = 4;
    pub const KEY: u8 = 5;
    pub const TEXT_INPUT: u8 = 6;
    pub const POINTER: u8 = 7;
    pub const PASTE: u8 = 8;
    pub const FOCUS: u8 = 9;
    pub const WORD_AT: u8 = 10;
    pub const LINE_AT: u8 = 11;
    pub const TEXT_OF: u8 = 12;
    pub const SEARCH: u8 = 13;
    pub const COMMANDS: u8 = 14;
    pub const IMAGES: u8 = 15;
    pub const IMAGE: u8 = 16;

    pub const FRAME: u8 = 65;
    pub const ROWS: u8 = 66;
    pub const ENDED: u8 = 67;
    pub const PASTED: u8 = 68;
    pub const SPAN: u8 = 69;
    pub const TEXT: u8 = 70;
    pub const SPANS: u8 = 71;
    pub const FAILED: u8 = 72;
    pub const COMMAND_LIST: u8 = 73;
    pub const IMAGE_LIST: u8 = 74;
    pub const IMAGE_PIXELS: u8 = 75;
}

const FRAME_CURSOR: u8 = 1;
const FRAME_ALTERNATE: u8 = 2;
const FRAME_MOUSE: u8 = 4;
const FRAME_STALE: u8 = 8;
const FRAME_BLINKING: u8 = 16;
const FRAME_REVERSE: u8 = 32;
const FRAME_COLORS: u8 = 64;

const COLOR_FOREGROUND: u8 = 1;
const COLOR_BACKGROUND: u8 = 2;
const COLOR_CURSOR: u8 = 4;

impl ToBroker {
    /// The message with its length in front.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Attach { channel, session } => framed_as_it_is(kind::ATTACH, |out| {
                out.number(*channel);
                out.text(session);
            }),
            Self::Detach { channel } => framed_as_it_is(kind::DETACH, |out| out.number(*channel)),
            Self::Acknowledge { channel, sequence } => framed_as_it_is(kind::ACKNOWLEDGE, |out| {
                out.number(*channel);
                out.number(*sequence);
            }),
            Self::Fetch { channel, request, epoch, first, count } => framed_as_it_is(kind::FETCH, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.number(*first);
                out.number(*count);
            }),
            Self::Key { channel, action, modifiers, code, text, unshifted, composed } => {
                framed_as_it_is(kind::KEY, |out| {
                    out.number(*channel);
                    out.byte(*action);
                    out.number(*modifiers);
                    out.text(code);
                    out.text(text);
                    out.number(*unshifted);
                    out.flag(*composed);
                })
            }
            Self::Text { channel, text } => framed_as_it_is(kind::TEXT_INPUT, |out| {
                out.number(*channel);
                out.text(text);
            }),
            Self::Pointer { channel, action, button, modifiers, x, y, cell_width, cell_height } => {
                framed_as_it_is(kind::POINTER, |out| {
                    out.number(*channel);
                    out.byte(*action);
                    out.byte(*button);
                    out.number(*modifiers);
                    out.float(*x);
                    out.float(*y);
                    out.float(*cell_width);
                    out.float(*cell_height);
                })
            }
            Self::Paste { channel, request, allow_unsafe, text } => framed_as_it_is(kind::PASTE, |out| {
                out.number(*channel);
                out.number(*request);
                out.flag(*allow_unsafe);
                out.text(text);
            }),
            Self::Focus { channel, gained } => framed_as_it_is(kind::FOCUS, |out| {
                out.number(*channel);
                out.flag(*gained);
            }),
            Self::WordAt { channel, request, epoch, at } => framed_as_it_is(kind::WORD_AT, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.position(*at);
            }),
            Self::LineAt { channel, request, epoch, at } => framed_as_it_is(kind::LINE_AT, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.position(*at);
            }),
            Self::TextOf { channel, request, epoch, span } => framed_as_it_is(kind::TEXT_OF, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.span(*span);
            }),
            Self::Search { channel, request, needle } => framed_as_it_is(kind::SEARCH, |out| {
                out.number(*channel);
                out.number(*request);
                out.text(needle);
            }),
            Self::Commands { channel, request } => framed_as_it_is(kind::COMMANDS, |out| {
                out.number(*channel);
                out.number(*request);
            }),
            Self::Images { channel, request } => framed_as_it_is(kind::IMAGES, |out| {
                out.number(*channel);
                out.number(*request);
            }),
            Self::Image { channel, request, image, offset } => framed_as_it_is(kind::IMAGE, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*image);
                out.number(*offset);
            }),
        }
    }

    /// A message from its kind and payload, which is what `read_message` hands back.
    pub fn decode(body: &[u8]) -> Result<Self, Malformed> {
        let mut read = Reader { bytes: body };
        let message = match read.byte()? {
            kind::ATTACH => Self::Attach { channel: read.u32()?, session: read.text()? },
            kind::DETACH => Self::Detach { channel: read.u32()? },
            kind::ACKNOWLEDGE => Self::Acknowledge { channel: read.u32()?, sequence: read.number()? },
            kind::FETCH => Self::Fetch {
                channel: read.u32()?,
                request: read.u32()?,
                epoch: read.u32()?,
                first: read.number()?,
                count: read.u32()?,
            },
            kind::KEY => Self::Key {
                channel: read.u32()?,
                action: read.byte()?,
                modifiers: read.u16()?,
                code: read.text()?,
                text: read.text()?,
                unshifted: read.u32()?,
                composed: read.flag()?,
            },
            kind::TEXT_INPUT => Self::Text { channel: read.u32()?, text: read.text()? },
            kind::POINTER => Self::Pointer {
                channel: read.u32()?,
                action: read.byte()?,
                button: read.byte()?,
                modifiers: read.u16()?,
                x: read.float()?,
                y: read.float()?,
                cell_width: read.float()?,
                cell_height: read.float()?,
            },
            kind::PASTE => Self::Paste {
                channel: read.u32()?,
                request: read.u32()?,
                allow_unsafe: read.flag()?,
                text: read.text()?,
            },
            kind::FOCUS => Self::Focus { channel: read.u32()?, gained: read.flag()? },
            kind::WORD_AT => {
                Self::WordAt { channel: read.u32()?, request: read.u32()?, epoch: read.u32()?, at: read.position()? }
            }
            kind::LINE_AT => {
                Self::LineAt { channel: read.u32()?, request: read.u32()?, epoch: read.u32()?, at: read.position()? }
            }
            kind::TEXT_OF => {
                Self::TextOf { channel: read.u32()?, request: read.u32()?, epoch: read.u32()?, span: read.span()? }
            }
            kind::SEARCH => Self::Search { channel: read.u32()?, request: read.u32()?, needle: read.text()? },
            kind::COMMANDS => Self::Commands { channel: read.u32()?, request: read.u32()? },
            kind::IMAGES => Self::Images { channel: read.u32()?, request: read.u32()? },
            kind::IMAGE => {
                Self::Image { channel: read.u32()?, request: read.u32()?, image: read.u32()?, offset: read.u32()? }
            }
            other => return Err(Malformed::UnknownKind(other)),
        };
        read.finish(message)
    }
}

impl ToView {
    /// The message with its length in front.
    pub fn encode(&self) -> Result<Vec<u8>, TooLong> {
        match self {
            Self::Frame(sent) => frame(
                &FrameHead {
                    channel: sent.channel,
                    sequence: sent.sequence,
                    epoch: sent.epoch,
                    first_active: sent.first_active,
                    columns: sent.columns,
                    screen_rows: sent.screen_rows,
                    scrollback: sent.scrollback,
                    commands: sent.commands,
                    images: sent.images,
                    cursor: sent.cursor,
                    alternate_screen: sent.alternate_screen,
                    mouse: sent.mouse,
                    reverse_video: sent.reverse_video,
                    stale_from: sent.stale_from,
                    colors: sent.colors.as_ref(),
                },
                &Rows::of(&sent.block),
            ),
            Self::Rows { channel, request, epoch, first, count, block } => {
                rows(*channel, *request, *epoch, *first, *count, &Rows::of(block))
            }
            Self::Ended { channel } => framed(kind::ENDED, |out| out.number(*channel)),
            Self::Pasted { channel, request, outcome } => framed(kind::PASTED, |out| {
                out.number(*channel);
                out.number(*request);
                out.byte(*outcome);
            }),
            Self::Span { channel, request, epoch, span } => framed(kind::SPAN, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.flag(span.is_some());
                if let Some(span) = span {
                    out.span(*span);
                }
            }),
            Self::Text { channel, request, epoch, text } => framed(kind::TEXT, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.text(text);
            }),
            Self::Spans { channel, request, epoch, spans } => framed(kind::SPANS, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.number(spans.len() as u64);
                for span in spans {
                    out.span(*span);
                }
            }),
            Self::Failed { channel, request, message } => framed(kind::FAILED, |out| {
                out.number(*channel);
                out.number(*request);
                out.text(message);
            }),
            Self::Commands { channel, request, epoch, commands } => framed(kind::COMMAND_LIST, |out| {
                out.number(*channel);
                out.number(*request);
                out.number(*epoch);
                out.number(commands.len() as u64);
                for command in commands {
                    out.command(command);
                }
            }),
            Self::Images { channel, request, epoch, cell_width, cell_height, placements } => {
                framed(kind::IMAGE_LIST, |out| {
                    out.number(*channel);
                    out.number(*request);
                    out.number(*epoch);
                    out.number(*cell_width);
                    out.number(*cell_height);
                    out.number(placements.len() as u64);
                    for placement in placements {
                        out.number(placement.image);
                        out.number(placement.generation);
                        // Zigzag, so a small depth either side of zero is one byte
                        out.number(((placement.z << 1) ^ (placement.z >> 31)) as u32);
                        out.number(placement.line);
                        out.number(placement.column);
                        out.number(placement.columns);
                        out.number(placement.rows);
                        out.number(placement.offset_x);
                        out.number(placement.offset_y);
                        out.number(placement.width);
                        out.number(placement.height);
                        out.number(placement.source_x);
                        out.number(placement.source_y);
                        out.number(placement.source_width);
                        out.number(placement.source_height);
                    }
                })
            }
            Self::Image { channel, request, image, generation, width, height, channels, total, offset, bytes } => {
                framed(kind::IMAGE_PIXELS, |out| {
                    out.number(*channel);
                    out.number(*request);
                    out.number(*image);
                    out.number(*generation);
                    out.number(*width);
                    out.number(*height);
                    out.byte(*channels);
                    out.number(*total);
                    out.number(*offset);
                    out.number(bytes.len() as u64);
                    out.bytes.extend_from_slice(bytes);
                })
            }
        }
    }

    /// A message from its kind and payload.
    pub fn decode(body: &[u8]) -> Result<Self, Malformed> {
        let mut read = Reader { bytes: body };
        let message = match read.byte()? {
            kind::FRAME => {
                let channel = read.u32()?;
                let sequence = read.number()?;
                let epoch = read.u32()?;
                let first_active = read.number()?;
                let columns = read.u16()?;
                let screen_rows = read.u16()?;
                let scrollback = read.u32()?;
                let commands = read.u32()?;
                let images = read.u32()?;
                let flags = read.byte()?;
                if flags >= 128 {
                    return Err(Malformed::Value("a frame's flags"));
                }
                let cursor = if flags & FRAME_CURSOR != 0 {
                    Some(Cursor {
                        column: read.u16()?,
                        row: read.u16()?,
                        shape: read.byte()?,
                        blinking: flags & FRAME_BLINKING != 0,
                    })
                } else {
                    None
                };
                let stale_from = if flags & FRAME_STALE != 0 { Some(read.number()?) } else { None };
                let colors = if flags & FRAME_COLORS != 0 {
                    let set = read.byte()?;
                    if set >= 8 {
                        return Err(Malformed::Value("which colours are set"));
                    }
                    let mut rgb = |is: u8| -> Result<Option<[u8; 3]>, Malformed> {
                        if set & is == 0 {
                            return Ok(None);
                        }
                        let rgb = read.take(3)?;
                        Ok(Some([rgb[0], rgb[1], rgb[2]]))
                    };
                    let (foreground, background, cursor) =
                        (rgb(COLOR_FOREGROUND)?, rgb(COLOR_BACKGROUND)?, rgb(COLOR_CURSOR)?);
                    let mut palette = Vec::new();
                    for _ in 0..read.count()? {
                        let entry = read.take(4)?;
                        palette.push((entry[0], [entry[1], entry[2], entry[3]]));
                    }
                    Some(ColorChanges { foreground, background, cursor, palette })
                } else {
                    None
                };
                Self::Frame(Frame {
                    channel,
                    sequence,
                    epoch,
                    first_active,
                    columns,
                    screen_rows,
                    scrollback,
                    commands,
                    images,
                    cursor,
                    alternate_screen: flags & FRAME_ALTERNATE != 0,
                    mouse: flags & FRAME_MOUSE != 0,
                    reverse_video: flags & FRAME_REVERSE != 0,
                    stale_from,
                    colors,
                    block: read.block()?,
                })
            }
            kind::ROWS => Self::Rows {
                channel: read.u32()?,
                request: read.u32()?,
                epoch: read.u32()?,
                first: read.number()?,
                count: read.u32()?,
                block: read.block()?,
            },
            kind::ENDED => Self::Ended { channel: read.u32()? },
            kind::PASTED => Self::Pasted { channel: read.u32()?, request: read.u32()?, outcome: read.byte()? },
            kind::SPAN => {
                let (channel, request, epoch) = (read.u32()?, read.u32()?, read.u32()?);
                let span = if read.flag()? { Some(read.span()?) } else { None };
                Self::Span { channel, request, epoch, span }
            }
            kind::TEXT => {
                Self::Text { channel: read.u32()?, request: read.u32()?, epoch: read.u32()?, text: read.text()? }
            }
            kind::SPANS => {
                let (channel, request, epoch) = (read.u32()?, read.u32()?, read.u32()?);
                let mut spans = Vec::new();
                // Four numbers a span, each at least a byte
                for _ in 0..read.count()? {
                    spans.push(read.span()?);
                }
                Self::Spans { channel, request, epoch, spans }
            }
            kind::FAILED => Self::Failed { channel: read.u32()?, request: read.u32()?, message: read.text()? },
            kind::COMMAND_LIST => {
                let (channel, request, epoch) = (read.u32()?, read.u32()?, read.u32()?);
                let mut commands = Vec::new();
                // An id and a byte of flags, at the least
                for _ in 0..read.count()? {
                    commands.push(read.command()?);
                }
                Self::Commands { channel, request, epoch, commands }
            }
            kind::IMAGE_LIST => {
                let (channel, request, epoch) = (read.u32()?, read.u32()?, read.u32()?);
                let (cell_width, cell_height) = (read.u16()?, read.u16()?);
                let count = read.count()?;
                let mut placements = Vec::new();
                for _ in 0..count {
                    let (image, generation) = (read.u32()?, read.number()?);
                    let zigzag = read.u32()?;
                    placements.push(Placement {
                        image,
                        generation,
                        z: ((zigzag >> 1) as i32) ^ -((zigzag & 1) as i32),
                        line: read.number()?,
                        column: read.u16()?,
                        columns: read.u32()?,
                        rows: read.u32()?,
                        offset_x: read.u32()?,
                        offset_y: read.u32()?,
                        width: read.u32()?,
                        height: read.u32()?,
                        source_x: read.u32()?,
                        source_y: read.u32()?,
                        source_width: read.u32()?,
                        source_height: read.u32()?,
                    });
                }
                Self::Images { channel, request, epoch, cell_width, cell_height, placements }
            }
            kind::IMAGE_PIXELS => {
                let (channel, request, image) = (read.u32()?, read.u32()?, read.u32()?);
                let generation = read.number()?;
                let (width, height) = (read.u32()?, read.u32()?);
                let channels = read.byte()?;
                if !(1..=4).contains(&channels) {
                    return Err(Malformed::Value("an image's channels"));
                }
                let (total, offset) = (read.u32()?, read.u32()?);
                let length = read.count()?;
                let bytes = read.take(length)?.to_vec();
                Self::Image { channel, request, image, generation, width, height, channels, total, offset, bytes }
            }
            other => return Err(Malformed::UnknownKind(other)),
        };
        read.finish(message)
    }
}

/// The body of an encoded message: what `read_message` would hand back for it.
pub fn body(encoded: &[u8]) -> &[u8] {
    encoded.get(4..).unwrap_or_default()
}
