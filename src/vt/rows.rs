//! A row read out of the terminal for something that draws it: every character with its style and link.
//!
//! `screen.rs` reads rows as text, for a search or a wait. A view needs the rest — which cells a character
//! covers, how it is drawn, what it links to — and needs the same answer for a row whether it is on the
//! screen or far up the history, because a view keeps a row it was sent and must be sent the same one when
//! it asks again.
//!
//! **So there is one reader and it reads the grid directly**, at a point in any of the core's coordinate
//! spaces. The core's render state would read the screen faster, but only the screen: a second reader for
//! history is a second answer to disagree with the first.

use ghostty_vt_rs_sys as sys;

use super::{CLUSTER_CODEPOINTS, Failed, Terminal};
use crate::view::wire::{Color, Style};

/// The longest hyperlink a cell's link is read as; one past this is not a link a person clicks.
const LINK_BYTES: usize = 4096;

/// Which coordinate space a row is named in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// Row zero is the top of the area a program can write to
    Active,
    /// Row zero is the oldest line still held
    Screen,
}

/// One character on a row, self-contained: its own style and link rather than an index into a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRead {
    pub column: u16,
    pub width: u8,
    pub text: String,
    pub style: Style,
    pub link: Option<String>,
}

/// A row as the grid holds it. Blank cells nothing styled are absent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowRead {
    pub wrapped: bool,
    pub prompt: bool,
    pub cells: Vec<CellRead>,
}

/// Where the cursor is and how it is drawn, or nothing while the program has hidden it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRead {
    pub column: u16,
    pub row: u16,
    /// `0` block, `1` bar, `2` underline, `3` hollow
    pub shape: u8,
    pub blinking: bool,
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
    fn reference_in(&self, space: Space, row: u32) -> Option<sys::GridRef> {
        let tag = match space {
            Space::Active => sys::PointTag::ACTIVE,
            Space::Screen => sys::PointTag::SCREEN,
        };
        let point = sys::Point { tag, value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: row } } };
        let mut reference = sys::GridRef::default();
        // SAFETY: `reference` is a valid out-pointer and the point is by value
        let found = unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut reference) };
        (found == sys::Result::SUCCESS).then_some(reference)
    }

    /// The row at `row` in `space`, or `None` where the terminal holds no such row.
    pub fn read_row(&self, space: Space, row: u32) -> Result<Option<RowRead>, Failed> {
        let Some(mut reference) = self.reference_in(space, row) else { return Ok(None) };
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
        let mut read = RowRead { wrapped, prompt: prompt != sys::RowSemanticPrompt::NONE, cells: Vec::new() };
        for column in 0..self.cols()? {
            // **A reference is a row and a column in it**, so the next cell is the same reference one
            // column on. Asking the terminal for each cell by point walks its pages from the top every
            // time, which for a row deep in a long history is most of the cost of reading it.
            reference.x = column;
            if let Some(cell) = self.read_cell(&reference, column)? {
                read.cells.push(cell);
            }
        }
        Ok(Some(read))
    }

    fn read_cell(&self, reference: &sys::GridRef, column: u16) -> Result<Option<CellRead>, Failed> {
        let mut cell: sys::Cell = 0;
        // SAFETY: the reference names a cell of a row that was just looked up
        if unsafe { sys::ghostty_grid_ref_cell(reference, &raw mut cell) } != sys::Result::SUCCESS {
            return Ok(None);
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
        // The cells a wide character leaves beside itself hold nothing of their own
        if wide == sys::CellWide::SPACER_TAIL || wide == sys::CellWide::SPACER_HEAD {
            return Ok(None);
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

        let mut text = String::new();
        match content {
            sys::CellContentTag::CODEPOINT => {
                if let Some(character) = char::from_u32(codepoint).filter(|found| *found != '\0') {
                    text.push(character);
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
                    text.extend(
                        codepoints[..written.min(codepoints.len())].iter().filter_map(|found| char::from_u32(*found)),
                    );
                } else if let Some(character) = char::from_u32(codepoint) {
                    // Longer than a read holds: its base, as `push_cell_text` answers
                    text.push(character);
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

        let link = if linked { self.read_link(reference) } else { None };
        // A blank cell nothing styled or linked is not a character: it is the absence of one
        if text.is_empty() {
            if style == Style::default() && link.is_none() {
                return Ok(None);
            }
            text.push(' ');
        }
        Ok(Some(CellRead { column, width: if wide == sys::CellWide::WIDE { 2 } else { 1 }, text, style, link }))
    }

    fn read_link(&self, reference: &sys::GridRef) -> Option<String> {
        let mut buffer = vec![0u8; 256];
        loop {
            let mut written: usize = 0;
            // SAFETY: the buffer is as long as the length passed
            let result = unsafe {
                sys::ghostty_grid_ref_hyperlink_uri(reference, buffer.as_mut_ptr(), buffer.len(), &raw mut written)
            };
            if result == sys::Result::OUT_OF_SPACE && written > buffer.len() && written <= LINK_BYTES {
                buffer.resize(written, 0);
                continue;
            }
            if result != sys::Result::SUCCESS || written == 0 {
                return None;
            }
            return Some(String::from_utf8_lossy(&buffer[..written.min(buffer.len())]).into_owned());
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

/// A mark on a row that follows it as the terminal scrolls, until the row is dropped.
///
/// **This is what counts the lines that scrolled.** The terminal says how many lines of history it holds,
/// which stops growing once the history is full; a mark set on the screen's first row is found later at
/// some row of the history, and the distance is exactly how many lines moved up past it.
pub struct Mark {
    handle: sys::TrackedGridRef,
}

// As `Terminal`: allocated and freed by the library, and held by one session under its lock
unsafe impl Send for Mark {}

impl Mark {
    /// A mark on the first row a program can write to.
    pub fn at_active_top(terminal: &Terminal) -> Result<Self, Failed> {
        let mut handle: sys::TrackedGridRef = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer and the terminal is live
        let result = unsafe { sys::ghostty_terminal_grid_ref_track(terminal.handle, active_top(), &raw mut handle) };
        Failed::check("ghostty_terminal_grid_ref_track", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_terminal_grid_ref_track", code: sys::Result::SUCCESS });
        }
        Ok(Self { handle })
    }

    /// A mark on the row the cursor is on.
    pub fn at_cursor(terminal: &Terminal) -> Result<Self, Failed> {
        let point = sys::Point {
            tag: sys::PointTag::ACTIVE,
            value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: u32::from(terminal.cursor_row()?) } },
        };
        let mut handle: sys::TrackedGridRef = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer and the terminal is live
        let result = unsafe { sys::ghostty_terminal_grid_ref_track(terminal.handle, point, &raw mut handle) };
        Failed::check("ghostty_terminal_grid_ref_track", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_terminal_grid_ref_track", code: sys::Result::SUCCESS });
        }
        Ok(Self { handle })
    }

    /// Moves the mark back to the first row a program can write to, on whichever screen is showing.
    pub fn reset(&mut self, terminal: &Terminal) -> Result<(), Failed> {
        // SAFETY: both handles are live
        let result = unsafe { sys::ghostty_tracked_grid_ref_set(self.handle, terminal.handle, active_top()) };
        Failed::check("ghostty_tracked_grid_ref_set", result)
    }

    /// The row the mark is on now, counted from the oldest line held, or `None` once that row is gone.
    pub fn screen_row(&self) -> Option<u32> {
        let mut point = sys::PointCoordinate { x: 0, y: 0 };
        // SAFETY: the handle is live and `point` is a valid out-pointer
        let result = unsafe { sys::ghostty_tracked_grid_ref_point(self.handle, sys::PointTag::SCREEN, &raw mut point) };
        (result == sys::Result::SUCCESS).then_some(point.y)
    }
}

impl Drop for Mark {
    fn drop(&mut self) {
        // SAFETY: freed once, by the value that owns it
        unsafe { sys::ghostty_tracked_grid_ref_free(self.handle) };
    }
}

fn active_top() -> sys::Point {
    sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: 0 } },
    }
}

/// The core's own account of what a renderer needs beyond the cells, which here is the cursor.
///
/// **Only the cursor is read from it.** How the cursor is drawn — its shape, whether it blinks — is not
/// something the terminal answers directly; the render state resolves it from the program's request and
/// the defaults.
pub struct Rendering {
    handle: sys::RenderState,
}

// As `Terminal`
unsafe impl Send for Rendering {}

impl Rendering {
    pub fn new() -> Result<Self, Failed> {
        let mut handle: sys::RenderState = std::ptr::null_mut();
        // SAFETY: `handle` is a valid out-pointer, and a null allocator asks for the library's own
        let result = unsafe { sys::ghostty_render_state_new(std::ptr::null(), &raw mut handle) };
        Failed::check("ghostty_render_state_new", result)?;
        if handle.is_null() {
            return Err(Failed { call: "ghostty_render_state_new", code: sys::Result::SUCCESS });
        }
        Ok(Self { handle })
    }

    /// The cursor as the terminal now has it.
    pub fn cursor(&mut self, terminal: &Terminal) -> Result<Option<CursorRead>, Failed> {
        // SAFETY: both handles are live, and the session's lock is what serializes this with a write
        Failed::check("ghostty_render_state_update", unsafe {
            sys::ghostty_render_state_update(self.handle, terminal.handle)
        })?;
        let mut cursor =
            sys::RenderStateCursor { size: std::mem::size_of::<sys::RenderStateCursor>(), ..Default::default() };
        // SAFETY: CURSOR writes a sized `GhosttyRenderStateCursor`, whose size was just set
        Failed::check("ghostty_render_state_get(CURSOR)", unsafe {
            sys::ghostty_render_state_get(self.handle, sys::RenderStateData::CURSOR, (&raw mut cursor).cast())
        })?;
        if !cursor.visible || !cursor.viewport_has_value {
            return Ok(None);
        }
        let shape = match cursor.visual_style {
            sys::CursorStyle::BAR => 1,
            sys::CursorStyle::UNDERLINE => 2,
            sys::CursorStyle::BLOCK_HOLLOW => 3,
            _ => 0,
        };
        Ok(Some(CursorRead { column: cursor.viewport_x, row: cursor.viewport_y, shape, blinking: cursor.blinking }))
    }
}

impl Drop for Rendering {
    fn drop(&mut self) {
        // SAFETY: freed once, by the value that owns it
        unsafe { sys::ghostty_render_state_free(self.handle) };
    }
}
