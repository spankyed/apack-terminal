//! What each view of a session has been sent, and what it is owed.
//!
//! **A frame is a change, and it is made for one view.** The broker keeps the screen as it last read it,
//! with the point at which each row last changed; a view is behind by however much changed since the frame
//! it was sent, and its next frame is exactly that. So two views of one session are each sent their own
//! difference, and a view that draws slowly is sent fewer, larger frames rather than a queue of small ones.
//!
//! **Paced by acknowledgement.** A view is sent a frame only when it has said it drew the last. Between
//! the two nothing is read out of the terminal for it at all: a program writing flat out costs the parse
//! and nothing else, however many lines scroll past.
//!
//! **Lines are numbered absolutely**, so a view can keep what it was sent. `first_active` is the line the
//! screen's first row is; it advances by exactly the lines that scrolled into history, which is counted
//! with a mark the core moves with its row (`vt::rows::Mark`). Whatever makes every number meaningless —
//! a resize, which reflows; a switch between the two screens; more scrolling than the history holds
//! between two looks — starts a new `epoch`, and a view forgets its rows.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use super::wire::{Block, Cursor, Frame, Glyph, Row, ToView};
use crate::vt::rows::{Mark, Rendering, RowRead, Space};
use crate::vt::{Failed, Terminal};

/// Synchronized output: a program asks for the screen to be held while it redraws.
const SYNCHRONIZED_OUTPUT: u16 = 2026;

/// The longest a program may hold the screen. One that asks and never lets go is drawn anyway.
pub const HOLD_LIMIT: Duration = Duration::from_secs(1);

/// The most lines one fetch answers with, whatever was asked for.
pub const FETCH_LINES: u32 = 2_000;

/// Which view: the connection it arrived on, and the number that connection gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewId {
    pub connection: u64,
    pub channel: u32,
}

/// What a view was last sent.
#[derive(Debug, Clone, Copy)]
struct Sent {
    epoch: u32,
    sequence: u64,
    first_active: u64,
}

struct Viewer {
    id: ViewId,
    sink: Sender<Vec<u8>>,
    sent: Option<Sent>,
    /// A frame has gone and its acknowledgement has not come back
    in_flight: bool,
    /// Its connection's writer has ended, so nothing sent to it arrives
    gone: bool,
}

struct Held {
    row: RowRead,
    /// The sequence at which this row last differed from what was there before
    changed: u64,
}

/// One session's screen as its views are told about it.
pub struct Publication {
    mark: Mark,
    rendering: Rendering,
    epoch: u32,
    /// The line the screen's first row is, as of the last look at the terminal
    first_active: u64,
    /// `first_active` when `active` was read, which is what its indices are relative to
    captured_first: u64,
    sequence: u64,
    columns: u16,
    rows: u16,
    alternate: bool,
    scrollback: u32,
    /// `scrollback` when `active` was read
    captured_scrollback: u32,
    active: Vec<Held>,
    cursor: Option<Cursor>,
    mouse: bool,
    /// The shell's command generation when the screen was last read
    commands: u32,
    /// The image storage's generation when the screen was last read, as much of it as a view compares
    images: u32,
    /// Bytes have been fed since the terminal was last looked at
    stale: bool,
    /// When the program was first seen holding the screen
    held_since: Option<Instant>,
    viewers: Vec<Viewer>,
}

impl Publication {
    pub fn new(terminal: &Terminal) -> Result<Self, Failed> {
        let scrollback = terminal.scrollback_rows()?;
        Ok(Self {
            mark: Mark::at_active_top(terminal)?,
            rendering: Rendering::new()?,
            epoch: 0,
            first_active: u64::from(scrollback),
            captured_first: u64::from(scrollback),
            sequence: 0,
            columns: terminal.cols()?,
            rows: terminal.rows()?,
            alternate: terminal.alt_screen()?,
            scrollback,
            captured_scrollback: scrollback,
            active: Vec::new(),
            cursor: None,
            mouse: false,
            commands: 0,
            images: 0,
            stale: true,
            held_since: None,
            viewers: Vec::new(),
        })
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn first_active(&self) -> u64 {
        self.first_active
    }

    pub fn has_viewers(&self) -> bool {
        !self.viewers.is_empty()
    }

    /// The terminal has been written to or resized: what is held here may no longer be what it shows.
    pub fn touch(&mut self) {
        self.stale = true;
    }

    /// Brings the line numbering up to the terminal, without reading a row.
    ///
    /// Everything that names a line calls this first, since a line's place in the terminal is counted
    /// from where the screen starts *now*.
    pub fn advance(&mut self, terminal: &Terminal) -> Result<(), Failed> {
        let scrollback = terminal.scrollback_rows()?;
        let (columns, rows, alternate) = (terminal.cols()?, terminal.rows()?, terminal.alt_screen()?);
        let marked = self.mark.screen_row();
        let reset = columns != self.columns || rows != self.rows || alternate != self.alternate || marked.is_none();
        self.scrollback = scrollback;
        if reset {
            (self.columns, self.rows, self.alternate) = (columns, rows, alternate);
            self.epoch = self.epoch.wrapping_add(1);
            self.first_active = u64::from(scrollback);
            self.captured_first = self.first_active;
            self.active.clear();
            // A new epoch is a change in itself: a view on the old one must be told
            self.sequence += 1;
        } else if let Some(marked) = marked {
            // The mark was on the screen's first row; it is now that many rows up the history. A row
            // that moved *down* — a program scrolling its screen the other way — moved no line into
            // history, and the rows are compared by content either way
            self.first_active += u64::from(scrollback.saturating_sub(marked));
        }
        self.mark.reset(terminal)
    }

    /// Reads the screen and notes what changed. Answers whether a program is holding it instead.
    fn capture(&mut self, terminal: &Terminal, commands: u32) -> Result<bool, Failed> {
        self.advance(terminal)?;
        if terminal.mode(SYNCHRONIZED_OUTPUT)? {
            let since = *self.held_since.get_or_insert_with(Instant::now);
            if since.elapsed() < HOLD_LIMIT {
                return Ok(true);
            }
        } else {
            self.held_since = None;
        }

        let next = self.sequence + 1;
        let shift = usize::try_from(self.first_active - self.captured_first).unwrap_or(usize::MAX);
        // History that shrank with nothing on the screen moving — a program erasing it — is a change a
        // view has to hear of: it is holding lines that are gone
        let mut changed = shift > 0 || self.scrollback != self.captured_scrollback;
        self.captured_scrollback = self.scrollback;
        let mut before: Vec<Option<Held>> = std::mem::take(&mut self.active).into_iter().map(Some).collect();
        for index in 0..self.rows {
            let row = terminal.read_row(Space::Active, u32::from(index))?.unwrap_or_default();
            let kept = shift
                .checked_add(usize::from(index))
                .and_then(|was| before.get_mut(was))
                .and_then(Option::take)
                .filter(|held| held.row == row);
            match kept {
                Some(held) => self.active.push(held),
                None => {
                    changed = true;
                    self.active.push(Held { row, changed: next });
                }
            }
        }
        self.captured_first = self.first_active;

        let cursor = self.rendering.cursor(terminal)?.map(|read| Cursor {
            column: read.column,
            row: read.row,
            shape: read.shape,
            blinking: read.blinking,
        });
        let mouse = terminal.mouse_tracking()?;
        // Compared and never ordered, so the low half is all of it a view needs
        let images = terminal.images_generation() as u32;
        if cursor != self.cursor || mouse != self.mouse || commands != self.commands || images != self.images {
            changed = true;
        }
        (self.cursor, self.mouse, self.commands, self.images) = (cursor, mouse, commands, images);
        if changed {
            self.sequence = next;
        }
        self.stale = false;
        Ok(false)
    }

    /// Sends every view that has drawn its last frame the one it is owed, if it is owed one.
    ///
    /// `commands` is the shell's command generation, which is a change a view is told of like any other.
    pub fn pump(&mut self, terminal: &Terminal, commands: u32) -> Result<(), Failed> {
        let waiting = |viewer: &Viewer| !viewer.in_flight;
        if !self.viewers.iter().any(waiting) {
            return Ok(());
        }
        if self.stale && self.capture(terminal, commands)? {
            return Ok(());
        }
        for index in 0..self.viewers.len() {
            let viewer = &self.viewers[index];
            let behind = viewer.sent.is_none_or(|sent| sent.epoch != self.epoch || sent.sequence != self.sequence);
            if viewer.in_flight || !behind {
                continue;
            }
            let frame = self.frame_for(terminal, viewer.id.channel, viewer.sent)?;
            self.send(index, frame);
        }
        // A view whose connection has gone is one nothing will acknowledge for
        self.viewers.retain(|viewer| !viewer.gone);
        Ok(())
    }

    fn send(&mut self, index: usize, frame: Frame) {
        let viewer = &mut self.viewers[index];
        viewer.sent = Some(Sent { epoch: frame.epoch, sequence: frame.sequence, first_active: frame.first_active });
        viewer.in_flight = true;
        viewer.gone = viewer.sink.send(ToView::Frame(frame).encode()).is_err();
    }

    /// The frame that takes a view from what it was sent to the screen as held.
    fn frame_for(&self, terminal: &Terminal, channel: u32, sent: Option<Sent>) -> Result<Frame, Failed> {
        let sent = sent.filter(|sent| sent.epoch == self.epoch);
        let mut build = Building::default();
        let mut stale_from = None;

        // **The lines that left the screen since.** A view holds them as they were when it was last sent
        // the screen, and a line is often written to once more before it scrolls — the one being typed
        // on always is. A few are sent as they ended up; a flood of them is named instead, since a view
        // showing the screen has no use for a thousand lines that passed between two of its frames.
        if let Some(sent) = sent
            && sent.first_active < self.first_active
        {
            let left = self.first_active - sent.first_active;
            if left <= u64::from(self.rows) && left <= u64::from(self.scrollback) {
                for line in sent.first_active..self.first_active {
                    if let Some(row) = self.read_line(terminal, line)? {
                        build.push(line, &row);
                    }
                }
            } else {
                stale_from = Some(sent.first_active);
            }
        }
        for (index, held) in self.active.iter().enumerate() {
            if sent.is_none_or(|sent| held.changed > sent.sequence) {
                build.push(self.first_active + index as u64, &held.row);
            }
        }
        Ok(Frame {
            channel,
            sequence: self.sequence,
            epoch: self.epoch,
            first_active: self.first_active,
            columns: self.columns,
            screen_rows: self.rows,
            scrollback: self.scrollback,
            commands: self.commands,
            images: self.images,
            cursor: self.cursor,
            alternate_screen: self.alternate,
            mouse: self.mouse,
            stale_from,
            block: build.block,
        })
    }

    /// Where a line is in the terminal now, counted from the oldest line held, or `None` for a line that
    /// is no longer held or not yet written. Only right after `advance`.
    pub fn screen_row(&self, line: u64) -> Option<u32> {
        let oldest = self.first_active.checked_sub(u64::from(self.scrollback))?;
        let row = line.checked_sub(oldest)?;
        (row < u64::from(self.scrollback) + u64::from(self.rows)).then(|| u32::try_from(row).ok()).flatten()
    }

    /// The line a row of the terminal is, the other way about.
    pub fn line_of(&self, screen_row: u32) -> u64 {
        self.first_active.saturating_sub(u64::from(self.scrollback)) + u64::from(screen_row)
    }

    fn read_line(&self, terminal: &Terminal, line: u64) -> Result<Option<RowRead>, Failed> {
        match self.screen_row(line) {
            Some(row) => terminal.read_row(Space::Screen, row),
            None => Ok(None),
        }
    }

    /// The lines a view asked for, as the terminal holds them now. Lines it no longer holds are absent.
    pub fn fetch(&mut self, terminal: &Terminal, first: u64, count: u32) -> Result<Block, Failed> {
        self.advance(terminal)?;
        let mut build = Building::default();
        for line in first..first.saturating_add(u64::from(count.min(FETCH_LINES))) {
            if let Some(row) = self.read_line(terminal, line)? {
                // What does not fit is left out, which a view reads as not yet fetched and asks for again
                if !build.fits(&row) {
                    break;
                }
                build.push(line, &row);
            }
        }
        Ok(build.block)
    }

    /// Starts sending a view this session's frames, beginning with the whole screen.
    pub fn attach(
        &mut self,
        terminal: &Terminal,
        commands: u32,
        id: ViewId,
        sink: Sender<Vec<u8>>,
    ) -> Result<(), Failed> {
        self.viewers.retain(|viewer| viewer.id != id);
        self.viewers.push(Viewer { id, sink, sent: None, in_flight: false, gone: false });
        self.stale = true;
        self.pump(terminal, commands)
    }

    pub fn detach(&mut self, id: ViewId) {
        self.viewers.retain(|viewer| viewer.id != id);
    }

    /// Every view a connection had, when the connection goes.
    pub fn detach_connection(&mut self, connection: u64) {
        self.viewers.retain(|viewer| viewer.id.connection != connection);
    }

    /// A view drew the frame with this sequence, so it may be sent the next.
    ///
    /// An acknowledgement of anything but the frame in flight is ignored: it is late, for a frame since
    /// superseded, and the one in flight has still to be drawn.
    pub fn acknowledge(&mut self, terminal: &Terminal, commands: u32, id: ViewId, sequence: u64) -> Result<(), Failed> {
        if let Some(viewer) = self.viewers.iter_mut().find(|viewer| viewer.id == id)
            && viewer.sent.is_some_and(|sent| sent.sequence == sequence)
        {
            viewer.in_flight = false;
        }
        self.pump(terminal, commands)
    }

    /// The session is over: every view is sent the screen as it ended, whether or not it had drawn its
    /// last frame, and told there will be no more.
    pub fn end(&mut self, terminal: &Terminal, commands: u32) {
        self.held_since = Some(Instant::now() - HOLD_LIMIT);
        self.stale = true;
        let captured = self.capture(terminal, commands).is_ok();
        for index in 0..self.viewers.len() {
            let (channel, sent) = (self.viewers[index].id.channel, self.viewers[index].sent);
            let behind = sent.is_none_or(|sent| sent.epoch != self.epoch || sent.sequence != self.sequence);
            if captured
                && behind
                && let Ok(frame) = self.frame_for(terminal, channel, sent)
            {
                self.send(index, frame);
            }
            let _ = self.viewers[index].sink.send(ToView::Ended { channel }.encode());
        }
        self.viewers.clear();
    }
}

/// A block being put together: rows, and the tables their styles and links are interned into.
#[derive(Default)]
struct Building {
    block: Block,
    styles: HashMap<super::wire::Style, u16>,
    links: HashMap<String, u16>,
}

impl Building {
    /// Whether a row's styles and links can all be given an index, which is sixteen bits wide.
    ///
    /// Counted as if every cell brought a new one of each, so it never says yes wrongly.
    fn fits(&self, row: &RowRead) -> bool {
        let room = usize::from(u16::MAX);
        self.block.styles.len() + row.cells.len() <= room && self.block.links.len() + row.cells.len() <= room
    }

    /// Adds a row. A style or link past the last index is drawn plain and unlinked: a screen with more
    /// than sixty-five thousand distinct styles on it is one no program draws, and `fetch` never gets
    /// there (`fits`).
    fn push(&mut self, line: u64, row: &RowRead) {
        let mut glyphs = Vec::with_capacity(row.cells.len());
        for cell in &row.cells {
            let style = if cell.style == super::wire::Style::default() {
                0
            } else if let Some(index) = self.styles.get(&cell.style) {
                *index
            } else if let Ok(index) = u16::try_from(self.block.styles.len()) {
                self.block.styles.push(cell.style);
                self.styles.insert(cell.style, index);
                index
            } else {
                0
            };
            let link = match &cell.link {
                None => 0,
                Some(link) => match (self.links.get(link), u16::try_from(self.block.links.len())) {
                    (Some(index), _) => *index,
                    (None, Ok(index)) => {
                        self.block.links.push(link.clone());
                        self.links.insert(link.clone(), index);
                        index
                    }
                    (None, Err(_)) => 0,
                },
            };
            glyphs.push(Glyph { column: cell.column, width: cell.width, text: cell.text.clone(), style, link });
        }
        self.block.rows.push(Row { line, wrapped: row.wrapped, prompt: row.prompt, glyphs });
    }
}
