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
//! **A row is written out once.** The screen is held as the bytes each row is sent as (`wire::write_row`),
//! so a row is compared with the one it replaces as bytes, and a frame for a view is those bytes put one
//! after another — the same ones for every view owed that row.
//!
//! **Lines are numbered absolutely**, so a view can keep what it was sent. `first_active` is the line the
//! screen's first row is; it advances by exactly the lines that scrolled into history, which is counted
//! with a mark the core moves with its row (`vt::rows::Mark`). Whatever makes every number meaningless —
//! a resize, which reflows; a switch between the two screens; more scrolling than the history holds
//! between two looks — starts a new `epoch`, and a view forgets its rows.

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;

use super::wire::{self, ColorChanges, Cursor, FrameHead, Rows, ToView};
use crate::vt::rows::{Mark, Rendering, RowRead, Space};
use crate::vt::{Failed, Terminal};

/// DEC private mode 5: the screen is drawn with its default foreground and background swapped.
const REVERSE_VIDEO: u16 = 5;

/// The most lines one fetch answers with, whatever was asked for.
pub const FETCH_LINES: u32 = 2_000;

/// Which view: the connection it arrived on, and the number that connection gave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewId {
    pub connection: u64,
    pub channel: u32,
}

/// Where a view's messages go.
pub trait Outlet: Send {
    /// Hands a message over. `false` when nothing handed over will arrive any more.
    fn deliver(&self, message: Vec<u8>) -> bool;
}

impl Outlet for Sender<Vec<u8>> {
    fn deliver(&self, message: Vec<u8>) -> bool {
        self.send(message).is_ok()
    }
}

/// What a view was last sent.
#[derive(Debug, Clone, Copy)]
struct Sent {
    epoch: u32,
    /// The sequence of the frame, which is what an acknowledgement names
    sequence: u64,
    /// The first line the view may not have as it ended up: everything that scrolled off the screen
    /// before this it was sent, or told was stale
    first_active: u64,
    /// The part of that frame that did not fit it, when there was one
    rest: Option<Rest>,
}

/// The rows a frame had no room for: every line from `from` on is as the view had it at `seen`.
#[derive(Debug, Clone, Copy)]
struct Rest {
    from: u64,
    seen: u64,
}

impl Sent {
    /// The sequence up to which the view has what was on `line`.
    fn seen(sent: Option<Self>, line: u64) -> u64 {
        match sent {
            None => 0,
            Some(Self { rest: Some(rest), .. }) if line >= rest.from => rest.seen,
            Some(sent) => sent.sequence,
        }
    }
}

struct Viewer {
    id: ViewId,
    outlet: Box<dyn Outlet>,
    sent: Option<Sent>,
    /// A frame has gone and its acknowledgement has not come back
    in_flight: bool,
    /// Its connection's writer has ended, so nothing sent to it arrives
    gone: bool,
}

/// Whether a view may be sent a frame now: it has drawn its last, and the program is not holding the
/// screen. **A view that has been sent nothing is not made to wait out a hold** — it is showing no screen
/// at all, and a screen part way through being redrawn is nearer the truth than none.
fn owed(viewer: &Viewer, held: bool) -> bool {
    !viewer.in_flight && !(held && viewer.sent.is_some())
}

struct Held {
    /// The row as a message carries it, less its line number
    body: Vec<u8>,
    /// What the grid flags the row as, as `Terminal::row_flags` answers it: that it runs on, and that the
    /// shell drew its prompt there
    flags: u8,
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
    /// The rows of the screen as the last frame published to any view carried them, by line, with the
    /// screen's first line and height at that frame. What a view holds of the screen once it has applied
    /// the frames sent; a fetch during a hold answers from it.
    shown: BTreeMap<u64, Vec<u8>>,
    shown_first: u64,
    shown_rows: u16,
    cursor: Option<Cursor>,
    mouse: bool,
    /// The shell's command generation when the screen was last read
    commands: u32,
    /// The image storage's generation when the screen was last read, as much of it as a view compares
    images: u32,
    /// One cell in pixels when the screen was last read, which is what a row's images are measured in
    cell: (u16, u16),
    /// How many times that has changed. Where an image is drawn and how large depends on it as much as
    /// on the image, so a view is told of a change in either as one: what a frame calls `images` is the
    /// storage's generation and this, added
    measured: u32,
    /// Whether the program had the screen in reverse video when it was last read
    reverse: bool,
    /// The colours the program has changed from the ones the session was given, as of the last read
    colors: ColorChanges,
    /// The sequence at which those last changed
    colors_changed: u64,
    /// The terminal may no longer be what is held here: it was written to or resized since it was read
    stale: bool,
    viewers: Vec<Viewer>,
    /// The row being read, kept so reading one allocates only when it is longer than any before it
    read: RowRead,
    /// That row as bytes, kept for the same reason
    body: Vec<u8>,
    /// Which rows of the screen the core says changed, as of the read under way
    changed: Vec<bool>,
}

impl Publication {
    /// `epoch` is where its numbering starts, which is past any this session's views were told before.
    pub fn new(terminal: &Terminal, epoch: u32) -> Result<Self, Failed> {
        let scrollback = terminal.scrollback_rows()?;
        Ok(Self {
            mark: Mark::at_active_top(terminal)?,
            rendering: Rendering::new()?,
            epoch,
            first_active: u64::from(scrollback),
            captured_first: u64::from(scrollback),
            sequence: 0,
            columns: terminal.cols()?,
            rows: terminal.rows()?,
            alternate: terminal.alt_screen()?,
            scrollback,
            captured_scrollback: scrollback,
            active: Vec::new(),
            shown: BTreeMap::new(),
            shown_first: 0,
            shown_rows: 0,
            cursor: None,
            mouse: false,
            commands: 0,
            images: 0,
            cell: (0, 0),
            measured: 0,
            reverse: false,
            colors: ColorChanges::default(),
            colors_changed: 0,
            stale: true,
            viewers: Vec::new(),
            read: RowRead::default(),
            body: Vec::new(),
            changed: Vec::new(),
        })
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn first_active(&self) -> u64 {
        self.first_active
    }

    /// Whether no view is left.
    pub fn unviewed(&self) -> bool {
        self.viewers.is_empty()
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
            self.shown.clear();
            // A new epoch is a change in itself, which the next read finds: every row is new to it.
            // Whatever it was that noticed, that read is now owed
            self.stale = true;
        } else if let Some(marked) = marked {
            // The mark was on the screen's first row; it is now that many rows up the history. A row
            // that moved *down* — a program scrolling its screen the other way — moved no line into
            // history, and the rows are compared by content either way
            self.first_active += u64::from(scrollback.saturating_sub(marked));
        }
        self.mark.reset(terminal)
    }

    /// Reads the screen and notes what changed.
    ///
    /// **The core says which rows to read, where it can be taken at its word.** It keeps a flag a row
    /// for what was written since it was last asked, so a keystroke's echo is one row read rather than
    /// a screen of them. That holds while every row is where it was. When the screen scrolled the core
    /// says everything changed — which is true of where each row is and not of what it holds — so every
    /// row is read and compared with the row that was on that line: a line that only moved is the row
    /// already held, and is not sent again.
    ///
    /// **Its word is about what a row holds, and a row is also what it is flagged as**: that it runs on
    /// into the next, which is set by the character that wraps *off* it and is written on the row below;
    /// and that the shell drew its prompt there, which a mark sets without printing anything. Neither
    /// touches the row's cells. So a row the core says is unchanged has those two flags read — and not
    /// its cells — and is read whole when they are not the ones it is held with.
    ///
    /// **Nor is it about what a row shows of an image.** A row of placeholder cells is sent with the parts
    /// of images those cells show (`wire::Piece`), and those change when the image does — sent again,
    /// replaced, deleted — and when a cell's size in pixels does, with no cell of the row written. So a
    /// read after either reads every row, and compares each as it does after a scroll.
    ///
    /// **Nothing held moves until all of it is read.** A read the core refuses part way leaves no screen
    /// held rather than half of one, with no row stamped newer than the sequence a view is compared
    /// with, and the next read starts from nothing.
    fn capture(&mut self, terminal: &Terminal, commands: u32) -> Result<(), Failed> {
        self.advance(terminal)?;
        // A flag for each row the core answers for by itself, and none when it says everything changed
        self.rendering.look(terminal, &mut self.changed)?;

        // Compared and never ordered, so the low half is all of it a view needs
        let images = terminal.images_generation() as u32;
        let cell = terminal.cell_pixels();
        let measured = if cell == self.cell { self.measured } else { self.measured.wrapping_add(1) };
        let pictured = images != self.images || measured != self.measured;

        let next = self.sequence + 1;
        let rows = usize::from(self.rows);
        let shift = usize::try_from(self.first_active - self.captured_first).unwrap_or(usize::MAX);
        // History that shrank with nothing on the screen moving — a program erasing it — is a change a
        // view has to hear of: it is holding lines that are gone. A screen that scrolled needs no saying
        // so here: its last rows are on lines nothing was held for, and are new whatever they hold
        let mut changed = self.scrollback != self.captured_scrollback;
        let mut before: Vec<Option<Held>> = std::mem::take(&mut self.active).into_iter().map(Some).collect();
        let mut active = Vec::with_capacity(rows);
        for index in 0..rows {
            let was = shift.checked_add(index).and_then(|was| before.get_mut(was)).and_then(Option::take);
            let was = match was {
                // The core's flag is of the row at this place on the screen, and what is held is the row
                // that was on this line: the same row only while nothing has scrolled. `shift` is this
                // file's own count of what did, kept beside the core's word: it flags no row once any has
                Some(held)
                    if shift == 0
                        && !pictured
                        && self.changed.get(index) == Some(&false)
                        && terminal.row_flags(Space::Active, index as u32)? == Some(held.flags) =>
                {
                    active.push(held);
                    continue;
                }
                was => was,
            };
            terminal.read_row_into(Space::Active, index as u32, &mut self.read)?;
            self.body.clear();
            wire::write_row(&mut self.body, self.read.wrapped, self.read.prompt, &self.read);
            match was {
                Some(held) if held.body == self.body => active.push(held),
                _ => {
                    changed = true;
                    let flags = wire::row_flags(self.read.wrapped, self.read.prompt);
                    active.push(Held { body: self.body.clone(), flags, changed: next });
                }
            }
        }
        let cursor = self.rendering.cursor()?.map(|read| Cursor {
            column: read.column,
            row: read.row,
            shape: read.shape,
            blinking: read.blinking,
            hidden: read.hidden,
        });
        let mouse = terminal.mouse_tracking()?;
        let reverse = terminal.mode(REVERSE_VIDEO)?;
        let colors = terminal.color_changes()?;

        // Everything is read: from here nothing can fail, and what is held moves as one
        if (cursor, mouse, reverse, commands, images, measured)
            != (self.cursor, self.mouse, self.reverse, self.commands, self.images, self.measured)
        {
            changed = true;
        }
        if colors != self.colors {
            changed = true;
            self.colors = colors;
            self.colors_changed = next;
        }
        self.active = active;
        self.captured_first = self.first_active;
        self.captured_scrollback = self.scrollback;
        (self.cursor, self.mouse, self.reverse, self.commands, self.images) =
            (cursor, mouse, reverse, commands, images);
        (self.cell, self.measured) = (cell, measured);
        if changed {
            self.sequence = next;
        }
        self.stale = false;
        // What the core flagged has been read. Told so last: were this refused, the flags would stand
        // and the same rows be read again, which costs a read and loses nothing
        self.rendering.seen()
    }

    /// Whether a view has yet to be sent the screen as held.
    fn behind(&self, viewer: &Viewer) -> bool {
        viewer.sent.is_none_or(|sent| sent.epoch != self.epoch || sent.sequence != self.sequence || sent.rest.is_some())
    }

    /// Sends every view that has drawn its last frame the one it is owed, if it is owed one.
    ///
    /// `commands` is the shell's command generation, which is a change a view is told of like any other.
    /// `held` is whether the program is holding the screen (mode 2026), which is `Screen`'s to know: it
    /// learns of a hold where the bytes that begin and end one are parsed.
    pub fn pump(&mut self, terminal: &Terminal, commands: u32, held: bool) -> Result<(), Failed> {
        if !self.viewers.iter().any(|viewer| owed(viewer, held)) {
            return Ok(());
        }
        if self.stale {
            self.capture(terminal, commands)?;
        }
        for index in 0..self.viewers.len() {
            let viewer = &self.viewers[index];
            if owed(viewer, held) && self.behind(viewer) {
                self.send_frame(terminal, index)?;
            }
        }
        // A view whose connection has gone is one nothing will acknowledge for
        self.viewers.retain(|viewer| !viewer.gone);
        Ok(())
    }

    /// Sends a view the frame that takes it from what it was sent towards the screen as held: all the
    /// way, unless that is more than one message carries.
    ///
    /// **A frame that would be too long is cut at a row, and the view is owed the rest.** What it was not
    /// sent is remembered as the line it was cut at (`Rest`), its next frame begins there, and the
    /// sequence moves on so that frame has a number of its own. Rows are sent in line order, so what a
    /// view lacks is always everything from one line down.
    ///
    /// **A view sent its first frame is told to trust nothing it holds of the history.** It may be a
    /// view that was attached before, and kept the lines it was sent then: the numbering is the same one
    /// while any other view has stayed, and a line can have been written to again after this view last
    /// saw it. So its first frame names every line of history as stale, which costs a view that holds
    /// none nothing, and the views that stayed are not disturbed.
    fn send_frame(&mut self, terminal: &Terminal, index: usize) -> Result<(), Failed> {
        let channel = self.viewers[index].id.channel;
        let sent = self.viewers[index].sent.filter(|sent| sent.epoch == self.epoch);
        let mut rows = Rows::default();
        let mut stale_from = None;
        let mut cut = None;
        let mut carried: Vec<(u64, Vec<u8>)> = Vec::new();

        // **The lines that left the screen since.** A view holds them as they were when it was last sent
        // the screen, and a line is often written to once more before it scrolls — the one being typed
        // on always is. A few are sent as they ended up; a flood of them is named instead, since a view
        // showing the screen has no use for a thousand lines that passed between two of its frames.
        match sent {
            Some(sent) => {
                // None left, and the loop below sends none
                let left = self.first_active.saturating_sub(sent.first_active);
                if left <= u64::from(self.rows) && left <= u64::from(self.scrollback) {
                    for line in sent.first_active..self.first_active {
                        let Some(row) = self.screen_row(line) else { continue };
                        if !terminal.read_row_into(Space::Screen, row, &mut self.read)? {
                            continue;
                        }
                        self.body.clear();
                        wire::write_row(&mut self.body, self.read.wrapped, self.read.prompt, &self.read);
                        if !rows.takes(self.body.len()) {
                            cut = Some(line);
                            break;
                        }
                        rows.push(line, &self.body);
                        carried.push((line, self.body.clone()));
                    }
                } else {
                    stale_from = Some(sent.first_active);
                }
            }
            None if self.scrollback > 0 => stale_from = Some(self.first_active - u64::from(self.scrollback)),
            None => {}
        }
        if cut.is_none() {
            for (row, held) in self.active.iter().enumerate() {
                let line = self.first_active + row as u64;
                if held.changed <= Sent::seen(sent, line) {
                    continue;
                }
                if !rows.takes(held.body.len()) {
                    cut = Some(line);
                    break;
                }
                rows.push(line, &held.body);
                carried.push((line, held.body.clone()));
            }
        }

        let head = FrameHead {
            channel,
            sequence: self.sequence,
            epoch: self.epoch,
            first_active: self.first_active,
            columns: self.columns,
            screen_rows: self.rows,
            scrollback: self.scrollback,
            commands: self.commands,
            images: self.images.wrapping_add(self.measured),
            cursor: self.cursor,
            alternate_screen: self.alternate,
            mouse: self.mouse,
            reverse_video: self.reverse,
            stale_from,
            // All of them whenever any changed since the frame this view last had, and in its first
            colors: sent.is_none_or(|sent| self.colors_changed > sent.sequence).then_some(&self.colors),
        };
        let message = wire::frame(&head, &rows);
        // What the view has of the lines it was not sent: the older of what it had of any of them, so a
        // line it was sent in between is sent again rather than taken to be newer than it is
        let seen = sent.map_or(0, |sent| sent.rest.map_or(sent.sequence, |rest| rest.seen));
        let viewer = &mut self.viewers[index];
        match message {
            Ok(message) => {
                viewer.sent = Some(Sent {
                    epoch: self.epoch,
                    sequence: self.sequence,
                    first_active: cut.map_or(self.first_active, |from| from.min(self.first_active)),
                    rest: cut.map(|from| Rest { from, seen }),
                });
                viewer.in_flight = true;
                viewer.gone = !viewer.outlet.deliver(message);
                self.show(carried);
            }
            // Past what a message carries though its rows were counted in: the view is told it cannot
            // be shown this session, and is not sent the same frame again at every change
            Err(too_long) => {
                let told = ToView::Failed { channel, request: 0, message: too_long.to_string() };
                if let Ok(told) = told.encode() {
                    viewer.outlet.deliver(told);
                }
                viewer.gone = true;
            }
        }
        if cut.is_some() {
            self.sequence += 1;
        }
        Ok(())
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

    /// The lines a view asked for, as the terminal holds them now, and how many lines from `first` that
    /// answers for: all that were asked, up to `FETCH_LINES`, unless a message filled first. A line
    /// answered for and not among the rows is one the terminal does not hold.
    ///
    /// **While the program holds the screen, a line on the screen is the row last sent for it**, the same
    /// rule the frames follow: what the program is part way through writing is not shown to a view until
    /// it lets go. A line that was not on the screen when it was last sent is read as the terminal has it.
    pub fn fetch(&mut self, terminal: &Terminal, first: u64, count: u32, held: bool) -> Result<(u32, Rows), Failed> {
        self.advance(terminal)?;
        let mut rows = Rows::default();
        let mut covered = 0;
        for line in first..first.saturating_add(u64::from(count.min(FETCH_LINES))) {
            if held && (self.shown_first..self.shown_first + u64::from(self.shown_rows)).contains(&line) {
                // A line no frame has carried is one the view does not hold, and is answered absent
                if let Some(body) = self.shown.get(&line) {
                    if !rows.takes(body.len()) {
                        break;
                    }
                    rows.push(line, body);
                }
                covered += 1;
                continue;
            }
            if let Some(row) = self.screen_row(line)
                && terminal.read_row_into(Space::Screen, row, &mut self.read)?
            {
                self.body.clear();
                wire::write_row(&mut self.body, self.read.wrapped, self.read.prompt, &self.read);
                if !rows.takes(self.body.len()) {
                    break;
                }
                rows.push(line, &self.body);
            }
            covered += 1;
        }
        Ok((covered, rows))
    }

    /// Records what a frame sent to a view carried of the screen: a line the frame did not carry is what it
    /// was, which is what a view that applied the frame holds.
    fn show(&mut self, carried: Vec<(u64, Vec<u8>)>) {
        self.shown_first = self.first_active;
        self.shown_rows = self.rows;
        let on_screen = self.shown_first..self.shown_first + u64::from(self.shown_rows);
        self.shown.retain(|line, _| on_screen.contains(line));
        for (line, body) in carried {
            if on_screen.contains(&line) {
                self.shown.insert(line, body);
            }
        }
    }

    /// Starts sending a view this session's frames, beginning with the whole screen.
    ///
    /// **A view that could not be sent its first frame is not attached**: left among the views it would
    /// be one nothing was ever sent to, waiting on an acknowledgement of a frame that never went.
    pub fn attach(
        &mut self,
        terminal: &Terminal,
        commands: u32,
        held: bool,
        id: ViewId,
        outlet: Box<dyn Outlet>,
    ) -> Result<(), Failed> {
        self.viewers.retain(|viewer| viewer.id != id);
        self.viewers.push(Viewer { id, outlet, sent: None, in_flight: false, gone: false });
        self.stale = true;
        self.pump(terminal, commands, held).inspect_err(|_| self.detach(id))
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
    pub fn acknowledge(
        &mut self,
        terminal: &Terminal,
        commands: u32,
        held: bool,
        id: ViewId,
        sequence: u64,
    ) -> Result<(), Failed> {
        if let Some(viewer) = self.viewers.iter_mut().find(|viewer| viewer.id == id)
            && viewer.sent.is_some_and(|sent| sent.sequence == sequence)
        {
            viewer.in_flight = false;
        }
        self.pump(terminal, commands, held)
    }

    /// The session is over: every view is sent the screen as it ended, whether or not it had drawn its
    /// last frame and whatever the program was holding, and told there will be no more.
    pub fn end(&mut self, terminal: &Terminal, commands: u32) {
        self.stale = true;
        let captured = self.capture(terminal, commands).is_ok();
        for index in 0..self.viewers.len() {
            // As many frames as the screen takes, which is one unless it is longer than a message. A frame
            // cut short still carries a row (`Rows::takes`), so each leaves fewer to send and one is the last
            while captured
                && !self.viewers[index].gone
                && self.behind(&self.viewers[index])
                && self.send_frame(terminal, index).is_ok()
            {}
            let viewer = &self.viewers[index];
            if let Ok(ended) = (ToView::Ended { channel: viewer.id.channel }).encode() {
                viewer.outlet.deliver(ended);
            }
        }
        self.viewers.clear();
    }
}
