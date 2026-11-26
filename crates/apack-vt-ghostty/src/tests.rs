use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use apack_core::marks::MarksMode;
use apack_core::session::SessionMachine;
use apack_core::time::TimestampMs;
use apack_core::vt::{CompressStep, PasteEncode, VtEngine, VtEvent};
use apack_proto::messages::{BrokerMsg, FrameUpdate, decode_broker_frame};
use apack_proto::types::{
    CellPos, CellPx, CellSpan, CursorShape, GridSize, ImageEncoding, KeyAction, KeyInput, MarksKey, NonZeroStreamId,
    PackedRow, PackedStyle, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy, SessionId, StreamId,
    SubFlags,
};

use crate::engine::{GhosttyConfig, GhosttyEngine, decode_osc7_cwd, features, last_snapshot_record_end};
use crate::error::GhosttyError;
use crate::image_store::{ImageStore, decode_png_rgba, encode_png_pixels, encode_png_rgba, png_rgba_pixel_count};
use crate::input::consumed;
use crate::placeholders::{PLACEHOLDER, PlaceholderRun, Said, fit};
use crate::rows::intern_link;
use crate::{
    GLOBAL_IMAGE_STORE_BYTES, GLOBAL_SCROLLBACK_PRESSURE_BYTES, SESSION_IMAGE_STORAGE_BYTES, UNKNOWN_OSC_MAX_BYTES,
};

fn grid(cols: u16, rows: u16) -> GridSize {
    GridSize::new(cols, rows).unwrap()
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn encode_test_png(width: u32, height: u32, color: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(color);
        enc.set_depth(depth);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(data).unwrap();
    }
    out
}

#[test]
fn constants_features_and_error_formatting() {
    assert_eq!(GLOBAL_IMAGE_STORE_BYTES, 256 * 1024 * 1024);
    assert_eq!(SESSION_IMAGE_STORAGE_BYTES, 64 * 1024 * 1024);
    assert_eq!(GLOBAL_SCROLLBACK_PRESSURE_BYTES, 512 * 1024 * 1024);
    assert_eq!(UNKNOWN_OSC_MAX_BYTES, 4096);

    let f = features();
    assert!(f.render_state);
    assert!(f.formatter);
    assert!(f.selection);
    assert!(f.search);
    assert!(f.input_encode);
    assert!(f.grid_introspection);
    assert!(f.snapshot);
    assert!(f.kitty_graphics);

    let unsup = GhosttyError { call: "test_fn", code: ghostty_vt_rs_sys::Result::UNSUPPORTED };
    assert!(unsup.to_string().contains("unsupported"));
    let other = GhosttyError { call: "test_fn", code: ghostty_vt_rs_sys::Result::OUT_OF_MEMORY };
    assert!(!other.to_string().contains("unsupported"));
    assert!(GhosttyError::check("ok", ghostty_vt_rs_sys::Result::SUCCESS).is_ok());
    assert!(GhosttyError::check("err", ghostty_vt_rs_sys::Result::OUT_OF_SPACE).is_err());
}

#[test]
fn packed_row_individual_styles_bg_tags_links_graphemes_wrapping_and_wire_roundtrip() {
    let mut vt = GhosttyEngine::new(GhosttyConfig::new(grid(40, 12))).unwrap();
    let mut ev = Vec::new();

    // Row 0: each SGR attribute isolated on a single character, plus palette fg (196 -> 197).
    vt.feed(
        b"\x1b[1mB\x1b[0m\x1b[3mI\x1b[0m\x1b[2mF\x1b[0m\x1b[5mK\x1b[0m\x1b[7mV\x1b[0m\x1b[8mH\x1b[0m\x1b[9mS\x1b[0m\x1b[4:3mU\x1b[0m\x1b[38;5;196mP\x1b[0m",
        &mut ev,
    );
    // Row 1: plain adjacent chars (coalesce), gap, hyperlinks (short dup + long), wide char, narrow grapheme, ZWJ grapheme.
    let long_url = format!("https://example.com/{}", "x".repeat(300));
    let short_url = "https://short.test/a";
    let row1 = format!(
        "\r\nABCD\x1b[2C\x1b]8;;{short_url}\x1b\\L1\x1b]8;;\x1b\\ \x1b]8;;{short_url}\x1b\\L2\x1b]8;;\x1b\\ \
         \x1b]8;;{long_url}\x1b\\L3\x1b]8;;\x1b\\ 中e\u{0301}👨‍👩‍👧"
    );
    vt.feed(row1.as_bytes(), &mut ev);
    // Row 2: palette & RGB erased backgrounds (Ghostty BG_COLOR_PALETTE & BG_COLOR_RGB cell tags).
    vt.feed(b"\r\n\x1b[48;5;42m\x1b[2X\x1b[4C\x1b[48;2;12;34;56m\x1b[2X\x1b[0m", &mut ev);
    // Row 3: OSC 133;A semantic prompt mark + long wrapped line onto row 4.
    vt.feed(format!("\r\n\x1b]133;A\x07{}", "W".repeat(45)).as_bytes(), &mut ev);

    let mut rows = Vec::new();
    vt.extract_viewport_rows(&[0, 1, 2, 3, 4], &mut rows);
    assert_eq!(rows.len(), 5);

    // Row 0 checks: exact text, individual flags, and palette fg (196 -> 197)
    let r0 = &rows[0];
    assert_eq!(r0.text, "BIFKVHSUP");
    assert_eq!(r0.runs.len(), 9);
    assert!(r0.flags & PackedRow::FLAG_HAS_BLINK != 0);
    let expected_flags = [
        PackedStyle::FLAG_BOLD,
        PackedStyle::FLAG_ITALIC,
        PackedStyle::FLAG_FAINT,
        PackedStyle::FLAG_BLINK,
        PackedStyle::FLAG_INVERSE,
        PackedStyle::FLAG_INVISIBLE,
        PackedStyle::FLAG_STRIKETHROUGH,
        3 << 2, // curly underline
    ];
    for (run, &want) in r0.runs.iter().zip(&expected_flags) {
        assert_eq!(r0.styles[usize::from(run.style_idx)].flags, want);
    }
    assert_eq!(r0.styles[usize::from(r0.runs[8].style_idx)].fg, 197);

    // Row 1 checks: run coalescing, gaps, deduplicated short link, long link, wide char, grapheme
    let r1 = &rows[1];
    assert_eq!(r1.flags & PackedRow::FLAG_HAS_BLINK, 0);
    assert_eq!(r1.runs[0].col_start, 0);
    assert_eq!(r1.runs[0].col_len, 4);
    assert_eq!(r1.runs[0].byte_len, 4);
    assert_eq!(r1.runs[0].flags, 0);
    assert_eq!(&r1.text[..4], "ABCD");
    // Gap at cols 4..6 splits runs; L1 starts at col 6 with link_idx=1
    let l1 = r1.runs.iter().find(|r| r.col_start == 6).unwrap();
    assert_eq!(l1.link_idx, 1);
    let l2 = r1.runs.iter().find(|r| r.col_start == 9).unwrap();
    assert_eq!(l2.link_idx, 1); // deduplicated!
    let l3 = r1.runs.iter().find(|r| r.col_start == 12).unwrap();
    assert_eq!(l3.link_idx, 2);
    assert_eq!(r1.links.len(), 3); // ["", short_url, long_url]
    assert_eq!(r1.links[1], short_url);
    assert_eq!(r1.links[2], long_url);
    // Wide non-grapheme char `'中'` (col_len=2, flags=1) AND narrow combining grapheme `e\u{0301}` (col_len=1, flags=1)
    assert!(r1.text.contains('中'));
    assert!(r1.text.contains("e\u{0301}"));
    assert!(r1.text.contains("👨‍👩‍👧"));
    let wide_cjk = r1
        .runs
        .iter()
        .find(|r| &r1.text[r.byte_offset as usize..(r.byte_offset + r.byte_len) as usize] == "中")
        .unwrap();
    assert_eq!(wide_cjk.col_len, 2);
    assert_eq!(wide_cjk.flags, 1);
    let narrow_graph = r1
        .runs
        .iter()
        .find(|r| &r1.text[r.byte_offset as usize..(r.byte_offset + r.byte_len) as usize] == "e\u{0301}")
        .unwrap();
    assert_eq!(narrow_graph.col_len, 1);
    assert_eq!(narrow_graph.flags, 1);

    // Direct `intern_link` budget and count-cap boundary tests
    let mut link_vec = Vec::new();
    let mut link_map = HashMap::new();
    let mut link_bytes = 0usize;
    assert_eq!(intern_link(String::new(), &mut link_vec, &mut link_map, &mut link_bytes, 10), 0);
    assert_eq!(intern_link("abcd".into(), &mut link_vec, &mut link_map, &mut link_bytes, 7), 1);
    assert_eq!(intern_link("abcd".into(), &mut link_vec, &mut link_map, &mut link_bytes, 7), 1);
    assert_eq!(intern_link("efg".into(), &mut link_vec, &mut link_map, &mut link_bytes, 7), 2);
    assert_eq!(link_bytes, 7);
    assert_eq!(intern_link("h".into(), &mut link_vec, &mut link_map, &mut link_bytes, 7), 0);
    // 1024-link cap (index 0 is "", indices 1..=1023 fit, 1024th rejected)
    let mut cap_vec = Vec::new();
    let mut cap_map = HashMap::new();
    let mut cap_bytes = 0usize;
    for i in 0..1023 {
        assert_eq!(intern_link(format!("u{i}"), &mut cap_vec, &mut cap_map, &mut cap_bytes, 1 << 20), (i + 1) as u16);
    }
    assert_eq!(intern_link("overflow".into(), &mut cap_vec, &mut cap_map, &mut cap_bytes, 1 << 20), 0);

    // Row 2 checks: BG_COLOR_PALETTE (42 -> 43) and BG_COLOR_RGB (12,34,56 -> 0x01_0C_22_38)
    let r2 = &rows[2];
    assert!(r2.styles.iter().any(|s| s.bg == 43));
    assert!(r2.styles.iter().any(|s| s.bg == 0x01_0C_22_38));
    // Verify palette & RGB cells are emitted as space runs with matching bg
    assert_eq!(r2.runs[0].col_start, 0);
    assert_eq!(r2.runs[0].col_len, 2);
    assert_eq!(r2.styles[usize::from(r2.runs[0].style_idx)].bg, 43);
    assert_eq!(r2.runs[1].col_start, 4);
    assert_eq!(r2.runs[1].col_len, 2);
    assert_eq!(r2.styles[usize::from(r2.runs[1].style_idx)].bg, 0x01_0C_22_38);

    // Row 3 & 4 checks: prompt flag + wrap flag + text_of unwrapping!
    assert!(rows[3].flags & PackedRow::FLAG_PROMPT != 0);
    assert!(rows[3].flags & PackedRow::FLAG_WRAPPED != 0);
    assert_eq!(rows[4].flags & PackedRow::FLAG_WRAPPED, 0);
    let unwrapped = vt.text_of(CellSpan { start: CellPos { line: 3, col: 0 }, end: CellPos { line: 4, col: 4 } });
    assert_eq!(unwrapped, "W".repeat(45));

    // Wire round-trip validation of all emitted PackedRows
    let meta = vt.meta();
    let frame = BrokerMsg::Frame(Box::new(FrameUpdate {
        seq: 1,
        epoch: meta.epoch,
        size: meta.size,
        first_active_line: meta.first_active_line,
        scrollback_lines: meta.scrollback_lines,
        scroll_delta: 0,
        cursor: meta.cursor,
        modes: meta.modes,
        palette: Some(vt.palette_overrides()),
        rows,
        images_gen: meta.images_gen,
        full_sync: true,
    }));
    let mut wire = Vec::new();
    frame.encode_to_frame(StreamId::from_raw(1), RequestId::PUSH, &mut wire).unwrap();
    let mut dec = apack_proto::frame::FrameDecoder::new();
    let (used, raw) = dec.step(&wire).unwrap();
    assert_eq!(used, wire.len());
    let (_, decoded) = decode_broker_frame(raw.unwrap()).unwrap().unwrap();
    assert_eq!(decoded, frame);
}

#[test]
fn scroll_tracking_dirty_shifting_clear_screen_alt_screen_and_resize_semantics() {
    let mut vt = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    let mut ev = Vec::new();

    // Clear initial full_dirty
    let d0 = vt.take_dirty_viewport();
    assert!(d0.full_dirty);
    let grid_gen0 = vt.generations().grid;

    // Partial update: touch row 0 and row 2 without scrolling
    vt.feed(b"row0\x1b[3;1Hrow2", &mut ev);
    let d1 = vt.take_dirty_viewport();
    assert!(!d1.full_dirty);
    assert_eq!(d1.scroll_delta, 0);
    assert!(d1.dirty_rows.contains(&0));
    assert!(d1.dirty_rows.contains(&2));
    assert!(vt.generations().grid > grid_gen0);

    // Dirty ONLY row 2, then scroll by 1 BEFORE taking dirty -> row 2 shifts to row 1 (not row 3!), row 3 is new!
    vt.feed(b"\x1b[3;1HB", &mut ev);
    let g_scroll0 = vt.generations().grid;
    vt.feed(b"\x1b[4;1H\r\n", &mut ev);
    let d2 = vt.take_dirty_viewport();
    assert!(!d2.full_dirty);
    assert_eq!(d2.scroll_delta, 1);
    assert_eq!(vt.meta().first_active_line, 1);
    assert_eq!(vt.generations().grid, g_scroll0 + 1);
    assert!(d2.dirty_rows.contains(&1), "dirty row 2 must shift up to row 1 (got {:?})", d2.dirty_rows);
    assert!(d2.dirty_rows.contains(&3), "newly scrolled-in bottom row 3 must be dirty (got {:?})", d2.dirty_rows);
    assert!(!d2.dirty_rows.contains(&0), "unchanged row 0 must stay clean (got {:?})", d2.dirty_rows);
    assert!(!d2.dirty_rows.contains(&2), "unchanged row 2 must stay clean (got {:?})", d2.dirty_rows);

    // Mutate upper row 0 AND scroll by 1 in ONE feed while row 1 stays untouched -> hash_viewport_row catches row 0, skips row 1!
    vt.feed(b"\x1b[1;1HrowA\x1b[2;1HrowB\x1b[3;1HrowC", &mut ev);
    let _ = vt.take_dirty_viewport();
    let g_scroll1 = vt.generations().grid;
    // Scroll up 1 (`rowB` moves to row 0, `rowC` moves to row 1), AND modify row 0 (`rowB` -> `MOD!`) and row 1's prompt flag
    vt.feed(b"\x1b[4;1H\r\n\x1b[1;1HMOD!\x1b[2;1H\x1b]133;A\x07", &mut ev);
    let d3 = vt.take_dirty_viewport();
    assert!(!d3.full_dirty);
    assert_eq!(d3.scroll_delta, 1);
    assert_eq!(vt.meta().first_active_line, 2);
    assert_eq!(vt.generations().grid, g_scroll1 + 1);
    assert!(d3.dirty_rows.contains(&0), "modified upper row 0 must be detected by row hash (got {:?})", d3.dirty_rows);
    assert!(
        d3.dirty_rows.contains(&1),
        "row 1 semantic prompt bit change must be detected by row hash (got {:?})",
        d3.dirty_rows
    );
    assert!(!d3.dirty_rows.contains(&2), "unchanged row 2 must NOT be dirty (got {:?})", d3.dirty_rows);
    assert!(d3.dirty_rows.contains(&3), "new bottom row 3 must be dirty (got {:?})", d3.dirty_rows);

    // Clear upper row 0 back to blank AND scroll by 1 -> hash detects blanking too
    vt.feed(b"\x1b[4;1H\r\n\x1b[1;1H\x1b[2K", &mut ev);
    let d3b = vt.take_dirty_viewport();
    assert!(!d3b.full_dirty);
    assert_eq!(d3b.scroll_delta, 1);
    assert!(d3b.dirty_rows.contains(&0));
    assert!(!d3b.dirty_rows.contains(&1));

    // Scroll by 1 twice on blank bottom rows -> hashes match, so ONLY `r >= keep` marks bottom row 3 dirty!
    vt.feed(b"\x1b[1;1H\x1b[2J\x1b[4;1H\r\n", &mut ev);
    let _ = vt.take_dirty_viewport();
    vt.feed(b"\x1b[4;1H\r\n", &mut ev);
    let d3c = vt.take_dirty_viewport();
    assert!(!d3c.full_dirty);
    assert_eq!(d3c.scroll_delta, 1);
    assert_eq!(d3c.dirty_rows, vec![3], "newly scrolled-in blank bottom row must be marked dirty by `r >= keep`");

    // Scroll by >= rows (4) lines in one feed degrades cleanly to full_dirty with scroll_delta == 0
    vt.feed(b"\x1b[4;1H\r\n\r\n\r\n\r\n", &mut ev);
    let d4 = vt.take_dirty_viewport();
    assert!(d4.full_dirty);
    assert_eq!(d4.scroll_delta, 0);
    assert!(d4.dirty_rows.is_empty());
    assert_eq!(vt.meta().first_active_line, 9);

    // Clear screen (ED 2) triggers full dirty via render_state DIRTY::FULL
    let grid_before_ed = vt.generations().grid;
    vt.feed(b"\x1b[2J", &mut ev);
    let d5 = vt.take_dirty_viewport();
    assert!(d5.full_dirty);
    assert!(vt.generations().grid > grid_before_ed);

    // Cursor shapes (Block, Bar, Underline, Hollow) & hidden toggle
    vt.set_cursor_defaults(CursorShape::Bar, true);
    vt.feed(b"\x1b[0 q", &mut ev);
    assert_eq!(vt.meta().cursor.shape, CursorShape::Bar);
    vt.set_cursor_defaults(CursorShape::Underline, true);
    vt.feed(b"\x1b[0 q", &mut ev);
    assert_eq!(vt.meta().cursor.shape, CursorShape::Underline);
    assert!(!vt.meta().cursor.hidden);
    vt.set_cursor_defaults(CursorShape::Hollow, false);
    vt.feed(b"\x1b[0 q\x1b[?25l", &mut ev);
    assert_eq!(vt.meta().cursor.shape, CursorShape::Hollow);
    assert!(vt.meta().cursor.hidden);
    vt.feed(b"\x1b[?25h", &mut ev);
    assert!(!vt.meta().cursor.hidden);

    // Alt-screen enter + scroll inside alt-screen (must NOT advance first_active_line!) + leave bumps epoch 1 -> 2 -> 3
    let modes_gen0 = vt.generations().modes;
    ev.clear();
    vt.feed(b"\x1b[?1049hALT\r\n\r\n\r\n\r\n\r\n", &mut ev);
    assert!(ev.contains(&VtEvent::EpochChanged));
    assert_eq!(vt.meta().epoch, 2);
    assert!(vt.meta().modes.alt_screen);
    assert_eq!(vt.meta().first_active_line, 0);
    assert_eq!(vt.generations().modes, modes_gen0 + 1);
    ev.clear();
    vt.feed(b"\x1b[?1049l", &mut ev);
    assert!(ev.contains(&VtEvent::EpochChanged));
    assert_eq!(vt.meta().epoch, 3);
    assert!(!vt.meta().modes.alt_screen);
    assert_eq!(vt.generations().modes, modes_gen0 + 2);

    // Resize: identical (size, cell_px) is a no-op; cell_px-only change does NOT bump epoch; grid change bumps epoch
    let ep = vt.meta().epoch;
    vt.resize(grid(20, 4), CellPx::default());
    assert_eq!(vt.meta().epoch, ep);
    vt.resize(grid(20, 4), CellPx { width: 9, height: 18 });
    assert_eq!(vt.meta().epoch, ep);
    ev.clear();
    vt.feed(b"\x1b[16t", &mut ev);
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[6;18;9t")));
    vt.resize(grid(24, 5), CellPx { width: 9, height: 18 });
    assert_eq!(vt.meta().epoch, ep + 1);
    assert_eq!(vt.meta().size, grid(24, 5));
}

#[test]
fn scrollback_eviction_compression_queries_rect_clamping_and_search_boundaries() {
    let mut cfg = GhosttyConfig::new(grid(80, 4));
    cfg.scrollback_lines = 100;
    let mut vt = GhosttyEngine::new(cfg).unwrap();
    let mut ev = Vec::new();

    for i in 0..2_000 {
        vt.feed(format!("line-{i:04} AlphaBeta\r\n").as_bytes(), &mut ev);
    }
    let meta = vt.meta();
    assert_eq!(meta.epoch, 1);
    assert!(meta.scrollback_lines < 1_000, "scrollback must evict old pages (got {})", meta.scrollback_lines);
    assert!(meta.first_active_line >= 1_990);
    let oldest = meta.oldest_line();
    assert!(oldest > 0);

    // Memory accounting & cooperative compression
    let mut fresh = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    assert_eq!(fresh.compress_step(2), CompressStep::Done);
    let before_bytes = vt.scrollback_bytes();
    assert!(before_bytes > 1024);
    assert_eq!(vt.compress_step(1), CompressStep::More);
    let mut steps = 0;
    while vt.compress_step(4) == CompressStep::More && steps < 64 {
        steps += 1;
    }
    assert!(steps < 64, "compression must terminate cleanly (steps={steps})");

    // Out-of-range epoch line queries return empty/None cleanly
    assert!(vt.line_at(oldest - 1).is_none());
    assert!(vt.line_at(meta.total_lines()).is_none());
    let mut hist = Vec::new();
    vt.extract_history_rows(oldest, 2, &mut hist);
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].line, oldest);
    assert_eq!(hist[1].line, oldest + 1);

    // word_at, line_at, text_of, text_of_rect (including col > cols-1 clamping)
    let word = vt.word_at(CellPos { line: oldest, col: 12 }, " ").unwrap();
    assert_eq!(vt.text_of(word), "AlphaBeta");
    let line_span = vt.line_at(oldest).unwrap();
    assert!(vt.text_of(line_span).contains("AlphaBeta"));
    let rect = vt.text_of_rect(CellSpan {
        start: CellPos { line: oldest, col: 0 },
        end: CellPos { line: oldest + 1, col: 200 },
    });
    assert!(rect.contains("AlphaBeta\n"));
    assert_eq!(rect.lines().count(), 2);

    // Search edge cases: empty needle, empty range, zero limit
    let mut out = Vec::new();
    assert!(!vt.search_lines(oldest, meta.total_lines(), "", true, 10, &mut out));
    assert!(!vt.search_lines(oldest, oldest, "AlphaBeta", true, 10, &mut out));
    assert!(!vt.search_lines(oldest, meta.total_lines(), "AlphaBeta", true, 0, &mut out));
    assert!(out.is_empty());

    // Exact range boundary filtering (`[oldest+1, oldest+4)` -> exactly 3 matches)
    let trunc_none = vt.search_lines(oldest + 1, oldest + 4, "AlphaBeta", true, 3, &mut out);
    assert!(!trunc_none);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].start.line, oldest + 1);
    assert_eq!(out[2].start.line, oldest + 3);

    // Truncation when matches > limit
    out.clear();
    let trunc_yes = vt.search_lines(oldest + 1, oldest + 4, "AlphaBeta", true, 2, &mut out);
    assert!(trunc_yes);
    assert_eq!(out.len(), 2);

    // Case-sensitive miss vs case-insensitive hit, plus completely absent needle (`total == 0`)
    out.clear();
    assert!(!vt.search_lines(oldest + 1, oldest + 4, "ZZZZZZ", true, 10, &mut out));
    assert!(out.is_empty());
    assert!(!vt.search_lines(oldest + 1, oldest + 4, "alphabeta", true, 10, &mut out));
    assert!(out.is_empty());
    assert!(!vt.search_lines(oldest + 1, oldest + 4, "alphabeta", false, 10, &mut out));
    assert_eq!(out.len(), 3);

    // Multi-page search (>1 page of scrollback) + wrapped case-sensitive match (verifies UNWRAP on formatter & multi-pass search!)
    let mut cfg_big = GhosttyConfig::new(grid(40, 4));
    cfg_big.scrollback_lines = 4_000;
    let mut big = GhosttyEngine::new(cfg_big).unwrap();
    let mut big_ev = Vec::new();
    big.feed(format!("{}WrappedCaseNeedle\r\n", "x".repeat(32)).as_bytes(), &mut big_ev);
    for i in 0..2_000 {
        big.feed(format!("mp-row-{i:04} filler-text-for-pages\r\n").as_bytes(), &mut big_ev);
    }
    big.feed(b"TailAnchorHit\r\n", &mut big_ev);
    let bm = big.meta();
    let mut wrapped_hits = Vec::new();
    assert!(!big.search_lines(bm.oldest_line(), bm.total_lines(), "WrappedCaseNeedle", true, 10, &mut wrapped_hits));
    assert_eq!(wrapped_hits.len(), 1);
    assert!(wrapped_hits[0].end.line > wrapped_hits[0].start.line);
    let mut mp_hits = Vec::new();
    assert!(!big.search_lines(bm.oldest_line(), bm.total_lines(), "mp-row-", true, 2_500, &mut mp_hits));
    assert_eq!(mp_hits.len(), 2_000);
}

#[test]
fn callbacks_marks_status_palette_osc7_and_session_machine_integration() {
    let mut cfg = GhosttyConfig::new(grid(40, 6));
    cfg.cell_px = CellPx { width: 8, height: 16 };
    let mut vt = GhosttyEngine::new(cfg).unwrap();
    let mut ev = Vec::new();

    assert!(!vt.meta().modes.mouse);

    let stream = concat!(
        "\x1b]2;my-win\x07",
        "\x07",
        "\x1b]7;file://localhost/tmp/hello%20world\x07",
        "\x1b]52;c;aGVsbG8=\x07",
        "\x1b]777;notify;Alert;Done!\x07",
        "\x1b]9;4;1;75\x07",
        "\x1b]7501;state=blocked:kind=permission:progress=50:app=claude:title=UnVu:msg=QXBwbHk/\x1b\\",
        "\x1b[?2026h\x1b[?2026l",
        "\x1b[6n\x1b[16t",
        "\x1b]10;#112233\x07\x1b]4;1;#aa5500\x07",
    );
    vt.feed(stream.as_bytes(), &mut ev);

    assert!(ev.iter().any(|e| matches!(e, VtEvent::Title(t) if t == "my-win")));
    assert!(ev.contains(&VtEvent::Bell));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::Cwd(c) if c == "/tmp/hello world")));
    assert!(
        ev.iter().any(|e| matches!(e, VtEvent::Clipboard { selection, data } if selection == "c" && data == "hello"))
    );
    assert!(
        ev.iter().any(|e| matches!(e, VtEvent::Notification { title, body } if title == "Alert" && body == "Done!"))
    );
    assert!(ev.iter().any(|e| matches!(e, VtEvent::Progress { state: ProgressState::Running, percent: Some(75) })));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::Osc7501(s) if s.contains("state=blocked") && s.contains("kind=permission") && s.contains("app=claude"))));
    assert!(ev.contains(&VtEvent::SyncOutput(true)));
    assert!(ev.contains(&VtEvent::SyncOutput(false)));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[1;1R")));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[6;16;8t")));

    let pal = vt.palette_overrides();
    assert_eq!(pal.fg, Some(apack_proto::types::Rgb([0x11, 0x22, 0x33])));
    assert!(pal.palette.contains(&(1, apack_proto::types::Rgb([0xAA, 0x55, 0x00]))));
    assert!(vt.generations().palette > 0);

    // Percent-decoding lower/upper hex digits and malformed escapes
    assert_eq!(decode_osc7_cwd("file://host/a%2fb%2Fc%7ed%7Ee%0Af%zz%2"), "/a/b/c~d~e\nf%zz%2");

    // End-to-end SessionMachine<GhosttyEngine> with keyed OSC 633;V marks in ONE chunk
    let key = MarksKey::parse(b"0123456789abcdef0123456789abcdef").unwrap();
    let vt2 = GhosttyEngine::new(GhosttyConfig::new(grid(40, 8))).unwrap();
    let mut sess = SessionMachine::new(
        SessionId::new(1).unwrap(),
        vt2,
        ResizePolicy::LatestActive,
        MarksMode::Keyed(key),
        CellPx::default(),
    );
    let k1 = (apack_core::lease::ConnId::new(1).unwrap(), NonZeroStreamId::new(1).unwrap());
    let mut fx = Vec::new();
    sess.attach_stream(k1, SubFlags::from_bits(SubFlags::VIEW | SubFlags::EVENTS).unwrap(), None, &mut fx);
    fx.clear();

    let chunk = format!(
        "\x1b]633;V;S;1;{};echo hi\x07line-one\r\nline-two\r\n\x1b]633;V;F;1;{};0;/work\x07",
        key.as_str(),
        key.as_str()
    );
    sess.on_pty_output(chunk.as_bytes(), TimestampMs::from_millis(100), &mut fx);
    let mut out_fx = Vec::new();
    sess.handle_stream_msg(
        k1,
        RequestId::from_raw(1),
        apack_proto::messages::ClientMsg::CommandOutput { command_id: 1 },
        &mut out_fx,
    );
    assert!(out_fx.iter().any(|e| matches!(
        e,
        apack_core::session::SessionEffect::Send { msg: BrokerMsg::CommandOutputReply { text: Some(t) }, .. }
        if t.contains("line-one") && t.contains("line-two")
    )));
}

#[test]
fn input_encoders_keys_kitty_protocol_pointer_buttons_motion_wheel_focus_and_large_paste() {
    let mut vt = GhosttyEngine::new(GhosttyConfig::new(grid(40, 10))).unwrap();
    let mut ev = Vec::new();
    let mut out = Vec::new();

    // Standard keys: ArrowUp, Ctrl+C, Shift+A (consumed Shift), Alt+Shift composed, macOS fn char filtered
    vt.encode_key(
        &KeyInput {
            code: "ArrowUp".into(),
            action: KeyAction::Press,
            modifiers: 0,
            text: String::new(),
            unshifted_codepoint: 0,
            composed: false,
        },
        &mut out,
    );
    assert_eq!(out, b"\x1b[A");

    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyC".into(),
            action: KeyAction::Press,
            modifiers: 2, // Ctrl
            text: "\x03".into(),
            unshifted_codepoint: u32::from('c'),
            composed: false,
        },
        &mut out,
    );
    assert_eq!(out, b"\x03");

    // Enable Kitty keyboard full flags (`\x1b[>31u`) to test Release text suppression, Shift consumed,
    // composed Alt+Shift consumed, Ctrl+Shift NOT consumed, and control/macOS-fn `typed()` filtering!
    vt.feed(b"\x1b[>31u", &mut ev);
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyA".into(),
            action: KeyAction::Release,
            modifiers: 0,
            text: "a".into(),
            unshifted_codepoint: u32::from('a'),
            composed: false,
        },
        &mut out,
    );
    assert_eq!(out, b"\x1b[97;1:3u"); // Release has no associated text `:97`!

    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyA".into(),
            action: KeyAction::Press,
            modifiers: 1, // Shift
            text: "A".into(),
            unshifted_codepoint: u32::from('a'),
            composed: false,
        },
        &mut out,
    );
    assert_eq!(out, b"\x1b[97:65;2;65u");

    // Composed Alt+Shift+e -> `é` consumes Shift+Alt (unlike non-composed)
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyE".into(),
            action: KeyAction::Press,
            modifiers: 1 | 4, // Shift | Alt
            text: "é".into(),
            unshifted_codepoint: u32::from('e'),
            composed: true,
        },
        &mut out,
    );
    let composed_seq = out.clone();
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyE".into(),
            action: KeyAction::Press,
            modifiers: 1 | 4,
            text: "é".into(),
            unshifted_codepoint: u32::from('e'),
            composed: false,
        },
        &mut out,
    );
    assert_ne!(composed_seq, out);

    // Ctrl+Shift+A does NOT consume Shift into text
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyA".into(),
            action: KeyAction::Press,
            modifiers: 1 | 2, // Shift | Ctrl
            text: "A".into(),
            unshifted_codepoint: u32::from('a'),
            composed: false,
        },
        &mut out,
    );
    assert!(out.starts_with(b"\x1b[97"));

    // Control / macOS Private Use fn char filtered by `typed()` -> no `;63232` text suffix
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "F1".into(),
            action: KeyAction::Press,
            modifiers: 0,
            text: "\u{f704}".into(),
            unshifted_codepoint: 0,
            composed: false,
        },
        &mut out,
    );
    assert_eq!(out, b"\x1b[1;1:1P");

    // Focus off by default, on after CSI ?1004h
    out.clear();
    vt.encode_focus(true, &mut out);
    assert!(out.is_empty());
    vt.feed(b"\x1b[?1004h", &mut ev);
    vt.encode_focus(true, &mut out);
    vt.encode_focus(false, &mut out);
    assert_eq!(out, b"\x1b[I\x1b[O");

    // Direct `consumed()` unit tests covering every branch
    let mk_key = |text: &str, mods: u8, unshifted: u32, composed: bool| KeyInput {
        code: "KeyA".into(),
        action: KeyAction::Press,
        modifiers: mods,
        text: text.into(),
        unshifted_codepoint: unshifted,
        composed,
    };
    assert_eq!(consumed(&mk_key("", 1, u32::from('a'), false)), 0);
    assert_eq!(consumed(&mk_key("ab", 1, u32::from('a'), false)), 0);
    assert_eq!(consumed(&mk_key("A", 1, 0, false)), 0);
    assert_eq!(consumed(&mk_key("a", 1, u32::from('a'), false)), 0);
    assert_eq!(consumed(&mk_key("\x01", 1, u32::from('a'), false)), 0);
    assert_eq!(consumed(&mk_key("A", 1 | 2, u32::from('a'), false)), 0); // Ctrl clears consumed
    assert_eq!(consumed(&mk_key("A", 1 | 4, u32::from('a'), false)), 1); // Shift only when not composed
    assert_eq!(consumed(&mk_key("é", 1 | 4, u32::from('e'), true)), 1 | 4); // Shift + Alt when composed
    assert_eq!(consumed(&mk_key("é", 4, u32::from('e'), true)), 4); // Alt alone when composed

    // Pointer: SGR button-event mode (?1002;1006h) verifies buttons 0/1/2, held_buttons bitmask, motion, wheel!
    assert!(!vt.meta().modes.mouse);
    vt.feed(b"\x1b[?1002;1006h", &mut ev);
    assert!(vt.meta().modes.mouse);
    let cell_px = CellPx { width: 8, height: 16 };
    let mk_ptr = |action, button, x_px, y_px| PointerInput { action, button, modifiers: 0, x_px, y_px, cell_px };

    // Drag past right edge (40 cols * 8px = 320px; x=400 is outside): reported ONLY while `held_buttons != 0`!
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut out);
    assert!(out.is_empty());

    // Press right (2) -> drag outside clamps to col 40 -> release right (2) -> outside drag silent
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Press, 2, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<2;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 2, 400, 16), &mut out);
    assert_eq!(out, b"\x1b[<34;40;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Release, 2, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<2;2;2m");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 2, 400, 16), &mut out);
    assert!(out.is_empty());

    // Press left (0) + press middle (1), release middle (1) -> left (0) STILL holds outside drag -> release left (0) -> silent!
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Press, 0, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<0;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Press, 1, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<1;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Release, 1, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<1;2;2m");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut out);
    assert_eq!(out, b"\x1b[<32;40;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Release, 0, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<0;2;2m");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut out);
    assert!(out.is_empty());

    // Press unnamed button (3, 5) outside `< 3` must NOT set held_buttons
    for unnamed in [3u8, 5] {
        out.clear();
        vt.encode_pointer(&mk_ptr(PointerAction::Press, unnamed, 8, 16), &mut out);
        vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut out);
        assert!(out.is_empty());
    }

    // Wheel up/down
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::WheelUp, 0, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<64;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::WheelDown, 0, 8, 16), &mut out);
    assert_eq!(out, b"\x1b[<65;2;2M");

    // Paste: unsafe `\n` and `\r` blocked without bracketed paste; safe passes; large >128B bracketed paste exercises `sized()` retry
    assert_eq!(vt.encode_paste("safe", false), PasteEncode::Bytes(b"safe".to_vec()));
    assert_eq!(vt.encode_paste("rm -rf /\n", false), PasteEncode::Unsafe);
    assert_eq!(vt.encode_paste("rm -rf /\r", false), PasteEncode::Unsafe);
    vt.feed(b"\x1b[?2004h", &mut ev);
    assert!(vt.meta().modes.bracketed_paste);
    let big = "x".repeat(200);
    let encoded = vt.encode_paste(&big, false);
    assert_eq!(encoded, PasteEncode::Bytes(format!("\x1b[200~{big}\x1b[201~").into_bytes()));
    assert_eq!(vt.encode_paste("bad\x1b[201~cmd", false), PasteEncode::Unsafe);
}

#[test]
fn image_store_lru_eviction_png_variants_and_kitty_direct_rgb_virtual_placeholders() {
    // 1. PNG codec: RGB (3ch), RGBA (4ch), Grayscale (1ch -> GrayscaleAlpha -> RGBA), GrayscaleAlpha (2ch -> RGBA),
    // invalid dims/channels, and oversized header rejection (>64 MiB).
    assert!(encode_png_pixels(0, 2, 4, &[]).is_none());
    assert!(encode_png_pixels(2, 0, 4, &[]).is_none());
    assert!(encode_png_pixels(2, 2, 5, &[0; 20]).is_none());
    let rgb_png = encode_png_pixels(2, 1, 3, &[10, 20, 30, 40, 50, 60]).unwrap();
    let (w, h, rgba) = decode_png_rgba(&rgb_png).unwrap();
    assert_eq!((w, h), (2, 1));
    assert_eq!(rgba, vec![10, 20, 30, 255, 40, 50, 60, 255]);

    // GrayscaleAlpha PNG exercises in-place pixel expansion loop in `decode_png_into`
    let ga_png = encode_test_png(
        2,
        2,
        png::ColorType::GrayscaleAlpha,
        png::BitDepth::Eight,
        &[10, 100, 20, 150, 30, 200, 40, 250],
    );
    let (gw, gh, grgba) = decode_png_rgba(&ga_png).unwrap();
    assert_eq!((gw, gh), (2, 2));
    assert_eq!(grgba, vec![10, 10, 10, 100, 20, 20, 20, 150, 30, 30, 30, 200, 40, 40, 40, 250]);

    // Oversized PNG dimensions (5000x5000 * 4 = 100 MiB > 64 MiB) rejected before allocation
    let mut bomb = rgb_png.clone();
    // IHDR starts at byte 8 (4 len, 4 "IHDR", 4 width, 4 height) -> patch width & height to 5000 + recompute?
    // Even simpler: use `png::Encoder` with 5000x5000 header only (truncated before IDAT is fine because read_info parses IHDR!)
    let mut big_hdr = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut big_hdr, 5000, 5000);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let _ = enc.write_header().unwrap();
    }
    assert!(decode_png_rgba(&big_hdr).is_none());
    bomb[0] = 0;
    assert!(decode_png_rgba(&bomb).is_none());

    // Exact pixel-budget boundary checks (0, exact cap, cap + 4, overflow)
    assert_eq!(png_rgba_pixel_count(0, 10, 64), None);
    assert_eq!(png_rgba_pixel_count(4, 4, 64), Some(16));
    assert_eq!(png_rgba_pixel_count(4, 4, 63), None);
    assert_eq!(png_rgba_pixel_count(u32::MAX, u32::MAX, u64::MAX), None);

    // 2. ImageStore: empty/oversized rejection, exact cap, used_bytes, len, is_empty, deterministic LRU via both insert_encoded & get_chunk!
    let mut store = ImageStore::new(15);
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert_eq!(store.used_bytes(), 0);
    assert!(store.insert_encoded(ImageEncoding::Png, Vec::new()).is_none());
    assert!(store.insert_encoded(ImageEncoding::Png, vec![1; 16]).is_none());

    let h1 = store.insert_encoded(ImageEncoding::Png, vec![1; 5]).unwrap();
    let h2 = store.insert_encoded(ImageEncoding::Png, vec![2; 5]).unwrap();
    let h3 = store.insert_encoded(ImageEncoding::Png, vec![3; 5]).unwrap();
    assert!(!store.is_empty());
    assert_eq!(store.len(), 3);
    assert_eq!(store.used_bytes(), 15);

    // Promote h1 via `get_chunk` and h2 via dedup `insert_encoded` -> h3 MUST be evicted when h4 arrives!
    let c1 = store.get_chunk(h1, 0, 3).unwrap();
    assert_eq!(c1.data, vec![1, 1, 1]);
    assert_eq!(store.insert_encoded(ImageEncoding::Png, vec![2; 5]), Some(h2));
    let h4 = store.insert_encoded(ImageEncoding::Png, vec![4; 5]).unwrap();
    assert_eq!(store.len(), 3);
    assert_eq!(store.used_bytes(), 15);
    assert!(store.get_chunk(h1, 0, 5).is_some());
    assert!(store.get_chunk(h2, 0, 5).is_some());
    assert!(store.get_chunk(h3, 0, 5).is_none(), "un-touched h3 must be LRU victim");
    assert!(store.get_chunk(h4, 0, 5).is_some());

    // Now h1 was touched by get_chunk, h2 touched, h4 touched; touch h2 & h4 via get_chunk -> h1 evicted next!
    let _ = store.get_chunk(h2, 0, 5);
    let _ = store.get_chunk(h4, 0, 5);
    let h5 = store.insert_encoded(ImageEncoding::Png, vec![5; 5]).unwrap();
    assert!(store.get_chunk(h1, 0, 5).is_none(), "oldest h1 must be evicted");
    assert!(store.get_chunk(h2, 0, 5).is_some());
    assert!(store.get_chunk(h4, 0, 5).is_some());
    assert!(store.get_chunk(h5, 0, 5).is_some());

    // Pure insert-only LRU promotion via dedup `insert_encoded` (no get_chunk) kills `clock += 1` -> `clock *= 1`!
    let mut s_ab = ImageStore::new(10);
    let ka = s_ab.insert_encoded(ImageEncoding::Png, vec![10; 5]).unwrap();
    let kb = s_ab.insert_encoded(ImageEncoding::Png, vec![20; 5]).unwrap();
    // Re-insert ka so ka becomes MRU and kb becomes LRU (on clock==0 tie, ka would be evicted instead!)
    assert_eq!(s_ab.insert_encoded(ImageEncoding::Png, vec![10; 5]), Some(ka));
    let _kc = s_ab.insert_encoded(ImageEncoding::Png, vec![30; 5]).unwrap();
    assert!(s_ab.get_chunk(ka, 0, 5).is_some(), "re-inserted ka must survive");
    assert!(s_ab.get_chunk(kb, 0, 5).is_none(), "older kb must be evicted");
    // And in reverse insertion order so no HashMap slot order can accidentally satisfy both!
    let mut s_ba = ImageStore::new(10);
    let kb2 = s_ba.insert_encoded(ImageEncoding::Png, vec![20; 5]).unwrap();
    let ka2 = s_ba.insert_encoded(ImageEncoding::Png, vec![10; 5]).unwrap();
    assert_eq!(s_ba.insert_encoded(ImageEncoding::Png, vec![20; 5]), Some(kb2));
    let _kc2 = s_ba.insert_encoded(ImageEncoding::Png, vec![30; 5]).unwrap();
    assert!(s_ba.get_chunk(kb2, 0, 5).is_some(), "re-inserted kb2 must survive");
    assert!(s_ba.get_chunk(ka2, 0, 5).is_none(), "older ka2 must be evicted");

    // Exact-cap single blob replaces all existing entries
    let h6 = store.insert_encoded(ImageEncoding::Png, vec![6; 15]).unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store.used_bytes(), 15);
    assert!(store.get_chunk(h6, 0, 15).is_some());

    // 3. GhosttyEngine: image_store(), hostile t=f rejected, raw RGB (f=24) + PNG (f=100) + image update gen bump,
    // AND virtual placement (U=1) + Unicode placeholder cells emitting PackedRow.pieces!
    let shared = Arc::new(Mutex::new(ImageStore::default()));
    let mut cfg = GhosttyConfig::new(grid(20, 6));
    cfg.cell_px = CellPx { width: 8, height: 16 };
    cfg.image_store = Some(Arc::clone(&shared));
    let mut vt = GhosttyEngine::new(cfg).unwrap();
    assert!(Arc::ptr_eq(vt.image_store(), &shared));
    let mut ev = Vec::new();

    // Hostile file medium rejected
    let hostile = format!("\x1b_Ga=T,t=f,f=100,i=99;{}\x1b\\", base64_encode(b"/etc/passwd"));
    vt.feed(hostile.as_bytes(), &mut ev);
    assert!(vt.images().is_empty());

    // Direct raw RGB (f=24) image id=7
    let raw_rgb = [255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
    let direct_rgb = format!("\x1b_Ga=T,f=24,s=2,v=2,i=7;{}\x1b\\", base64_encode(&raw_rgb));
    vt.feed(direct_rgb.as_bytes(), &mut ev);
    let imgs1 = vt.images();
    assert_eq!(imgs1.len(), 1);
    let blob1 = imgs1[0].blob;

    // Re-transmit image id=7 with DIFFERENT pixels -> generation bumps and new blob replaces old!
    let raw_rgb2 = [0u8, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110];
    let direct_rgb2 = format!("\x1b_Ga=T,f=24,s=2,v=2,i=7;{}\x1b\\", base64_encode(&raw_rgb2));
    vt.feed(direct_rgb2.as_bytes(), &mut ev);
    let imgs2 = vt.images();
    assert_eq!(imgs2.len(), 1);
    assert_ne!(imgs2[0].blob, blob1);

    // Virtual placement (`U=1`, id=11, placement=3, c=2, r=2) + TWO separate placeholder rows:
    // Row A: plain CODEPOINT placeholder alone (no diacritics -> row=0, col=0)
    // Row B: CODEPOINT_GRAPHEME placeholder alone (diacritics row=1, col=1 -> `\u{030D}\u{030D}`)
    let png_virt = encode_png_rgba(16, 32, &[99u8; 16 * 32 * 4]).unwrap();
    let virt_seq = format!(
        "\r\n\x1b_Ga=T,U=1,f=100,i=11,p=3,c=2,r=2;{}\x1b\\\x1b[38;2;0;0;11;58;2;0;0;3m{}\x1b[0m\r\n\x1b[38;2;0;0;11;58;2;0;0;3m{}\u{030D}\u{030D}\x1b[0m",
        base64_encode(&png_virt),
        PLACEHOLDER,
        PLACEHOLDER,
    );
    vt.feed(virt_seq.as_bytes(), &mut ev);
    let row_b = vt.meta().cursor.row;
    let row_a = row_b - 1;
    let mut rows = Vec::new();
    vt.extract_viewport_rows(&[row_a, row_b], &mut rows);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].flags & PackedRow::FLAG_HAS_PLACEHOLDER != 0);
    assert_eq!(rows[0].pieces.len(), 1);
    assert_eq!(rows[0].pieces[0].src_rect, [0, 0, 8, 16]);
    assert!(rows[1].flags & PackedRow::FLAG_HAS_PLACEHOLDER != 0);
    assert_eq!(rows[1].pieces.len(), 1);
    assert_eq!(rows[1].pieces[0].src_rect, [8, 16, 8, 16]);
}

#[test]
fn placeholder_run_and_fit_geometry_comprehensive() {
    // Said::of + PlaceholderRun::takes + high-byte image id + placement id
    let st_rgb = PackedStyle { fg: 0x01_00_00_05, bg: 0, underline_color: 10, flags: 0 };
    let st_pal = PackedStyle { fg: 6, bg: 0, underline_color: 0, flags: 0 };
    let s0 = Said::of(&st_rgb, ['\u{0305}', '\u{030D}', '\u{030E}']); // row=0, col=1, high=2
    let mut run = PlaceholderRun::begin(3, s0);
    assert_eq!(run.image(), 5 + (2 << 24));
    assert_eq!(run.placement(), 9); // palette 10 -> 10 - 1 = 9!
    assert_eq!(run.image_row, 0);
    assert_eq!(run.image_column, 1);

    // Rejects: non-adjacent col, different image_low, different placement, wrong row, wrong col, wrong high
    assert!(!run.takes(5, Said::of(&st_rgb, []))); // col != 4
    assert!(!run.takes(4, Said::of(&PackedStyle { fg: 0x01_00_00_06, ..st_rgb }, []))); // image_low
    assert!(!run.takes(4, Said::of(&PackedStyle { underline_color: 11, ..st_rgb }, []))); // placement
    assert!(!run.takes(4, Said::of(&st_rgb, ['\u{030D}']))); // row=1 != 0
    assert!(!run.takes(4, Said::of(&st_rgb, ['\u{0305}', '\u{0305}']))); // col=0 != 2
    assert!(!run.takes(4, Said::of(&st_rgb, ['\u{0305}', '\u{030E}', '\u{0305}']))); // high=0 != 2
    // Accepts exact continuation (row=0, col=2, high=2)
    assert!(run.takes(4, Said::of(&st_rgb, ['\u{0305}', '\u{030E}', '\u{030E}'])));
    assert_eq!(run.width, 2);

    // Palette colour + no placement + unknown diacritic
    let run_pal = PlaceholderRun::begin(0, Said::of(&st_pal, ['X']));
    assert_eq!(run_pal.image(), 5); // palette 6 -> 5
    assert_eq!(run_pal.placement(), 0);

    // fit(): zero-dimension guards
    let mk_run = |row: u32, col: u32, width: u16| {
        let mut r = PlaceholderRun::begin(0, Said::of(&st_pal, []));
        r.image_row = row;
        r.image_column = col;
        r.width = width;
        r
    };
    assert!(fit(&mk_run(0, 0, 1), (2, 2), (10, 10), (0, 10)).is_none());
    assert!(fit(&mk_run(0, 0, 1), (2, 2), (10, 10), (10, 0)).is_none());
    assert!(fit(&mk_run(0, 0, 1), (2, 2), (0, 10), (10, 10)).is_none());
    assert!(fit(&mk_run(0, 0, 1), (2, 2), (10, 0), (10, 10)).is_none());

    // fit(): auto-grid (0, 0) with non-multiple dimensions (15x25 in 10x10 cells -> 2x3 grid)
    let f_auto = fit(&mk_run(0, 0, 2), (0, 0), (15, 25), (10, 10)).unwrap();
    assert_eq!(f_auto.dst_px, [18, 10]); // scale=1.2 -> grid 20x30, pic 15x25 -> 18x30; row 0 is 18x10

    // fit(): WIDER picture (100x20 in 5x5 grid of 10x10 = 50x50px -> scale=0.5, rendered 50x10, margin_y=20px = 2 rows top & bottom)
    // Pure top margin row (row 0) -> None!
    assert!(fit(&mk_run(0, 0, 5), (5, 5), (100, 20), (10, 10)).is_none());
    // Pure bottom margin row (row 4) -> None!
    assert!(fit(&mk_run(4, 0, 5), (5, 5), (100, 20), (10, 10)).is_none());
    // Middle row (row 2) -> exact full 100x20 image in 50x10px
    let f_mid_w = fit(&mk_run(2, 0, 5), (5, 5), (100, 20), (10, 10)).unwrap();
    assert_eq!(f_mid_w.src_rect, [0, 0, 100, 20]);
    assert_eq!(f_mid_w.dst_px, [50, 10]);

    // WIDER picture with partial top/bottom margins (100x30 in 5x5 grid of 10x10 -> scale=0.5, rendered 50x15, margin_y=17.5px):
    // row 1 (y=10..20) straddles top margin (17.5..20 -> 2.5px -> 3px)
    let f_top = fit(&mk_run(1, 0, 5), (5, 5), (100, 30), (10, 10)).unwrap();
    assert_eq!(f_top.src_rect, [0, 0, 100, 5]);
    assert_eq!(f_top.dst_px, [50, 3]);
    // row 2 (y=20..30) strictly interior (20..30 -> 10px)
    let f_int_y = fit(&mk_run(2, 0, 5), (5, 5), (100, 30), (10, 10)).unwrap();
    assert_eq!(f_int_y.src_rect, [0, 5, 100, 20]);
    assert_eq!(f_int_y.dst_px, [50, 10]);
    // row 3 (y=30..40) straddles bottom margin (30..32.5 -> 2.5px -> 3px)
    let f_bot = fit(&mk_run(3, 0, 5), (5, 5), (100, 30), (10, 10)).unwrap();
    assert_eq!(f_bot.src_rect, [0, 25, 100, 5]);
    assert_eq!(f_bot.dst_px, [50, 3]);

    // Single-row grid taller than picture -> top branch clamps `source_height > picture_height`!
    let f_clamp_y = fit(&mk_run(0, 0, 4), (4, 1), (100, 10), (10, 20)).unwrap();
    assert_eq!(f_clamp_y.src_rect, [0, 0, 100, 10]);
    assert_eq!(f_clamp_y.dst_px, [40, 4]);

    // TALLER picture with partial left/right margins (30x100 in 5x5 grid of 10x10 -> scale=0.5, rendered 15x50, margin_x=17.5px):
    assert!(fit(&mk_run(0, 0, 1), (5, 5), (30, 100), (10, 10)).is_none()); // pure left margin
    let f_left = fit(&mk_run(0, 1, 1), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_left.src_rect, [0, 0, 5, 20]);
    assert_eq!(f_left.dst_px, [3, 10]);
    let f_int_x = fit(&mk_run(0, 2, 1), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_int_x.src_rect, [5, 0, 20, 20]);
    assert_eq!(f_int_x.dst_px, [10, 10]);
    let f_right = fit(&mk_run(0, 3, 1), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_right.src_rect, [25, 0, 5, 20]);
    assert_eq!(f_right.dst_px, [3, 10]);
    // Wide run spanning whole taller picture -> left branch clamps `source_width > picture_width`!
    let f_clamp_x = fit(&mk_run(0, 0, 5), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_clamp_x.src_rect, [0, 0, 30, 20]);
    assert_eq!(f_clamp_x.dst_px, [15, 10]);
}

#[test]
fn snapshot_export_cut_truncation_and_restore_preserves_scrollback_and_bumps_epoch() {
    let mut src = GhosttyEngine::new(GhosttyConfig::new(grid(80, 4))).unwrap();
    let mut ev = Vec::new();
    // Feed 1_500 lines so snapshot spans multiple scrollback pages decoded via `ghostty_snapshot_decoder_next`!
    for i in 0..1_500 {
        src.feed(format!("snap-line-{i:04} padding-to-fill-multiple-64k-pages-cleanly\r\n").as_bytes(), &mut ev);
    }
    let snap = src.export_snapshot(1024 * 1024).unwrap();
    assert!(!snap.is_empty());
    assert_eq!(last_snapshot_record_end(&snap), snap.len());
    // Synthetic header (10 bytes) + record header claiming len=50 with only 5 payload bytes -> must return 10!
    let mut synth = vec![0u8; 25];
    synth[12..16].copy_from_slice(&50u32.to_le_bytes());
    assert_eq!(last_snapshot_record_end(&synth), 10);
    assert_eq!(last_snapshot_record_end(&snap[..5]), 5);

    // Tight max_bytes triggers `cap.cut == true` and still returns a valid truncated snapshot!
    let tight = src.export_snapshot(snap.len() / 2).unwrap();
    assert!(tight.len() <= snap.len() / 2);
    assert_eq!(last_snapshot_record_end(&tight), tight.len());

    // Restore twice so `epoch += 1` moves 1 -> 2 -> 3 and verify ALL multi-page scrollback records decoded!
    let mut dst = GhosttyEngine::new(GhosttyConfig::new(grid(10, 2))).unwrap();
    dst.restore_snapshot(&snap).unwrap();
    assert_eq!(dst.meta().epoch, 2);
    assert_eq!(dst.meta().size, grid(80, 4));
    assert!(
        dst.meta().scrollback_lines >= 1_490,
        "restore must decode all scrollback pages (got {})",
        dst.meta().scrollback_lines
    );
    let mut hist = Vec::new();
    dst.extract_history_rows(0, 2, &mut hist);
    assert!(hist[0].text.starts_with("snap-line-0000"));
    assert!(hist[1].text.starts_with("snap-line-0001"));

    dst.restore_snapshot(&snap).unwrap();
    assert_eq!(dst.meta().epoch, 3);
}
