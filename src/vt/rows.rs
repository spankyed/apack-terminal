//! A row read out of the terminal for something that draws it: every character with its style and link.
//!
//! `screen.rs` reads rows as text, for a search or a wait. A view needs the rest — which cells a character
//! covers, how it is drawn, what it links to — and needs the same answer for a row whether it is on the
//! screen or far up the history, because a view keeps a row it was sent and must be sent the same one when
//! it asks again.
//!
//! **So there is one reader and it reads the grid directly**, at a point in any of the core's coordinate
//! spaces. The core's render state would read the screen faster, but only the screen: a second reader for
//! history is a second answer to disagree with the first. What the render state is asked is which rows of
//! the screen to read at all (`Rendering`).

use ghostty_vt_rs_sys as sys;

use super::placeholders::{PLACEHOLDER, Run, Said};
use super::{CLUSTER_CODEPOINTS, Failed, Terminal};
use crate::view::wire::{self, Color, ColorChanges, Piece, Style};

/// The most one row's links may come to, in bytes. Past it a cell is drawn unlinked.
///
/// **What keeps a row inside a message.** A row is sent whole, and everything else about one is bounded
/// by the grid's width; its links are as many and as long as a program cares to make them. A megabyte of
/// addresses on one row is nobody's screen, and is far inside what a message carries.
pub const ROW_LINK_BYTES: usize = 1 << 20;

/// Which coordinate space a row is named in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// Row zero is the top of the area a program can write to
    Active,
    /// Row zero is the oldest line still held
    Screen,
}

/// One character on a row, with its own style and link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRead<'a> {
    pub column: u16,
    pub width: u8,
    pub text: &'a str,
    pub style: Style,
    pub link: Option<&'a str>,
}

/// Where one character is on its row, and where in the row's tables what it is made of is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placed {
    column: u16,
    width: u8,
    /// Where its text ends in the row's; it begins where the character before it ends
    end: u32,
    /// An index into the row's styles, counted from one; `0` is the plain style
    style: u16,
    /// An index into the row's links, counted from one; `0` is none
    link: u16,
}

/// A row as the grid holds it. Blank cells nothing styled are absent.
///
/// **One allocation for the text of every character and one for where each is**, rather than a string a
/// cell: a screen is read whole whenever it scrolls, and a row is compared with the row it replaces, so
/// what a row costs to read and to compare is paid for every line a program prints. Two rows are equal
/// when they would be drawn alike.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowRead {
    pub wrapped: bool,
    pub prompt: bool,
    /// Every character on the row, one after another
    text: String,
    cells: Vec<Placed>,
    /// Each distinct style on the row but the plain one, in the order they first appear
    styles: Vec<Style>,
    /// Each distinct link on the row, in the order they first appear
    links: Vec<String>,
    /// The runs of placeholder cells on the row, as the cells say them, left to right
    runs: Vec<Run>,
    /// What those runs show, for the ones that show anything, and the cell they are measured in
    pieces: Vec<Piece>,
    cell: (u16, u16),
}

impl RowRead {
    /// Every character on the row, left to right.
    pub fn cells(&self) -> impl Iterator<Item = CellRead<'_>> {
        (0..self.cells.len()).map(|index| {
            let placed = self.cells[index];
            CellRead {
                column: placed.column,
                width: placed.width,
                text: self.text_of(index),
                style: placed.style.checked_sub(1).map_or(Style::default(), |at| self.styles[usize::from(at)]),
                link: placed.link.checked_sub(1).map(|at| self.links[usize::from(at)].as_str()),
            }
        })
    }

    fn text_of(&self, index: usize) -> &str {
        let start = index.checked_sub(1).map_or(0, |before| self.cells[before].end as usize);
        &self.text[start..self.cells[index].end as usize]
    }

    /// Empties the row, keeping what it allocated for the next one read into it.
    fn clear(&mut self) {
        self.wrapped = false;
        self.prompt = false;
        self.text.clear();
        self.cells.clear();
        self.styles.clear();
        self.links.clear();
        self.runs.clear();
        self.pieces.clear();
        self.cell = (0, 0);
    }

    /// The parts of images this row shows through placeholder cells, left to right.
    pub fn pieces(&self) -> &[Piece] {
        &self.pieces
    }

    /// Notes a placeholder cell at `column`: the next cell of the run before it, or the first of another.
    fn placeholder(&mut self, column: u16, said: Said) {
        if !self.runs.last_mut().is_some_and(|run| run.takes(column, said)) {
            self.runs.push(Run::begin(column, said));
        }
    }

    /// The index of `style` in this row's table, added if it is new. `0` is the plain style.
    fn style_index(&mut self, style: Style) -> u16 {
        if style == Style::default() {
            return 0;
        }
        // The newest first: a run of cells shares the style of the cell before it
        let found = self.styles.iter().rposition(|held| *held == style).unwrap_or_else(|| {
            self.styles.push(style);
            self.styles.len() - 1
        });
        // A row has at most a grid's width of cells, which is far inside sixteen bits
        u16::try_from(found + 1).unwrap_or(0)
    }

    /// The index of `link` in this row's table, added if it is new — or `0`, no link, when the row's
    /// links already come to `ROW_LINK_BYTES` and this one would be more.
    fn link_index(&mut self, link: &[u8]) -> u16 {
        if let Some(found) = self.links.iter().rposition(|held| held.as_bytes() == link) {
            return u16::try_from(found + 1).unwrap_or(0);
        }
        let held: usize = self.links.iter().map(String::len).sum();
        if held + link.len() > ROW_LINK_BYTES {
            return 0;
        }
        self.links.push(String::from_utf8_lossy(link).into_owned());
        u16::try_from(self.links.len()).unwrap_or(0)
    }
}

impl wire::Cells for RowRead {
    fn styles(&self) -> &[Style] {
        &self.styles
    }

    fn links(&self) -> &[String] {
        &self.links
    }

    fn count(&self) -> usize {
        self.cells.len()
    }

    fn cell(&self, index: usize) -> wire::Cell<'_> {
        let placed = self.cells[index];
        wire::Cell {
            column: placed.column,
            width: placed.width,
            text: self.text_of(index),
            style: placed.style,
            link: placed.link,
        }
    }

    fn pieces(&self) -> (&[Piece], (u16, u16)) {
        (&self.pieces, self.cell)
    }
}

/// Where the cursor is and how it is drawn. A cursor the program has hidden is still somewhere, which
/// is where an input method composes (`wire::Cursor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRead {
    pub column: u16,
    pub row: u16,
    /// `0` block, `1` bar, `2` underline, `3` hollow
    pub shape: u8,
    pub blinking: bool,
    /// The program has hidden it (mode 25)
    pub hidden: bool,
}

fn color(color: sys::StyleColor) -> Color {
    match u32::from(color.tag) {
        // SAFETY: the tag says which member of the union was written
        sys::StyleColorTag::PALETTE => Color::Palette(unsafe { color.value.palette }),
        sys::StyleColorTag::RGB => {
            // SAFETY: as above
            let rgb = unsafe { color.value.rgb };
            Color::Rgb(rgb.r, rgb.g, rgb.b)
        }
        _ => Color::Default,
    }
}

impl Terminal {
    fn reference_in(&self, space: Space, column: u16, row: u32) -> Option<sys::GridRef> {
        let tag = match space {
            Space::Active => sys::PointTag::ACTIVE,
            Space::Screen => sys::PointTag::SCREEN,
        };
        let point =
            sys::Point { tag, value: sys::PointValue { coordinate: sys::PointCoordinate { x: column, y: row } } };
        let mut reference = sys::GridRef::default();
        // SAFETY: `reference` is a valid out-pointer and the point is by value
        let found = unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut reference) };
        (found == sys::Result::SUCCESS).then_some(reference)
    }

    /// The row at `row` in `space`, or `None` where the terminal holds no such row.
    pub fn read_row(&self, space: Space, row: u32) -> Result<Option<RowRead>, Failed> {
        let mut read = RowRead::default();
        Ok(self.read_row_into(space, row, &mut read)?.then_some(read))
    }

    /// Whether the row at `row` in `space` runs on into the next and whether the shell drew its prompt
    /// there, as the byte a row's body begins with (`wire::write_row`), or `None` where there is no such
    /// row. What a row holds is not read.
    pub fn row_flags(&self, space: Space, row: u32) -> Result<Option<u8>, Failed> {
        let Some(reference) = self.reference_in(space, 0, row) else { return Ok(None) };
        let mut raw: sys::Row = 0;
        // SAFETY: the reference was just filled and nothing has written since
        if unsafe { sys::ghostty_grid_ref_row(&raw const reference, &raw mut raw) } != sys::Result::SUCCESS {
            return Ok(None);
        }
        let mut wrapped = false;
        let mut prompt: sys::RowSemanticPrompt::Type = sys::RowSemanticPrompt::NONE;
        // SAFETY: WRAP writes a `bool` and SEMANTIC_PROMPT a `GhosttyRowSemanticPrompt`
        unsafe {
            Failed::check(
                "ghostty_row_get(WRAP)",
                sys::ghostty_row_get(raw, sys::RowData::WRAP, (&raw mut wrapped).cast()),
            )?;
            Failed::check(
                "ghostty_row_get(SEMANTIC_PROMPT)",
                sys::ghostty_row_get(raw, sys::RowData::SEMANTIC_PROMPT, (&raw mut prompt).cast()),
            )?;
        }
        Ok(Some(wire::row_flags(wrapped, prompt != sys::RowSemanticPrompt::NONE)))
    }

    /// One of the terminal's colours, or `None` where it has none.
    fn color_of(&self, key: sys::TerminalData::Type, what: &'static str) -> Result<Option<[u8; 3]>, Failed> {
        let mut color = sys::ColorRgb { r: 0, g: 0, b: 0 };
        // SAFETY: every key passed here writes a `GhosttyColorRgb`
        let result = unsafe { sys::ghostty_terminal_get(self.handle, key, (&raw mut color).cast()) };
        if result == sys::Result::NO_VALUE {
            return Ok(None);
        }
        Failed::check(what, result)?;
        Ok(Some([color.r, color.g, color.b]))
    }

    fn palette_of(&self, key: sys::TerminalData::Type, what: &'static str) -> Result<[sys::ColorRgb; 256], Failed> {
        let mut palette = [sys::ColorRgb { r: 0, g: 0, b: 0 }; 256];
        // SAFETY: both keys passed here write exactly 256 `GhosttyColorRgb`
        let result = unsafe { sys::ghostty_terminal_get(self.handle, key, palette.as_mut_ptr().cast()) };
        Failed::check(what, result)?;
        Ok(palette)
    }

    /// The colours a program has changed from the ones the session was given: the foreground, background
    /// and cursor it set (OSC 10, 11, 12), and each palette entry it redefined (OSC 4).
    ///
    /// **As a difference from what the session was given**, because that is what a view lacks: it draws
    /// in its own theme, which is where the given colours came from, and a program that changes one —
    /// a shell theme redefining the palette, an editor setting its background — expects to be drawn in
    /// what it asked for.
    pub fn color_changes(&self) -> Result<ColorChanges, Failed> {
        use sys::TerminalData as Data;
        let changed = |now: Data::Type, given: Data::Type, what: &'static str| -> Result<Option<[u8; 3]>, Failed> {
            let now = self.color_of(now, what)?;
            Ok(now.filter(|_| now != self.color_of(given, what).ok().flatten()))
        };
        let now = self.palette_of(Data::COLOR_PALETTE, "ghostty_terminal_get(COLOR_PALETTE)")?;
        let given = self.palette_of(Data::COLOR_PALETTE_DEFAULT, "ghostty_terminal_get(COLOR_PALETTE_DEFAULT)")?;
        let palette = (0..=u8::MAX)
            .zip(now.iter().zip(given.iter()))
            .filter(|(_, (now, given))| (now.r, now.g, now.b) != (given.r, given.g, given.b))
            .map(|(index, (now, _))| (index, [now.r, now.g, now.b]))
            .collect();
        Ok(ColorChanges {
            foreground: changed(
                Data::COLOR_FOREGROUND,
                Data::COLOR_FOREGROUND_DEFAULT,
                "ghostty_terminal_get(COLOR_FOREGROUND)",
            )?,
            background: changed(
                Data::COLOR_BACKGROUND,
                Data::COLOR_BACKGROUND_DEFAULT,
                "ghostty_terminal_get(COLOR_BACKGROUND)",
            )?,
            cursor: changed(Data::COLOR_CURSOR, Data::COLOR_CURSOR_DEFAULT, "ghostty_terminal_get(COLOR_CURSOR)")?,
            palette,
        })
    }

    /// Whether the cursor has written the last cell of its row and not yet moved on: the next character
    /// goes on the row below.
    pub fn cursor_pending_wrap(&self) -> Result<bool, Failed> {
        let mut pending = false;
        // SAFETY: CURSOR_PENDING_WRAP writes a `bool`
        let result = unsafe {
            sys::ghostty_terminal_get(self.handle, sys::TerminalData::CURSOR_PENDING_WRAP, (&raw mut pending).cast())
        };
        Failed::check("ghostty_terminal_get(CURSOR_PENDING_WRAP)", result)?;
        Ok(pending)
    }

    /// The same into a row the caller keeps, so reading a screen allocates for the rows that grew and
    /// not for every row. Answers whether the terminal holds such a row; `read` is empty when it does not.
    pub fn read_row_into(&self, space: Space, row: u32, read: &mut RowRead) -> Result<bool, Failed> {
        read.clear();
        let Some(mut reference) = self.reference_in(space, 0, row) else { return Ok(false) };
        let mut raw: sys::Row = 0;
        // SAFETY: the reference was just filled and nothing has written since
        if unsafe { sys::ghostty_grid_ref_row(&raw const reference, &raw mut raw) } != sys::Result::SUCCESS {
            return Ok(false);
        }
        let mut wrapped = false;
        let mut prompt: sys::RowSemanticPrompt::Type = sys::RowSemanticPrompt::NONE;
        // SAFETY: WRAP writes a `bool` and SEMANTIC_PROMPT a `GhosttyRowSemanticPrompt`
        unsafe {
            Failed::check(
                "ghostty_row_get(WRAP)",
                sys::ghostty_row_get(raw, sys::RowData::WRAP, (&raw mut wrapped).cast()),
            )?;
            Failed::check(
                "ghostty_row_get(SEMANTIC_PROMPT)",
                sys::ghostty_row_get(raw, sys::RowData::SEMANTIC_PROMPT, (&raw mut prompt).cast()),
            )?;
        }
        read.wrapped = wrapped;
        read.prompt = prompt != sys::RowSemanticPrompt::NONE;
        for column in 0..self.cols()? {
            // **A reference is a row and a column in it**, so the next cell is the same reference one
            // column on. Asking the terminal for each cell by point walks its pages from the top every
            // time, which for a row deep in a long history is most of the cost of reading it.
            reference.x = column;
            self.read_cell(&reference, column, read)?;
        }
        if !read.runs.is_empty() {
            self.show_runs(read);
        }
        Ok(true)
    }

    /// Works out what each run of placeholder cells on a row shows (`placeholders`), as the image storage
    /// stands now. A run whose image is gone, has not all arrived, or has no virtual placement shows
    /// nothing, and its cells are blank.
    fn show_runs(&self, read: &mut RowRead) {
        let cell = self.cell_pixels();
        for run in &read.runs {
            let Some(shown) = self.shown_by(run.image(), run.placement()) else { continue };
            let Some(fit) =
                super::placeholders::fit(run, shown.grid, shown.picture, (u32::from(cell.0), u32::from(cell.1)))
            else {
                continue;
            };
            read.pieces.push(Piece {
                image: run.image(),
                generation: shown.generation,
                column: run.column,
                columns: run.width,
                offset_x: fit.offset.0,
                offset_y: fit.offset.1,
                width: fit.size.0,
                height: fit.size.1,
                source_x: fit.source.0,
                source_y: fit.source.1,
                source_width: fit.source.2,
                source_height: fit.source.3,
            });
        }
        if !read.pieces.is_empty() {
            read.cell = cell;
        }
    }

    /// Adds the cell at `reference` to `read`, when it holds a character or is drawn as anything.
    fn read_cell(&self, reference: &sys::GridRef, column: u16, read: &mut RowRead) -> Result<(), Failed> {
        let mut cell: sys::Cell = 0;
        // SAFETY: the reference names a cell of a row that was just looked up
        if unsafe { sys::ghostty_grid_ref_cell(reference, &raw mut cell) } != sys::Result::SUCCESS {
            return Ok(());
        }
        let mut wide: sys::CellWide::Type = sys::CellWide::NARROW;
        let mut content: sys::CellContentTag::Type = sys::CellContentTag::CODEPOINT;
        let mut codepoint: u32 = 0;
        let mut styled = false;
        let mut linked = false;
        // SAFETY: each key writes the type its out-pointer is: a `GhosttyCellWide`, a
        // `GhosttyCellContentTag`, a `uint32_t` and two `bool`s
        unsafe {
            Failed::check(
                "ghostty_cell_get(WIDE)",
                sys::ghostty_cell_get(cell, sys::CellData::WIDE, (&raw mut wide).cast()),
            )?;
            Failed::check(
                "ghostty_cell_get(CONTENT_TAG)",
                sys::ghostty_cell_get(cell, sys::CellData::CONTENT_TAG, (&raw mut content).cast()),
            )?;
            Failed::check(
                "ghostty_cell_get(CODEPOINT)",
                sys::ghostty_cell_get(cell, sys::CellData::CODEPOINT, (&raw mut codepoint).cast()),
            )?;
            Failed::check(
                "ghostty_cell_get(HAS_STYLING)",
                sys::ghostty_cell_get(cell, sys::CellData::HAS_STYLING, (&raw mut styled).cast()),
            )?;
            Failed::check(
                "ghostty_cell_get(HAS_HYPERLINK)",
                sys::ghostty_cell_get(cell, sys::CellData::HAS_HYPERLINK, (&raw mut linked).cast()),
            )?;
        }
        // The second cell of a wide character is that character's, and holds nothing of its own.
        //
        // **The cell a wide character did not fit in is not one of those.** It is the last cell of the
        // row before, left empty when the character went to the next one, and it is drawn as any empty
        // cell is: with whatever colours it was left holding. The core marks it only for as long as that
        // character is there, and turns it back into an ordinary cell when the character is written
        // over — a change to this row made by a write to the next, which no row is reported as changed
        // for. So it is read as the empty cell it is drawn as, and what a view was sent does not depend
        // on which of the two it was at the time
        if wide == sys::CellWide::SPACER_TAIL {
            return Ok(());
        }

        let mut style = Style::default();
        if styled {
            let mut raw = sys::Style::default();
            // SAFETY: the reference is live and `raw` is a valid out-pointer
            Failed::check("ghostty_grid_ref_style", unsafe { sys::ghostty_grid_ref_style(reference, &raw mut raw) })?;
            style = Style {
                fg: color(raw.fg_color),
                bg: color(raw.bg_color),
                underline: color(raw.underline_color),
                flags: raw.flags,
            };
        }

        let before = read.text.len();
        match content {
            // **A placeholder cell is not a character to draw**: it stands for part of an image, which is
            // read from it and from the characters that follow it in the cell (`placeholders`). What is
            // left is a blank cell in the colours it was written in, which is what the core draws of one
            // whether or not there is an image for it to show
            sys::CellContentTag::CODEPOINT if codepoint == u32::from(PLACEHOLDER) => {
                read.placeholder(column, Said::of(&style, []));
            }
            sys::CellContentTag::CODEPOINT_GRAPHEME if codepoint == u32::from(PLACEHOLDER) => {
                let mut codepoints = [0u32; CLUSTER_CODEPOINTS];
                let mut written: usize = 0;
                // SAFETY: the buffer is `CLUSTER_CODEPOINTS` long, which is the length passed
                let result = unsafe {
                    sys::ghostty_grid_ref_graphemes(
                        reference,
                        codepoints.as_mut_ptr(),
                        codepoints.len(),
                        &raw mut written,
                    )
                };
                // The first is the placeholder itself. A cell holding more characters than a read does
                // is one no program writes for this, and is read as a placeholder that says nothing
                let marks = if result == sys::Result::SUCCESS {
                    &codepoints[1.min(written)..written.min(codepoints.len())]
                } else {
                    &[]
                };
                read.placeholder(column, Said::of(&style, marks.iter().filter_map(|mark| char::from_u32(*mark))));
            }
            sys::CellContentTag::CODEPOINT => {
                if let Some(character) = char::from_u32(codepoint).filter(|found| *found != '\0') {
                    read.text.push(character);
                }
            }
            sys::CellContentTag::CODEPOINT_GRAPHEME => {
                let mut codepoints = [0u32; CLUSTER_CODEPOINTS];
                let mut written: usize = 0;
                // SAFETY: the buffer is `CLUSTER_CODEPOINTS` long, which is the length passed
                let result = unsafe {
                    sys::ghostty_grid_ref_graphemes(
                        reference,
                        codepoints.as_mut_ptr(),
                        codepoints.len(),
                        &raw mut written,
                    )
                };
                if result == sys::Result::SUCCESS {
                    read.text.extend(
                        codepoints[..written.min(codepoints.len())].iter().filter_map(|found| char::from_u32(*found)),
                    );
                } else if let Some(character) = char::from_u32(codepoint) {
                    // Longer than a read holds: its base, as `push_cell_text` answers
                    read.text.push(character);
                }
            }
            // **A cell that is only a background.** An erase with a colour set leaves cells holding no
            // character and no style, just the colour, which is how a full-width status bar is drawn
            sys::CellContentTag::BG_COLOR_PALETTE => {
                let mut index: sys::ColorPaletteIndex = 0;
                // SAFETY: COLOR_PALETTE writes a `GhosttyColorPaletteIndex`
                Failed::check("ghostty_cell_get(COLOR_PALETTE)", unsafe {
                    sys::ghostty_cell_get(cell, sys::CellData::COLOR_PALETTE, (&raw mut index).cast())
                })?;
                style.bg = Color::Palette(index);
            }
            sys::CellContentTag::BG_COLOR_RGB => {
                let mut rgb = sys::ColorRgb { r: 0, g: 0, b: 0 };
                // SAFETY: COLOR_RGB writes a `GhosttyColorRgb`
                Failed::check("ghostty_cell_get(COLOR_RGB)", unsafe {
                    sys::ghostty_cell_get(cell, sys::CellData::COLOR_RGB, (&raw mut rgb).cast())
                })?;
                style.bg = Color::Rgb(rgb.r, rgb.g, rgb.b);
            }
            _ => {}
        }

        let link = if linked { self.read_link(reference, read) } else { 0 };
        // A blank cell nothing styled or linked is not a character: it is the absence of one
        if read.text.len() == before {
            if style == Style::default() && link == 0 {
                return Ok(());
            }
            read.text.push(' ');
        }
        let style = read.style_index(style);
        read.cells.push(Placed {
            column,
            width: if wide == sys::CellWide::WIDE { 2 } else { 1 },
            end: u32::try_from(read.text.len()).unwrap_or(u32::MAX),
            style,
            link,
        });
        Ok(())
    }

    /// The index in `read`'s links of the link the cell at `reference` has, or `0` for none.
    ///
    /// **Read into a buffer on the stack and compared before anything is kept.** A link covers a run of
    /// cells, and each of them answers the same address: what is allocated is one string for the run, not
    /// one for each cell of it. An address longer than the buffer is read into one of its own length,
    /// which is the core's to bound: it holds no link longer than the sequence that set one may be.
    fn read_link(&self, reference: &sys::GridRef, read: &mut RowRead) -> u16 {
        let mut short = [0u8; 256];
        let mut written: usize = 0;
        // SAFETY: the buffer is as long as the length passed
        let result = unsafe {
            sys::ghostty_grid_ref_hyperlink_uri(reference, short.as_mut_ptr(), short.len(), &raw mut written)
        };
        match result {
            // A cell with no link answers a length of nothing
            sys::Result::SUCCESS if written == 0 => 0,
            sys::Result::SUCCESS => read.link_index(&short[..written.min(short.len())]),
            // Too long for the buffer, and `written` is how long it is
            sys::Result::OUT_OF_SPACE => {
                let mut long = vec![0u8; written];
                // SAFETY: as above
                let result = unsafe {
                    sys::ghostty_grid_ref_hyperlink_uri(reference, long.as_mut_ptr(), long.len(), &raw mut written)
                };
                if result == sys::Result::SUCCESS { read.link_index(&long[..written.min(long.len())]) } else { 0 }
            }
            _ => 0,
        }
    }

    /// Whether the program asked for the mouse, in any of the ways it can.
    pub fn mouse_tracking(&self) -> Result<bool, Failed> {
        let mut tracking = false;
        // SAFETY: MOUSE_TRACKING writes a `bool`
        let result = unsafe {
            sys::ghostty_terminal_get(self.handle, sys::TerminalData::MOUSE_TRACKING, (&raw mut tracking).cast())
        };
        Failed::check("ghostty_terminal_get(MOUSE_TRACKING)", result)?;
        Ok(tracking)
    }

    /// Whether a mode is on. `mode` is the core's packed form: the number, with bit 15 set for an ANSI mode.
    pub fn mode(&self, mode: u16) -> Result<bool, Failed> {
        let mut config = sys::TerminalModeConfig { mode, value: false };
        // SAFETY: MODE reads `mode` from and writes `value` to a `GhosttyTerminalModeConfig`
        let result =
            unsafe { sys::ghostty_terminal_get(self.handle, sys::TerminalData::MODE, (&raw mut config).cast()) };
        Failed::check("ghostty_terminal_get(MODE)", result)?;
        Ok(config.value)
    }
}

/// A mark on a cell that follows it as the terminal scrolls and reflows, until its row is dropped.
///
/// **This is what counts the lines that scrolled.** The terminal says how many lines of history it holds,
/// which stops growing once the history is full; a mark set on the screen's first row is found later at
/// some row of the history, and the distance is exactly how many lines moved up past it.
pub struct Mark {
    handle: sys::TrackedGridRef,
}

// SAFETY: as `Terminal`: allocated and freed by the library, and held by one session under its lock
unsafe impl Send for Mark {}

impl Mark {
    /// A mark on the first row a program can write to.
    pub fn at_active_top(terminal: &Terminal) -> Result<Self, Failed> {
        Self::at(terminal, 0, 0)
    }

    /// A mark on the cell at `column` of row `row` of the area a program can write to.
    pub fn at(terminal: &Terminal, column: u16, row: u16) -> Result<Self, Failed> {
        let mut handle: sys::TrackedGridRef = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer and the terminal is live
        let result =
            unsafe { sys::ghostty_terminal_grid_ref_track(terminal.handle, active(column, row), &raw mut handle) };
        Failed::check("ghostty_terminal_grid_ref_track", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_terminal_grid_ref_track", code: sys::Result::SUCCESS });
        }
        Ok(Self { handle })
    }

    /// Moves the mark back to the first row a program can write to, on whichever screen is showing.
    pub fn reset(&mut self, terminal: &Terminal) -> Result<(), Failed> {
        // SAFETY: both handles are live
        let result = unsafe { sys::ghostty_tracked_grid_ref_set(self.handle, terminal.handle, active(0, 0)) };
        Failed::check("ghostty_tracked_grid_ref_set", result)
    }

    /// The cell the mark is on now — its column, and its row counted from the oldest line held — or `None`
    /// once that row is gone.
    pub fn screen_cell(&self) -> Option<(u16, u32)> {
        let mut point = sys::PointCoordinate { x: 0, y: 0 };
        // SAFETY: the handle is live and `point` is a valid out-pointer
        let result = unsafe { sys::ghostty_tracked_grid_ref_point(self.handle, sys::PointTag::SCREEN, &raw mut point) };
        (result == sys::Result::SUCCESS).then_some((point.x, point.y))
    }

    /// The row the mark is on now, counted from the oldest line held, or `None` once that row is gone.
    pub fn screen_row(&self) -> Option<u32> {
        self.screen_cell().map(|(_, row)| row)
    }
}

impl Drop for Mark {
    fn drop(&mut self) {
        // SAFETY: freed once, by the value that owns it
        unsafe { sys::ghostty_tracked_grid_ref_free(self.handle) };
    }
}

fn active(column: u16, row: u16) -> sys::Point {
    sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: column, y: u32::from(row) } },
    }
}

/// The core's own account of what a renderer needs to know that the grid does not say: which rows of the
/// screen changed since it was last looked at, and how the cursor is drawn.
///
/// **It says which rows to read, and the rows are still read from the grid** (`Terminal::read_row_into`),
/// so a row on the screen and the same row once it has scrolled into history are one reader's answer.
///
/// **One per terminal.** Looking consumes the terminal's own record of what changed, so a second one would
/// each be told about half of it.
pub struct Rendering {
    handle: sys::RenderState,
    rows: sys::RenderStateRowIterator,
}

// SAFETY: as `Terminal`: the handles are the library's and held by one session under its lock
unsafe impl Send for Rendering {}

/// What a look at the terminal found changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    /// Anything may have: the screen scrolled, was resized, or changed in a way no row is to blame for
    Everything,
    /// Only the rows said to have
    Rows,
}

impl Rendering {
    pub fn new() -> Result<Self, Failed> {
        let mut handle: sys::RenderState = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer, and a null allocator asks for the library's own
        let result = unsafe { sys::ghostty_render_state_new(std::ptr::null(), &raw mut handle) };
        Failed::check("ghostty_render_state_new", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_render_state_new", code: sys::Result::SUCCESS });
        }
        let mut rows: sys::RenderStateRowIterator = std::ptr::null_mut();
        // SAFETY: `rows` is a valid out-pointer
        let result = unsafe { sys::ghostty_render_state_row_iterator_new(std::ptr::null(), &raw mut rows) };
        // Made before the check, so a failure here frees the state through `Drop`
        let rendering = Self { handle, rows };
        Failed::check("ghostty_render_state_row_iterator_new", result)?;
        Ok(rendering)
    }

    /// Looks at the terminal: afterwards `changed[row]` says whether that row of the screen may differ
    /// from when `seen` was last called, and the cursor is the terminal's.
    ///
    /// **What changed accumulates until `seen`**, so a look whose rows could not all be read loses
    /// nothing: the next one says the same rows again, and whatever changed since.
    pub fn look(&mut self, terminal: &Terminal, changed: &mut Vec<bool>) -> Result<Changed, Failed> {
        // SAFETY: both handles are live, and the session's lock is what serializes this with a write
        Failed::check("ghostty_render_state_update", unsafe {
            sys::ghostty_render_state_update(self.handle, terminal.handle)
        })?;
        let mut dirty: sys::RenderStateDirty::Type = sys::RenderStateDirty::FULL;
        // SAFETY: DIRTY writes a `GhosttyRenderStateDirty`
        Failed::check("ghostty_render_state_get(DIRTY)", unsafe {
            sys::ghostty_render_state_get(self.handle, sys::RenderStateData::DIRTY, (&raw mut dirty).cast())
        })?;
        changed.clear();
        if dirty == sys::RenderStateDirty::FULL {
            return Ok(Changed::Everything);
        }
        // SAFETY: ROW_ITERATOR fills the iterator it is handed a pointer to, which stays valid until the
        // state is next updated — after this function has returned
        Failed::check("ghostty_render_state_get(ROW_ITERATOR)", unsafe {
            sys::ghostty_render_state_get(self.handle, sys::RenderStateData::ROW_ITERATOR, (&raw mut self.rows).cast())
        })?;
        // SAFETY: the iterator was just filled
        while unsafe { sys::ghostty_render_state_row_iterator_next(self.rows) } {
            let mut row = true;
            // SAFETY: DIRTY writes a `bool`, and the iterator is on a row
            Failed::check("ghostty_render_state_row_get(DIRTY)", unsafe {
                sys::ghostty_render_state_row_get(self.rows, sys::RenderStateRowData::DIRTY, (&raw mut row).cast())
            })?;
            changed.push(row);
        }
        Ok(Changed::Rows)
    }

    /// What `look` said changed has been read: the next look says only what changes from here.
    pub fn seen(&mut self) -> Result<(), Failed> {
        // SAFETY: the handle is live
        Failed::check("ghostty_render_state_clean", unsafe { sys::ghostty_render_state_clean(self.handle) })
    }

    /// The cursor as the terminal had it at the last `look`, or `None` when it is not on the screen.
    ///
    /// How the cursor is drawn — its shape, whether it blinks — is not something the terminal answers
    /// directly; the render state resolves it from the program's request and the defaults.
    pub fn cursor(&self) -> Result<Option<CursorRead>, Failed> {
        let mut cursor =
            sys::RenderStateCursor { size: std::mem::size_of::<sys::RenderStateCursor>(), ..Default::default() };
        // SAFETY: CURSOR writes a sized `GhosttyRenderStateCursor`, whose size was just set
        Failed::check("ghostty_render_state_get(CURSOR)", unsafe {
            sys::ghostty_render_state_get(self.handle, sys::RenderStateData::CURSOR, (&raw mut cursor).cast())
        })?;
        if !cursor.viewport_has_value {
            return Ok(None);
        }
        let shape = match cursor.visual_style {
            sys::CursorStyle::BAR => 1,
            sys::CursorStyle::UNDERLINE => 2,
            sys::CursorStyle::BLOCK_HOLLOW => 3,
            _ => 0,
        };
        Ok(Some(CursorRead {
            column: cursor.viewport_x,
            row: cursor.viewport_y,
            shape,
            blinking: cursor.blinking,
            hidden: !cursor.visible,
        }))
    }
}

/// Part of an image the core says a run of placeholder cells on the screen shows: `Rendering::shown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShownRead {
    /// The row of the screen the run is on
    pub row: u16,
    pub piece: Piece,
}

impl Rendering {
    /// What the core says the placeholder cells on the screen show, as of the last `look`, top to bottom
    /// and left to right.
    ///
    /// **The core's answer to the question `Terminal::read_row_into` answers for itself**, and only for
    /// the screen, which is why a row is not read from here (`placeholders`). It is what the suite holds
    /// that reader to: the two have to say the same of every row both answer for.
    pub fn shown(&self) -> Result<Vec<ShownRead>, Failed> {
        use sys::RenderStateImageData as Data;
        let mut iterator: sys::RenderStateImageIterator = std::ptr::null_mut();
        // SAFETY: `iterator` is a valid out-pointer; it is freed below
        Failed::check("ghostty_render_state_image_iterator_new", unsafe {
            sys::ghostty_render_state_image_iterator_new(std::ptr::null(), &raw mut iterator)
        })?;
        // SAFETY: IMAGE_ITERATOR fills the iterator it is handed a pointer to
        let filled = unsafe {
            sys::ghostty_render_state_get(self.handle, sys::RenderStateData::IMAGE_ITERATOR, (&raw mut iterator).cast())
        };
        let mut shown = Vec::new();
        if filled == sys::Result::SUCCESS {
            let read_u32 = |data: Data::Type| {
                let mut value: u32 = 0;
                // SAFETY: each key read through this writes a `uint32_t`
                let result = unsafe { sys::ghostty_render_state_image_get(iterator, data, (&raw mut value).cast()) };
                (result == sys::Result::SUCCESS).then_some(value)
            };
            let read_i32 = |data: Data::Type| {
                let mut value: i32 = 0;
                // SAFETY: each key read through this writes an `int32_t`
                let result = unsafe { sys::ghostty_render_state_image_get(iterator, data, (&raw mut value).cast()) };
                (result == sys::Result::SUCCESS).then_some(value)
            };
            // SAFETY: the iterator was filled from this state and nothing has updated it since
            while unsafe { sys::ghostty_render_state_image_iterator_next(iterator) } {
                let mut kind: sys::RenderStateKittyPlacementKind::Type = sys::RenderStateKittyPlacementKind::FIXED;
                // SAFETY: KITTY_KIND writes a `GhosttyRenderStateKittyPlacementKind`
                let read =
                    unsafe { sys::ghostty_render_state_image_get(iterator, Data::KITTY_KIND, (&raw mut kind).cast()) };
                if read != sys::Result::SUCCESS || kind != sys::RenderStateKittyPlacementKind::VIRTUAL {
                    continue;
                }
                let mut generation: u64 = 0;
                // SAFETY: IMAGE_GENERATION writes a `uint64_t`
                let _ = unsafe {
                    sys::ghostty_render_state_image_get(iterator, Data::IMAGE_GENERATION, (&raw mut generation).cast())
                };
                let place = |value: Option<i32>| value.and_then(|at| u16::try_from(at).ok());
                let (Some(row), Some(column)) =
                    (place(read_i32(Data::VIEWPORT_ROW)), place(read_i32(Data::VIEWPORT_COL)))
                else {
                    continue;
                };
                let of = |data: Data::Type| read_u32(data).unwrap_or(0);
                shown.push(ShownRead {
                    row,
                    piece: Piece {
                        image: of(Data::IMAGE_ID),
                        generation,
                        column,
                        columns: u16::try_from(of(Data::GRID_COLS)).unwrap_or(u16::MAX),
                        offset_x: of(Data::CELL_OFFSET_X),
                        offset_y: of(Data::CELL_OFFSET_Y),
                        width: of(Data::PIXEL_WIDTH),
                        height: of(Data::PIXEL_HEIGHT),
                        source_x: of(Data::SOURCE_X),
                        source_y: of(Data::SOURCE_Y),
                        source_width: of(Data::SOURCE_WIDTH),
                        source_height: of(Data::SOURCE_HEIGHT),
                    },
                });
            }
        }
        // SAFETY: freed once
        unsafe { sys::ghostty_render_state_image_iterator_free(iterator) };
        Failed::check("ghostty_render_state_get(IMAGE_ITERATOR)", filled)?;
        shown.sort_by_key(|one| (one.row, one.piece.column));
        Ok(shown)
    }
}

impl Drop for Rendering {
    fn drop(&mut self) {
        // SAFETY: each is freed once, by the value that owns it, and each free takes a null
        unsafe {
            sys::ghostty_render_state_row_iterator_free(self.rows);
            sys::ghostty_render_state_free(self.handle);
        }
    }
}
