//! What a view asks about the text on screen: the word under a cell, the line it is on, the text between
//! two cells, and where a string appears.
//!
//! **Asked of the core, not worked out from rows.** What counts as a word, where a line that wrapped
//! begins and ends, what a selection's text is once wrapped rows are joined and trailing blanks dropped —
//! the core has rules for each, and they are the ones a person has in their hands from every other
//! terminal built on it. A view that reimplemented them would agree on the easy cases.
//!
//! Every position here is a cell in **screen** coordinates: a column, and a row counted from the oldest
//! line held.

use std::ptr::NonNull;

use ghostty_vt_rs_sys as sys;

use super::{Failed, Terminal};

/// A cell: its column, and its row counted from the oldest line held.
pub type Cell = (u16, u32);

/// The most matches one search answers with, newest first.
pub const SEARCH_MATCHES: usize = 2_000;

impl Terminal {
    fn reference_at(&self, cell: Cell) -> Option<sys::GridRef> {
        let point = sys::Point {
            tag: sys::PointTag::SCREEN,
            value: sys::PointValue { coordinate: sys::PointCoordinate { x: cell.0, y: cell.1 } },
        };
        let mut reference = sys::GridRef::default();
        // SAFETY: `reference` is a valid out-pointer and the point is by value
        let found = unsafe { sys::ghostty_terminal_grid_ref(self.handle, point, &raw mut reference) };
        (found == sys::Result::SUCCESS).then_some(reference)
    }

    pub(super) fn cell_of(&self, reference: &sys::GridRef) -> Option<Cell> {
        let mut point = sys::PointCoordinate { x: 0, y: 0 };
        // SAFETY: the reference came from this terminal and nothing has written since
        let result = unsafe {
            sys::ghostty_terminal_point_from_grid_ref(self.handle, reference, sys::PointTag::SCREEN, &raw mut point)
        };
        (result == sys::Result::SUCCESS).then_some((point.x, point.y))
    }

    fn cells_of(&self, selection: &sys::Selection) -> Option<(Cell, Cell)> {
        let (start, end) = (self.cell_of(&selection.start)?, self.cell_of(&selection.end)?);
        // A selection made backwards is the same cells; a view is told them in reading order
        Some(if (start.1, start.0) <= (end.1, end.0) { (start, end) } else { (end, start) })
    }

    /// The word at a cell, by the core's own idea of where words end, or `None` where there is none.
    pub fn word_at(&self, cell: Cell) -> Result<Option<(Cell, Cell)>, Failed> {
        let Some(reference) = self.reference_at(cell) else { return Ok(None) };
        let options = sys::TerminalSelectWordOptions {
            size: std::mem::size_of::<sys::TerminalSelectWordOptions>(),
            ref_: reference,
            ..Default::default()
        };
        let mut selection = sys::Selection::default();
        // SAFETY: both structs are sized and live for the call
        let result = unsafe { sys::ghostty_terminal_select_word(self.handle, &raw const options, &raw mut selection) };
        if result == sys::Result::NO_VALUE {
            return Ok(None);
        }
        Failed::check("ghostty_terminal_select_word", result)?;
        Ok(self.cells_of(&selection))
    }

    /// The whole line a cell is on, across every row it wrapped onto.
    pub fn line_at(&self, cell: Cell) -> Result<Option<(Cell, Cell)>, Failed> {
        let Some(reference) = self.reference_at(cell) else { return Ok(None) };
        let options = sys::TerminalSelectLineOptions {
            size: std::mem::size_of::<sys::TerminalSelectLineOptions>(),
            ref_: reference,
            ..Default::default()
        };
        let mut selection = sys::Selection::default();
        // SAFETY: both structs are sized and live for the call
        let result = unsafe { sys::ghostty_terminal_select_line(self.handle, &raw const options, &raw mut selection) };
        if result == sys::Result::NO_VALUE {
            return Ok(None);
        }
        Failed::check("ghostty_terminal_select_line", result)?;
        Ok(self.cells_of(&selection))
    }

    /// The text from one cell to another, both included, as a program wrote it: rows that wrapped joined
    /// into their line, and the blanks after a line's last character dropped.
    pub fn text_between(&self, start: Cell, end: Cell) -> Result<String, Failed> {
        Ok(self.text_within(start, end, usize::MAX)?.unwrap_or_default())
    }

    /// The same, or `Err` of how long the text is when that is more than `limit` bytes: a caller with a
    /// bound on what it can carry is told the size rather than handed a copy it would have to drop.
    pub fn text_within(&self, start: Cell, end: Cell, limit: usize) -> Result<Result<String, usize>, Failed> {
        let (Some(first), Some(last)) = (self.reference_at(start), self.reference_at(end)) else {
            return Ok(Ok(String::new()));
        };
        let selection = sys::Selection { start: first, end: last, rectangle: false };
        let mut formatter: sys::Formatter = std::ptr::null_mut();
        // SAFETY: `formatter` is a valid out-pointer and the terminal outlives it: it is freed below
        Failed::check("ghostty_formatter_terminal_new", unsafe {
            sys::ghostty_formatter_terminal_new(std::ptr::null(), &raw mut formatter, self.handle)
        })?;
        let unwrap = true;
        let mut text: *mut u8 = std::ptr::null_mut();
        let mut length: usize = 0;
        // SAFETY: SELECTION takes a `GhosttySelection*` and UNWRAP a `bool*`, each copied by the call;
        // the output pointers are valid, and what they are given is freed below with the same allocator
        let result = unsafe {
            let mut result =
                sys::ghostty_formatter_set(formatter, sys::FormatterOption::SELECTION, (&raw const selection).cast());
            if result == sys::Result::SUCCESS {
                result =
                    sys::ghostty_formatter_set(formatter, sys::FormatterOption::UNWRAP, (&raw const unwrap).cast());
            }
            if result == sys::Result::SUCCESS {
                result =
                    sys::ghostty_formatter_format_alloc(formatter, std::ptr::null(), &raw mut text, &raw mut length);
            }
            result
        };
        let copied = match NonNull::new(text).filter(|_| result == sys::Result::SUCCESS) {
            None => Ok(String::new()),
            Some(_) if length > limit => Err(length),
            // SAFETY: the library wrote `length` bytes at `text`
            Some(text) => {
                Ok(String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(text.as_ptr(), length) }).into_owned())
            }
        };
        // SAFETY: each is freed once; a null buffer is one `ghostty_free` takes
        unsafe {
            sys::ghostty_free(std::ptr::null(), text, length);
            sys::ghostty_formatter_free(formatter);
        }
        Failed::check("ghostty_formatter_format_alloc", result)?;
        Ok(copied)
    }

    /// The terminal — its screen, the history above it from `from` on, and the state a program left it in —
    /// as the sequences that redraw it in another terminal of the same size. `from` is a row counted from
    /// the oldest line held, and `last` the bottom-right cell.
    ///
    /// **Wrapped rows are written as the one line they are**, so the terminal redrawing them wraps them
    /// itself and knows they are one line: written a row at a time, each would arrive as a line of its own
    /// and a later resize would no longer rejoin them.
    pub fn redraw(&self, from: u32, last: Cell) -> Result<Vec<u8>, Failed> {
        let (Some(first), Some(last)) = (self.reference_at((0, from)), self.reference_at(last)) else {
            return Ok(Vec::new());
        };
        let selection = sys::Selection { start: first, end: last, rectangle: false };
        let mut formatter: sys::Formatter = std::ptr::null_mut();
        // SAFETY: `formatter` is a valid out-pointer and the terminal outlives it: it is freed below
        Failed::check("ghostty_formatter_terminal_new", unsafe {
            sys::ghostty_formatter_terminal_new(std::ptr::null(), &raw mut formatter, self.handle)
        })?;
        let emit: sys::FormatterFormat::Type = sys::FormatterFormat::VT;
        let extras: sys::FormatterExtras = sys::FORMATTER_EXTRAS_ALL;
        let unwrap = true;
        // Untrimmed, so the blank rows below the last line are written too and the cursor lands where it was
        let trim = false;
        let mut text: *mut u8 = std::ptr::null_mut();
        let mut length: usize = 0;
        // SAFETY: each option's value is the type the core documents for it — EMIT a
        // `GhosttyFormatterFormat*`, EXTRAS a `GhosttyFormatterExtras*`, SELECTION a `GhosttySelection*`,
        // UNWRAP and TRIM a `bool*` — and is copied by the call; the output pointers are valid, and what
        // they are given is freed below with the same allocator
        let result = unsafe {
            let options: [(sys::FormatterOption::Type, *const std::ffi::c_void); 5] = [
                (sys::FormatterOption::EMIT, (&raw const emit).cast()),
                (sys::FormatterOption::EXTRAS, (&raw const extras).cast()),
                (sys::FormatterOption::UNWRAP, (&raw const unwrap).cast()),
                (sys::FormatterOption::TRIM, (&raw const trim).cast()),
                (sys::FormatterOption::SELECTION, (&raw const selection).cast()),
            ];
            let mut result = sys::Result::SUCCESS;
            for (option, value) in options {
                // The whole of what is held needs no selection, and is then written with the state before it
                if option == sys::FormatterOption::SELECTION && from == 0 {
                    continue;
                }
                result = sys::ghostty_formatter_set(formatter, option, value);
                if result != sys::Result::SUCCESS {
                    break;
                }
            }
            if result == sys::Result::SUCCESS {
                result =
                    sys::ghostty_formatter_format_alloc(formatter, std::ptr::null(), &raw mut text, &raw mut length);
            }
            result
        };
        let copied = match NonNull::new(text).filter(|_| result == sys::Result::SUCCESS) {
            // SAFETY: the library wrote `length` bytes at `text`
            Some(text) => unsafe { std::slice::from_raw_parts(text.as_ptr(), length) }.to_vec(),
            None => Vec::new(),
        };
        // SAFETY: each is freed once; a null buffer is one `ghostty_free` takes
        unsafe {
            sys::ghostty_free(std::ptr::null(), text, length);
            sys::ghostty_formatter_free(formatter);
        }
        Failed::check("ghostty_formatter_format_alloc", result)?;
        Ok(copied)
    }

    /// Every place `needle` appears, newest first, as the cells each match covers — the newest
    /// `SEARCH_MATCHES` of them. Letters match in either case, which is the core's rule.
    ///
    /// **The search stops when it has enough, rather than finding everything and keeping some.** The core
    /// searches the screen and then the history a part at a time, newest first, so it is given parts until
    /// it has the matches a view is answered with and no further: a single letter looked for in a hundred
    /// thousand lines is the work of finding two thousand of it, not of finding every one.
    pub fn find(&self, needle: &str) -> Result<Vec<(Cell, Cell)>, Failed> {
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut search: sys::Search = std::ptr::null_mut();
        // SAFETY: `search` is a valid out-pointer; it is freed below, before this returns
        Failed::check("ghostty_search_new", unsafe {
            sys::ghostty_search_new(std::ptr::null(), &raw mut search, self.handle)
        })?;
        let text = sys::String::from(needle);
        let mut found: Vec<sys::Selection> = Vec::new();
        // SAFETY: NEEDLE copies its `GhosttyString`; STATUS and TOTAL_MATCHES write the types their
        // out-pointers are; the buffer handed to MATCHES is `cap` entries long
        let result = unsafe {
            let mut result =
                sys::ghostty_search_set(search, sys::SearchOption::GHOSTTY_SEARCH_OPT_NEEDLE, (&raw const text).cast());
            let mut total: usize = 0;
            while result == sys::Result::SUCCESS {
                // One part of the terminal, searched to the end of what was taken
                result = sys::ghostty_search_feed(search);
                let mut status: sys::SearchStatus::Type = sys::SearchStatus::RUNNING;
                while result == sys::Result::SUCCESS && status == sys::SearchStatus::RUNNING {
                    result = sys::ghostty_search_tick(search, &raw mut status);
                }
                if result == sys::Result::SUCCESS {
                    result = sys::ghostty_search_get(search, sys::SearchData::TOTAL_MATCHES, (&raw mut total).cast());
                }
                if status == sys::SearchStatus::COMPLETE || total >= SEARCH_MATCHES {
                    break;
                }
            }
            if result == sys::Result::SUCCESS && total > 0 {
                // The core fills a buffer that holds every match so far or none of it, so it is given
                // room for them all: at most what was wanted and one part's worth more
                found.resize(total, sys::Selection::default());
                let mut buffer = sys::SelectionBuffer { ptr: found.as_mut_ptr(), cap: found.len(), len: 0 };
                result = sys::ghostty_search_get(search, sys::SearchData::MATCHES, (&raw mut buffer).cast());
                found.truncate(if result == sys::Result::SUCCESS { buffer.len.min(found.len()) } else { 0 });
            }
            result
        };
        let matches = found.iter().take(SEARCH_MATCHES).filter_map(|selection| self.cells_of(selection)).collect();
        // SAFETY: freed once, with the terminal still live
        unsafe { sys::ghostty_search_free(search) };
        Failed::check("ghostty_search", result)?;
        Ok(matches)
    }
}
