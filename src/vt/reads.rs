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

/// How many of the core's matches a case-sensitive search reads back to find the exact ones
/// (`Terminal::find`): ten times what it answers, which is where the work of one search and what the
/// core holds for it stop growing with what the history holds.
pub const SEARCH_CANDIDATES: usize = 20_000;

/// What a search found (`Terminal::find`).
#[derive(Debug, Default)]
pub struct Found {
    /// The cells each match covers, newest first.
    pub matches: Vec<(Cell, Cell)>,
    /// Whether there are, or may be, matches older than these: more than are answered, or a search that
    /// stopped before the oldest line.
    pub more: bool,
}

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

    /// The newest places `needle` appears, newest first, as the cells each match covers — `SEARCH_MATCHES`
    /// of them at most — and whether there are, or may be, older ones. Letters match in either case, which
    /// is the core's rule; `case_sensitive` keeps only the matches whose text is the needle exactly, checked
    /// against the text of each.
    ///
    /// **A search stops when it has enough, rather than finding everything and keeping some.** The core
    /// searches the screen and then the history a part at a time, newest first, so it is given parts until
    /// it has what a view is answered with and no further: a single letter looked for in a hundred thousand
    /// lines is the work of finding two thousand of it, not of finding every one.
    ///
    /// **A case-sensitive search looks through `SEARCH_CANDIDATES` of the core's matches and no more.** The
    /// core has no rule for case, so its matches in either case are candidates, each read back to see
    /// whether it is the needle exactly — and the core keeps every one it has found. Looking to the end of
    /// the history whatever it holds is unbounded in both: `benches/search.rs` measured a search for `a`
    /// in a full history of `A` at ten seconds with the session's lock held, and four gigabytes. So it
    /// stops at that many candidates, and an exact match older than all of them is not found, which the
    /// answer says: `more` is true whenever the search stopped short of the oldest line.
    pub fn find(&self, needle: &str, case_sensitive: bool) -> Result<Found, Failed> {
        if needle.is_empty() {
            return Ok(Found::default());
        }
        let mut search: sys::Search = std::ptr::null_mut();
        // SAFETY: `search` is a valid out-pointer; it is freed below, before this returns
        Failed::check("ghostty_search_new", unsafe {
            sys::ghostty_search_new(std::ptr::null(), &raw mut search, self.handle)
        })?;
        let text = sys::String::from(needle);
        let mut found: Vec<sys::Selection> = Vec::new();
        let wanted = if case_sensitive { SEARCH_CANDIDATES } else { SEARCH_MATCHES };
        let mut complete = false;
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
                complete = status == sys::SearchStatus::COMPLETE;
                if complete || total >= wanted {
                    break;
                }
            }
            if result == sys::Result::SUCCESS {
                // The core fills a buffer that holds every match so far or none of it, so it is given
                // room for them all: at most what was wanted and one part's worth more
                found.resize(total, sys::Selection::default());
                let mut buffer = sys::SelectionBuffer { ptr: found.as_mut_ptr(), cap: found.len(), len: 0 };
                result = sys::ghostty_search_get(search, sys::SearchData::MATCHES, (&raw mut buffer).cast());
                found.truncate(if result == sys::Result::SUCCESS { buffer.len.min(found.len()) } else { 0 });
            }
            result
        };
        let mut kept = found
            .iter()
            .filter_map(|selection| self.cells_of(selection))
            .filter(|&(start, end)| !case_sensitive || self.text_between(start, end).is_ok_and(|text| text == needle));
        let matches: Vec<_> = kept.by_ref().take(SEARCH_MATCHES).collect();
        // Older ones there are, where one more was kept than is answered; older ones there may be, where
        // the search stopped before the oldest line
        let more = !complete || kept.next().is_some();
        // SAFETY: freed once, with the terminal still live
        unsafe { sys::ghostty_search_free(search) };
        Failed::check("ghostty_search", result)?;
        Ok(Found { matches, more })
    }
}
