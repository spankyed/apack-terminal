//! Single-pass `libghostty-vt` grid reader emitting v2 [`PackedRow`]s directly.
//!
//! Reads directly from `libghostty-vt`'s grid coordinate spaces (`ACTIVE` or `SCREEN`) so
//! viewport rows and history rows share one canonical path, merging contiguous cells with
//! identical `(style_idx, link_idx, flags)` into flat [`PackedRun`]s in a single pass.

use std::collections::HashMap;

use apack_proto::types::{BlobHash, CellPx, ImagePiece, PackedRow, PackedRun, PackedStyle};
use ghostty_vt_rs_sys as sys;

use crate::placeholders::{PLACEHOLDER, PlaceholderRun, Said, fit};

/// Maximum UTF-8 bytes of hyperlinks stored in a single `PackedRow` (`64 KiB`).
///
/// Bounded well below `MAX_FRAME_PAYLOAD` (`8 MiB`) so even a 100+ row `full_sync` viewport frame
/// packed with unique OSC 8 hyperlinks stays strictly encodable on the wire.
pub const ROW_LINK_BYTES: usize = 64 * 1024;

/// Resolved Kitty virtual placement metadata for converting placeholder runs into [`ImagePiece`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualImageInfo {
    /// Content-addressed blob hash stored in `ImageStore`.
    pub blob: BlobHash,
    /// Natural image pixel dimensions `(width, height)`.
    pub picture_px: (u32, u32),
    /// Placement grid dimensions `(cols, rows)` (`0` = auto from picture size).
    pub grid: (u32, u32),
}

#[inline]
fn pack_rgb(rgb: sys::ColorRgb) -> u32 {
    u32::from_be_bytes([1, rgb.r, rgb.g, rgb.b])
}

/// Converts a `libghostty-vt` `StyleColor` into `PackedStyle`'s `u32` colour encoding.
pub(crate) fn pack_color(color: sys::StyleColor) -> u32 {
    match u32::from(color.tag) {
        sys::StyleColorTag::PALETTE => {
            // SAFETY: `tag == PALETTE` guarantees `color.value.palette` was initialized.
            let idx = unsafe { color.value.palette };
            u32::from(idx) + 1
        }
        sys::StyleColorTag::RGB => {
            // SAFETY: `tag == RGB` guarantees `color.value.rgb` was initialized.
            let rgb = unsafe { color.value.rgb };
            pack_rgb(rgb)
        }
        _ => 0,
    }
}

/// Maps `libghostty-vt`'s raw 16-bit style flags into v2 [`PackedStyle::flags`].
pub(crate) fn pack_style_flags(raw: u16) -> u32 {
    let mut flags = 0u32;
    if raw & 1 != 0 {
        flags |= PackedStyle::FLAG_BOLD;
    }
    if raw & 2 != 0 {
        flags |= PackedStyle::FLAG_ITALIC;
    }
    if raw & 4 != 0 {
        flags |= PackedStyle::FLAG_FAINT;
    }
    if raw & 8 != 0 {
        flags |= PackedStyle::FLAG_BLINK;
    }
    if raw & 16 != 0 {
        flags |= PackedStyle::FLAG_INVERSE;
    }
    if raw & 32 != 0 {
        flags |= PackedStyle::FLAG_INVISIBLE;
    }
    if raw & 64 != 0 {
        flags |= PackedStyle::FLAG_STRIKETHROUGH;
    }
    let underline = u32::from((raw >> 8) & 7).min(5);
    flags |= underline << 2;
    flags
}

/// Extracts one row from `terminal` at `(tag, raw_row)` into a wire-valid [`PackedRow`] labeled `abs_line`.
pub(crate) fn extract_row(
    terminal: sys::Terminal,
    tag: sys::PointTag::Type,
    raw_row: u32,
    cols: u16,
    abs_line: u64,
    cell_px: CellPx,
    resolve_virtual: &mut impl FnMut(u32, u32) -> Option<VirtualImageInfo>,
) -> Option<PackedRow> {
    let point = sys::Point { tag, value: sys::PointValue { coordinate: sys::PointCoordinate { x: 0, y: raw_row } } };
    let mut grid_ref = sys::GridRef::default();
    // SAFETY: `grid_ref` is a valid out-pointer and `terminal` is live for this call.
    if unsafe { sys::ghostty_terminal_grid_ref(terminal, point, &raw mut grid_ref) } != sys::Result::SUCCESS {
        return None;
    }
    let mut raw: sys::Row = 0;
    // SAFETY: `grid_ref` was just populated and no terminal mutation happens during extraction.
    if unsafe { sys::ghostty_grid_ref_row(&raw const grid_ref, &raw mut raw) } != sys::Result::SUCCESS {
        return None;
    }
    let mut wrapped = false;
    let mut prompt: sys::RowSemanticPrompt::Type = sys::RowSemanticPrompt::NONE;
    // SAFETY: `WRAP` writes `bool` and `SEMANTIC_PROMPT` writes `GhosttyRowSemanticPrompt`.
    unsafe {
        let _ = sys::ghostty_row_get(raw, sys::RowData::WRAP, (&raw mut wrapped).cast());
        let _ = sys::ghostty_row_get(raw, sys::RowData::SEMANTIC_PROMPT, (&raw mut prompt).cast());
    }

    let mut row_flags = 0u8;
    if wrapped {
        row_flags |= PackedRow::FLAG_WRAPPED;
    }
    if prompt != sys::RowSemanticPrompt::NONE {
        row_flags |= PackedRow::FLAG_PROMPT;
    }

    let mut text = String::new();
    let mut runs: Vec<PackedRun> = Vec::new();
    let mut styles: Vec<PackedStyle> = Vec::new();
    let mut style_map: HashMap<[u32; 4], u16> = HashMap::new();
    let mut links: Vec<String> = Vec::new();
    let mut link_map: HashMap<String, u16> = HashMap::new();
    let mut link_bytes = 0usize;
    let mut link_scratch: Vec<u8> = Vec::new();
    let mut placeholders: Vec<PlaceholderRun> = Vec::new();

    for col in 0..cols {
        grid_ref.x = col;
        let mut cell: sys::Cell = 0;
        // SAFETY: `grid_ref` names a valid column in the row resolved above.
        if unsafe { sys::ghostty_grid_ref_cell(&raw const grid_ref, &raw mut cell) } != sys::Result::SUCCESS {
            continue;
        }
        let mut wide: sys::CellWide::Type = sys::CellWide::NARROW;
        let mut content: sys::CellContentTag::Type = sys::CellContentTag::CODEPOINT;
        let mut codepoint: u32 = 0;
        let mut styled = false;
        let mut linked = false;
        // SAFETY: each key matches the exact out-pointer type documented by `libghostty-vt`.
        unsafe {
            let _ = sys::ghostty_cell_get(cell, sys::CellData::WIDE, (&raw mut wide).cast());
            let _ = sys::ghostty_cell_get(cell, sys::CellData::CONTENT_TAG, (&raw mut content).cast());
            let _ = sys::ghostty_cell_get(cell, sys::CellData::CODEPOINT, (&raw mut codepoint).cast());
            let _ = sys::ghostty_cell_get(cell, sys::CellData::HAS_STYLING, (&raw mut styled).cast());
            let _ = sys::ghostty_cell_get(cell, sys::CellData::HAS_HYPERLINK, (&raw mut linked).cast());
        }
        if wide == sys::CellWide::SPACER_TAIL {
            continue;
        }

        let mut style = PackedStyle::default();
        if styled {
            let mut raw_style = sys::Style::default();
            // SAFETY: `grid_ref` is live and `raw_style` is a valid out-pointer.
            if unsafe { sys::ghostty_grid_ref_style(&raw const grid_ref, &raw mut raw_style) } == sys::Result::SUCCESS {
                style = PackedStyle {
                    fg: pack_color(raw_style.fg_color),
                    bg: pack_color(raw_style.bg_color),
                    underline_color: pack_color(raw_style.underline_color),
                    flags: pack_style_flags(raw_style.flags),
                };
            }
        }

        let before = text.len();
        let max_text = apack_proto::MAX_PACKED_ROW_TEXT_BYTES;
        let mut is_grapheme = false;
        match content {
            sys::CellContentTag::CODEPOINT if codepoint == u32::from(PLACEHOLDER) => {
                push_placeholder(&mut placeholders, col, Said::of(&style, []));
                row_flags |= PackedRow::FLAG_HAS_PLACEHOLDER;
                // Placeholder cells encode image/placement IDs in fg/underline_color; strip them
                // so no bogus colored/underlined/inverted space run is painted over the image.
                style = PackedStyle { bg: style.bg, ..PackedStyle::default() };
            }
            sys::CellContentTag::CODEPOINT_GRAPHEME if codepoint == u32::from(PLACEHOLDER) => {
                let mut cps = Vec::new();
                let marks: &[u32] = if crate::engine::read_grapheme_codepoints(&grid_ref, &mut cps) {
                    &cps[1.min(cps.len())..]
                } else {
                    &[]
                };
                push_placeholder(
                    &mut placeholders,
                    col,
                    Said::of(&style, marks.iter().filter_map(|&m| char::from_u32(m))),
                );
                row_flags |= PackedRow::FLAG_HAS_PLACEHOLDER;
                style = PackedStyle { bg: style.bg, ..PackedStyle::default() };
            }
            sys::CellContentTag::CODEPOINT => {
                if let Some(ch) = char::from_u32(codepoint).filter(|&c| c != '\0')
                    && fits_row_text(text.len(), ch.len_utf8(), max_text)
                {
                    text.push(ch);
                }
            }
            sys::CellContentTag::CODEPOINT_GRAPHEME => {
                let mut cps = Vec::new();
                if crate::engine::read_grapheme_codepoints(&grid_ref, &mut cps) {
                    let needed: usize = cps.iter().filter_map(|&c| char::from_u32(c)).map(char::len_utf8).sum();
                    if fits_row_text(text.len(), needed, max_text) {
                        is_grapheme = true;
                        text.extend(cps.iter().filter_map(|&c| char::from_u32(c)));
                    }
                } else if let Some(ch) = char::from_u32(codepoint)
                    && fits_row_text(text.len(), ch.len_utf8(), max_text)
                {
                    is_grapheme = true;
                    text.push(ch);
                }
            }
            sys::CellContentTag::BG_COLOR_PALETTE => {
                let mut idx: sys::ColorPaletteIndex = 0;
                // SAFETY: `COLOR_PALETTE` writes a `GhosttyColorPaletteIndex`.
                if unsafe { sys::ghostty_cell_get(cell, sys::CellData::COLOR_PALETTE, (&raw mut idx).cast()) }
                    == sys::Result::SUCCESS
                {
                    style.bg = u32::from(idx) + 1;
                }
            }
            sys::CellContentTag::BG_COLOR_RGB => {
                let mut rgb = sys::ColorRgb { r: 0, g: 0, b: 0 };
                // SAFETY: `COLOR_RGB` writes a `GhosttyColorRgb`.
                if unsafe { sys::ghostty_cell_get(cell, sys::CellData::COLOR_RGB, (&raw mut rgb).cast()) }
                    == sys::Result::SUCCESS
                {
                    style.bg = pack_rgb(rgb);
                }
            }
            _ => {}
        }

        let link_idx = if linked {
            read_link_index(&grid_ref, &mut links, &mut link_map, &mut link_bytes, &mut link_scratch)
        } else {
            0
        };

        // Blank cell with default style and no hyperlink is omitted completely.
        // Also drop cells if `text` has reached the protocol row-text byte cap.
        if text.len() == before {
            if (style == PackedStyle::default() && link_idx == 0) || !fits_row_text(text.len(), 1, max_text) {
                continue;
            }
            text.push(' ');
        }
        if style.flags & PackedStyle::FLAG_BLINK != 0 {
            row_flags |= PackedRow::FLAG_HAS_BLINK;
        }
        let style_idx = intern_style(style, &mut styles, &mut style_map);
        let cell_width: u16 = if wide == sys::CellWide::WIDE { 2 } else { 1 };
        let run_flags: u8 = u8::from(cell_width > 1 || is_grapheme);
        let byte_len = (text.len() - before) as u32;

        // Merge into the previous run when adjacent and identical simple narrow styling.
        let merged = if run_flags == 0
            && let Some(last) = runs.last_mut()
            && last.flags == 0
            && last.style_idx == style_idx
            && last.link_idx == link_idx
            && last.col_start.saturating_add(last.col_len) == col
        {
            last.col_len = last.col_len.saturating_add(cell_width);
            last.byte_len = last.byte_len.saturating_add(byte_len);
            true
        } else {
            false
        };
        if !merged {
            runs.push(PackedRun {
                col_start: col,
                col_len: cell_width,
                byte_offset: before as u32,
                byte_len,
                style_idx,
                link_idx,
                flags: run_flags,
            });
        }
    }

    let mut pieces = Vec::new();
    let cell_dim = (u32::from(cell_px.width), u32::from(cell_px.height));
    for pr in &placeholders {
        if let Some(info) = resolve_virtual(pr.image(), pr.placement())
            && let Some(fitted) = fit(pr, info.grid, info.picture_px, cell_dim)
        {
            pieces.push(ImagePiece {
                blob: info.blob,
                col_start: pr.column,
                col_len: pr.width,
                src_rect: fitted.src_rect,
                dst_px: fitted.dst_px,
            });
        }
    }

    Some(PackedRow { line: abs_line, flags: row_flags, text, runs, styles, links, pieces })
}

/// Returns `true` when appending `add_bytes` to `current_bytes` stays within `max_bytes`.
#[inline]
pub(crate) fn fits_row_text(current_bytes: usize, add_bytes: usize, max_bytes: usize) -> bool {
    current_bytes.saturating_add(add_bytes) <= max_bytes
}

fn push_placeholder(runs: &mut Vec<PlaceholderRun>, col: u16, said: Said) {
    if let Some(last) = runs.last_mut()
        && last.takes(col, said)
    {
        return;
    }
    runs.push(PlaceholderRun::begin(col, said));
}

fn intern_style(style: PackedStyle, styles: &mut Vec<PackedStyle>, map: &mut HashMap<[u32; 4], u16>) -> u16 {
    if style == PackedStyle::default() {
        return 0;
    }
    let key = [style.fg, style.bg, style.underline_color, style.flags];
    if let Some(&idx) = map.get(&key) {
        return idx;
    }
    if styles.is_empty() {
        styles.push(PackedStyle::default());
    }
    if styles.len() >= 4096 {
        return 0;
    }
    let idx = styles.len() as u16;
    styles.push(style);
    map.insert(key, idx);
    idx
}

fn read_link_index(
    grid_ref: &sys::GridRef,
    links: &mut Vec<String>,
    map: &mut HashMap<String, u16>,
    total_bytes: &mut usize,
    scratch: &mut Vec<u8>,
) -> u16 {
    let mut short = [0u8; 256];
    let mut written = 0usize;
    // SAFETY: `short` is valid for `short.len()` bytes; `grid_ref` is live.
    let res =
        unsafe { sys::ghostty_grid_ref_hyperlink_uri(grid_ref, short.as_mut_ptr(), short.len(), &raw mut written) };
    let uri_slice: &[u8] = match res {
        sys::Result::SUCCESS => &short[..written.min(short.len())],
        sys::Result::OUT_OF_SPACE => {
            scratch.resize(written.min(ROW_LINK_BYTES), 0);
            // SAFETY: `scratch` has `scratch.len()` bytes matching the passed length.
            let res2 = unsafe {
                sys::ghostty_grid_ref_hyperlink_uri(grid_ref, scratch.as_mut_ptr(), scratch.len(), &raw mut written)
            };
            if res2 != sys::Result::SUCCESS {
                return 0;
            }
            &scratch[..written.min(scratch.len())]
        }
        _ => return 0,
    };
    let uri = String::from_utf8_lossy(uri_slice);
    intern_link(&uri, links, map, total_bytes, ROW_LINK_BYTES)
}

pub(crate) fn intern_link(
    uri: &str,
    links: &mut Vec<String>,
    map: &mut HashMap<String, u16>,
    total_bytes: &mut usize,
    max_bytes: usize,
) -> u16 {
    if uri.is_empty() {
        return 0;
    }
    if let Some(&idx) = map.get(uri) {
        return idx;
    }
    if total_bytes.saturating_add(uri.len()) > max_bytes || links.len() >= 1024 {
        return 0;
    }
    if links.is_empty() {
        links.push(String::new());
    }
    let idx = links.len() as u16;
    *total_bytes += uri.len();
    let owned = uri.to_owned();
    map.insert(owned.clone(), idx);
    links.push(owned);
    idx
}
