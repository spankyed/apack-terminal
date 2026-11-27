//! Stateless `libghostty-vt` FFI query, row hashing, and Kitty image pixel-copy helpers.

use std::ptr::NonNull;

use apack_proto::types::{BlobHash, ImagePlacement, PaletteOverrides, Rgb};
use ghostty_vt_rs_sys as sys;

use crate::error::GhosttyError;
use crate::rows::{ROW_LINK_BYTES, pack_color, pack_style_flags};

use super::callbacks::percent_decode;

const CLUSTER_CODEPOINTS: usize = 16;

// --- Helper FFI queries ---

/// Hashes all visually observable state of viewport row `row` (wrap/prompt flags, cells, AND
/// side-table styles, grapheme continuations, and hyperlink URIs guarded by fast row-level flags).
pub(super) fn hash_viewport_row(handle: sys::Terminal, row: u32, cols: u16) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let point = sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: row } },
    };
    let mut grid_ref = sys::GridRef::default();
    // SAFETY: `grid_ref` is a valid out-pointer on live `handle`.
    if unsafe { sys::ghostty_terminal_grid_ref(handle, point, &raw mut grid_ref) } != sys::Result::SUCCESS {
        return 0;
    }
    let mut raw_row: sys::Row = 0;
    let mut wrapped = false;
    let mut prompt: sys::RowSemanticPrompt::Type = sys::RowSemanticPrompt::NONE;
    let mut row_styled = false;
    let mut row_grapheme = false;
    let mut row_hyperlink = false;
    // SAFETY: `grid_ref` was just populated; each `RowData` key matches its documented out-pointer type.
    unsafe {
        if sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw_row) == sys::Result::SUCCESS {
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::WRAP, (&raw mut wrapped).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::SEMANTIC_PROMPT, (&raw mut prompt).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::STYLED, (&raw mut row_styled).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::GRAPHEME, (&raw mut row_grapheme).cast());
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::HYPERLINK, (&raw mut row_hyperlink).cast());
        }
    }
    let mut h: u64 =
        0xcbf2_9ce4_8422_2325u64.wrapping_add(u64::from(wrapped)).wrapping_mul(PRIME).wrapping_add(u64::from(prompt));
    let mut link_scratch: Vec<u8> = Vec::new();
    for col in 0..cols {
        grid_ref.x = col;
        let mut cell: sys::Cell = 0;
        // SAFETY: `grid_ref` names a valid column in the resolved row.
        if unsafe { sys::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut cell) } != sys::Result::SUCCESS {
            continue;
        }
        h = h.wrapping_mul(PRIME).wrapping_add(cell);

        if row_styled {
            let mut styled = false;
            // SAFETY: `HAS_STYLING` writes a `bool`.
            let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::HAS_STYLING, (&raw mut styled).cast()) };
            if styled {
                let mut st = sys::Style::default();
                // SAFETY: `grid_ref` is live and `st` is a valid out-pointer.
                let _ = unsafe { sys::ghostty_grid_ref_style(&raw const grid_ref, &raw mut st) };
                h = h
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_color(st.fg_color)))
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_color(st.bg_color)))
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_color(st.underline_color)))
                    .wrapping_mul(PRIME)
                    .wrapping_add(u64::from(pack_style_flags(st.flags)));
            }
        }
        if row_grapheme {
            let mut content: sys::CellContentTag::Type = sys::CellContentTag::CODEPOINT;
            // SAFETY: `CONTENT_TAG` writes a `GhosttyCellContentTag`.
            let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::CONTENT_TAG, (&raw mut content).cast()) };
            if content == sys::CellContentTag::CODEPOINT_GRAPHEME {
                let mut cluster = Vec::new();
                if read_grapheme_codepoints(&grid_ref, &mut cluster) {
                    for &cp in &cluster {
                        h = h.wrapping_mul(PRIME).wrapping_add(u64::from(cp));
                    }
                }
            }
        }
        if row_hyperlink {
            let mut linked = false;
            // SAFETY: `HAS_HYPERLINK` writes a `bool`.
            let _ = unsafe { sys::ghostty_cell_get(cell, sys::CellData::HAS_HYPERLINK, (&raw mut linked).cast()) };
            if linked {
                let mut short = [0u8; 256];
                let mut written = 0usize;
                // SAFETY: `short` is valid for `short.len()` bytes.
                let res = unsafe {
                    sys::ghostty_grid_ref_hyperlink_uri(
                        &raw const grid_ref,
                        short.as_mut_ptr(),
                        short.len(),
                        &raw mut written,
                    )
                };
                let slice: &[u8] = match res {
                    sys::Result::SUCCESS => &short[..written.min(short.len())],
                    sys::Result::OUT_OF_SPACE => {
                        link_scratch.resize(written.min(ROW_LINK_BYTES), 0);
                        // SAFETY: `link_scratch` has `link_scratch.len()` bytes.
                        let r2 = unsafe {
                            sys::ghostty_grid_ref_hyperlink_uri(
                                &raw const grid_ref,
                                link_scratch.as_mut_ptr(),
                                link_scratch.len(),
                                &raw mut written,
                            )
                        };
                        if r2 == sys::Result::SUCCESS { &link_scratch[..written.min(link_scratch.len())] } else { &[] }
                    }
                    _ => &[],
                };
                for &b in slice {
                    h = h.wrapping_mul(PRIME).wrapping_add(u64::from(b));
                }
            }
        }
    }
    h
}

pub(super) fn viewport_row_has_placeholder(handle: sys::Terminal, row: u32) -> bool {
    let point = sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: row } },
    };
    let mut grid_ref = sys::GridRef::default();
    // SAFETY: `grid_ref` is a valid out-pointer on live `handle`.
    if unsafe { sys::ghostty_terminal_grid_ref(handle, point, &raw mut grid_ref) } != sys::Result::SUCCESS {
        return false;
    }
    let mut raw_row: sys::Row = 0;
    let mut has = false;
    // SAFETY: `KITTY_VIRTUAL_PLACEHOLDER` writes a `bool`.
    unsafe {
        if sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw_row) == sys::Result::SUCCESS {
            let _ = sys::ghostty_row_get(raw_row, sys::RowData::KITTY_VIRTUAL_PLACEHOLDER, (&raw mut has).cast());
        }
    }
    has
}

pub(super) fn track_active_top(handle: sys::Terminal) -> Result<sys::TrackedGridRef, GhosttyError> {
    let point = sys::Point {
        tag: sys::PointTag::ACTIVE,
        value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: 0 } },
    };
    let mut mark: sys::TrackedGridRef = std::ptr::null_mut();
    // SAFETY: `handle` is live and `mark` is a valid out-pointer.
    let res = unsafe { sys::ghostty_terminal_grid_ref_track(handle, point, &raw mut mark) };
    GhosttyError::check("ghostty_terminal_grid_ref_track", res)?;
    if mark.is_null() {
        return Err(GhosttyError { call: "ghostty_terminal_grid_ref_track", code: sys::Result::OUT_OF_MEMORY });
    }
    Ok(mark)
}

pub(super) fn tracked_screen_row(mark: sys::TrackedGridRef) -> Option<u32> {
    let mut pt = sys::PointCoordinate { x: 0, y: 0 };
    // SAFETY: `mark` is live and `pt` is a valid out-pointer.
    let res = unsafe { sys::ghostty_tracked_grid_ref_point(mark, sys::PointTag::SCREEN, &raw mut pt) };
    (res == sys::Result::SUCCESS).then_some(pt.y)
}

pub(super) fn read_u16(handle: sys::Terminal, key: sys::TerminalData::Type) -> Option<u16> {
    let mut val = 0u16;
    // SAFETY: caller passes only `u16`-typed `TerminalData` keys (`COLS`, `ROWS`, `CURSOR_X`, `CURSOR_Y`).
    let res = unsafe { sys::ghostty_terminal_get(handle, key, (&raw mut val).cast()) };
    (res == sys::Result::SUCCESS).then_some(val)
}

pub(super) fn query_alt_screen(handle: sys::Terminal) -> bool {
    let mut screen: sys::TerminalScreen::Type = sys::TerminalScreen::PRIMARY;
    // SAFETY: `ACTIVE_SCREEN` writes a `GhosttyTerminalScreen`.
    let res = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::ACTIVE_SCREEN, (&raw mut screen).cast()) };
    res == sys::Result::SUCCESS && screen == sys::TerminalScreen::ALTERNATE
}

pub(super) fn query_scrollback_rows(handle: sys::Terminal) -> u64 {
    let mut rows: usize = 0;
    // SAFETY: `SCROLLBACK_ROWS` writes a `size_t`.
    let res = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::SCROLLBACK_ROWS, (&raw mut rows).cast()) };
    if res == sys::Result::SUCCESS { rows as u64 } else { 0 }
}

pub(super) fn query_mouse_tracking(handle: sys::Terminal) -> bool {
    let mut tracking = false;
    // SAFETY: `MOUSE_TRACKING` writes a `bool`.
    if unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::MOUSE_TRACKING, (&raw mut tracking).cast()) }
        == sys::Result::SUCCESS
    {
        tracking
    } else {
        false
    }
}

pub(super) fn query_color(handle: sys::Terminal, key: sys::TerminalData::Type) -> Option<Rgb> {
    let mut c = sys::ColorRgb { r: 0, g: 0, b: 0 };
    // SAFETY: colour keys write a `GhosttyColorRgb`.
    let res = unsafe { sys::ghostty_terminal_get(handle, key, (&raw mut c).cast()) };
    (res == sys::Result::SUCCESS).then_some(Rgb([c.r, c.g, c.b]))
}

pub(super) fn query_palette_overrides(handle: sys::Terminal) -> PaletteOverrides {
    use sys::TerminalData as D;
    let diff = |now_k: D::Type, def_k: D::Type| {
        let now = query_color(handle, now_k)?;
        (Some(now) != query_color(handle, def_k)).then_some(now)
    };
    let mut now_pal = [sys::ColorRgb { r: 0, g: 0, b: 0 }; 256];
    let mut def_pal = [sys::ColorRgb { r: 0, g: 0, b: 0 }; 256];
    // SAFETY: `COLOR_PALETTE` and `COLOR_PALETTE_DEFAULT` each write 256 `GhosttyColorRgb` entries.
    let _ = unsafe {
        let _ = sys::ghostty_terminal_get(handle, D::COLOR_PALETTE, now_pal.as_mut_ptr().cast());
        sys::ghostty_terminal_get(handle, D::COLOR_PALETTE_DEFAULT, def_pal.as_mut_ptr().cast())
    };
    let palette = (0..=u8::MAX)
        .zip(now_pal.iter().zip(def_pal.iter()))
        .filter(|(_, (n, d))| (n.r, n.g, n.b) != (d.r, d.g, d.b))
        .map(|(idx, (n, _))| (idx, Rgb([n.r, n.g, n.b])))
        .collect();
    PaletteOverrides {
        fg: diff(D::COLOR_FOREGROUND, D::COLOR_FOREGROUND_DEFAULT),
        bg: diff(D::COLOR_BACKGROUND, D::COLOR_BACKGROUND_DEFAULT),
        cursor: diff(D::COLOR_CURSOR, D::COLOR_CURSOR_DEFAULT),
        palette,
    }
}

pub(super) fn query_graphics(handle: sys::Terminal) -> Option<sys::KittyGraphics> {
    let mut graphics: sys::KittyGraphics = std::ptr::null_mut();
    // SAFETY: `KITTY_GRAPHICS` writes a `GhosttyKittyGraphics`.
    let _ = unsafe { sys::ghostty_terminal_get(handle, sys::TerminalData::KITTY_GRAPHICS, (&raw mut graphics).cast()) };
    (!graphics.is_null()).then_some(graphics)
}

pub(super) fn query_images_generation(handle: sys::Terminal) -> u64 {
    let Some(graphics) = query_graphics(handle) else { return 0 };
    let mut generation: u64 = 0;
    // SAFETY: `GENERATION` writes a `uint64_t`.
    let res = unsafe {
        sys::ghostty_kitty_graphics_get(graphics, sys::KittyGraphicsData::GENERATION, (&raw mut generation).cast())
    };
    if res == sys::Result::SUCCESS { generation } else { 0 }
}

pub(super) fn query_image_generation(img: sys::KittyGraphicsImage) -> u64 {
    let mut generation: u64 = 0;
    // SAFETY: `GENERATION` writes a `uint64_t`.
    let _ = unsafe {
        sys::ghostty_kitty_graphics_image_get(
            img,
            sys::KittyGraphicsImageData::GENERATION,
            (&raw mut generation).cast(),
        )
    };
    generation
}

pub(super) fn read_placement_u32(
    iter: sys::KittyGraphicsPlacementIterator,
    key: sys::KittyGraphicsPlacementData::Type,
) -> Option<u32> {
    let mut val = 0u32;
    // SAFETY: caller passes only `uint32_t`-typed placement keys.
    let res = unsafe { sys::ghostty_kitty_graphics_placement_get(iter, key, (&raw mut val).cast()) };
    (res == sys::Result::SUCCESS).then_some(val)
}

pub(super) fn copy_kitty_pixels(img: sys::KittyGraphicsImage) -> Option<(u32, u32, u8, Vec<u8>)> {
    let mut width = 0u32;
    let mut height = 0u32;
    let mut format: sys::KittyImageFormat::Type = sys::KittyImageFormat::RGBA;
    let mut pixels = sys::String::default();
    // SAFETY: each out-pointer matches its documented `KittyGraphicsImageData` type.
    unsafe {
        let _ = sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::WIDTH, (&raw mut width).cast());
        let _ =
            sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::HEIGHT, (&raw mut height).cast());
        let _ =
            sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::FORMAT, (&raw mut format).cast());
        let _ =
            sys::ghostty_kitty_graphics_image_get(img, sys::KittyGraphicsImageData::PIXELS, (&raw mut pixels).cast());
    }
    if pixels.ptr.is_null() {
        return None;
    }
    let channels = match format {
        sys::KittyImageFormat::RGB => 3,
        sys::KittyImageFormat::RGBA => 4,
        _ => return None,
    };
    // SAFETY: `pixels.ptr` is non-null and readable for `pixels.len` bytes while terminal is immutable.
    let slice = unsafe { std::slice::from_raw_parts(pixels.ptr, pixels.len) };
    Some((width, height, channels, slice.to_vec()))
}

pub(super) fn selection_matches_exact(formatter: sys::Formatter, sel: &sys::Selection, needle: &[u8]) -> bool {
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len: usize = 0;
    // SAFETY: `formatter` is live; `SELECTION` copies `sel`; output buffer freed below.
    let res = unsafe {
        let mut r =
            sys::ghostty_formatter_set(formatter, sys::FormatterOption::SELECTION, std::ptr::from_ref(sel).cast());
        if r == sys::Result::SUCCESS {
            r = sys::ghostty_formatter_format_alloc(formatter, std::ptr::null(), &raw mut ptr, &raw mut len);
        }
        r
    };
    let matched = match NonNull::new(ptr).filter(|_| res == sys::Result::SUCCESS) {
        None => needle.is_empty(),
        // SAFETY: `ptr` is non-null and initialized for `len` bytes.
        Some(nn) => unsafe { std::slice::from_raw_parts(nn.as_ptr(), len) == needle },
    };
    // SAFETY: `ghostty_free` accepts null pointers.
    unsafe { sys::ghostty_free(std::ptr::null(), ptr, len) };
    matched
}

/// Decodes OSC 7 `file://host/path` percent-escapes, returning bare paths (OSC 9 / OSC 1337) verbatim.
pub(crate) fn decode_osc7_cwd(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("file://")
        && let Some(slash) = rest.find('/')
    {
        percent_decode(&rest[slash..])
    } else {
        raw.to_owned()
    }
}

/// Maximum grapheme cluster codepoints read on retry when a cluster exceeds `CLUSTER_CODEPOINTS`.
const MAX_CLUSTER_CODEPOINTS: usize = 256;

/// Returns `true` when `handle`'s VT parser and UTF-8 decoder have no pending continuation bytes.
pub(super) fn handle_at_ground(handle: sys::Terminal) -> bool {
    let mut written = 0usize;
    // SAFETY: passing null buffer with length 0 queries continuation byte length cleanly.
    if unsafe { sys::ghostty_terminal_continuation_buf(handle, std::ptr::null_mut(), 0, &raw mut written) }
        == sys::Result::SUCCESS
    {
        written == 0
    } else {
        false
    }
}

/// Reads grapheme cluster codepoints from `grid_ref` into `out`, retrying on `OUT_OF_SPACE` up to
/// [`MAX_CLUSTER_CODEPOINTS`] so clusters with `> 16` codepoints are never silently dropped.
pub(crate) fn read_grapheme_codepoints(grid_ref: &sys::GridRef, out: &mut Vec<u32>) -> bool {
    out.clear();
    let mut stack = [0u32; CLUSTER_CODEPOINTS];
    let mut written = 0usize;
    // SAFETY: `stack` has length `CLUSTER_CODEPOINTS` matching the passed capacity.
    let res = unsafe { sys::ghostty_grid_ref_graphemes(grid_ref, stack.as_mut_ptr(), stack.len(), &raw mut written) };
    if res == sys::Result::SUCCESS {
        out.extend_from_slice(&stack[..written.min(stack.len())]);
        return true;
    }
    if res == sys::Result::OUT_OF_SPACE {
        let cap = written.clamp(stack.len(), MAX_CLUSTER_CODEPOINTS);
        let mut heap = vec![0u32; cap];
        let mut retry_written = 0usize;
        // SAFETY: `heap` has length `cap` matching the passed capacity.
        let retry =
            unsafe { sys::ghostty_grid_ref_graphemes(grid_ref, heap.as_mut_ptr(), heap.len(), &raw mut retry_written) };
        if retry == sys::Result::SUCCESS {
            out.extend_from_slice(&heap[..retry_written.min(heap.len())]);
            return true;
        }
    }
    false
}

/// Resolves the drawn pixel dimensions of an image placement, falling back to `fallback` when headless.
pub(crate) fn resolve_drawn_pixel_size(drawn_ok: bool, px_w: u32, px_h: u32, fallback: [u32; 2]) -> [u32; 2] {
    if drawn_ok && px_w > 0 && px_h > 0 { [px_w, px_h] } else { fallback }
}

/// Builds a wire-valid [`ImagePlacement`], returning `None` for degenerate zero-sized or overflowing placements.
#[allow(clippy::too_many_arguments)]
pub(crate) fn checked_image_placement(
    blob: BlobHash,
    line: u64,
    col: i32,
    source_rect: [u32; 4],
    cell_offset: [u32; 2],
    pixel_size: [u32; 2],
    cols: u32,
    rows: u32,
    z_index: i32,
) -> Option<ImagePlacement> {
    let span_cols = cols.min(u32::from(u16::MAX)) as u16;
    let span_rows = rows.min(u32::from(u16::MAX)) as u16;
    if source_rect[2] == 0
        || source_rect[3] == 0
        || source_rect[0].checked_add(source_rect[2]).is_none()
        || source_rect[1].checked_add(source_rect[3]).is_none()
        || pixel_size[0] == 0
        || pixel_size[1] == 0
        || span_cols == 0
        || span_rows == 0
    {
        return None;
    }
    Some(ImagePlacement {
        blob,
        line,
        col,
        source_rect,
        cell_offset,
        pixel_size,
        cell_span: [span_cols, span_rows],
        z_index,
    })
}
