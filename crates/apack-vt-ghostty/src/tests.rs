use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use apack_core::marks::MarksMode;
use apack_core::session::SessionMachine;
use apack_core::time::TimestampMs;
use apack_core::vt::{CompressStep, PasteEncode, ProgramStatusUpdate, VtEngine, VtEvent};
use apack_proto::messages::{BrokerMsg, FrameUpdate, decode_broker_frame};
use apack_proto::types::{
    BlockedKind, CellPos, CellPx, CellSpan, CursorShape, GridSize, ImageEncoding, KeyAction, KeyInput, MarksKey,
    NonZeroStreamId, PackedRow, PackedStyle, PointerAction, PointerInput, ProgressState, RequestId, ResizePolicy,
    SessionId, StatusState, StreamId, SubFlags,
};

use crate::engine::{
    GhosttyConfig, GhosttyEngine, coalesce_feed_events, decode_osc7_cwd, escape_osc633_field, features,
    last_snapshot_record_end, sanitize_osc633_body,
};
use crate::error::GhosttyError;
use crate::image_store::{ImageStore, decode_png_rgba, encode_png_pixels, encode_png_rgba, png_rgba_pixel_count};
use crate::input::consumed;
use crate::placeholders::{PLACEHOLDER, PlaceholderRun, Said, fit};
use crate::rows::{ROW_LINK_BYTES, intern_link};
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
    assert_eq!(ROW_LINK_BYTES, 64 * 1024);

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
    assert_eq!(intern_link("", &mut link_vec, &mut link_map, &mut link_bytes, 10), 0);
    assert_eq!(intern_link("abcd", &mut link_vec, &mut link_map, &mut link_bytes, 7), 1);
    assert_eq!(intern_link("abcd", &mut link_vec, &mut link_map, &mut link_bytes, 7), 1);
    assert_eq!(intern_link("efg", &mut link_vec, &mut link_map, &mut link_bytes, 7), 2);
    assert_eq!(link_bytes, 7);
    assert_eq!(intern_link("h", &mut link_vec, &mut link_map, &mut link_bytes, 7), 0);
    let mut cap_vec = Vec::new();
    let mut cap_map = HashMap::new();
    let mut cap_bytes = 0usize;
    for i in 0..1023 {
        assert_eq!(intern_link(&format!("u{i}"), &mut cap_vec, &mut cap_map, &mut cap_bytes, 1 << 20), (i + 1) as u16);
    }
    assert_eq!(intern_link("overflow", &mut cap_vec, &mut cap_map, &mut cap_bytes, 1 << 20), 0);

    // Row 2 checks: BG_COLOR_PALETTE (42 -> 43) and BG_COLOR_RGB (12,34,56 -> 0x01_0C_22_38)
    let r2 = &rows[2];
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
    let unwrapped = vt.text_of(CellSpan { start: CellPos { line: 3, col: 0 }, end: CellPos { line: 4, col: 4 } }, 4096);
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
fn scroll_tracking_side_table_hashing_alt_screen_zero_scrollback_and_resize_semantics() {
    let mut vt = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    let mut ev = Vec::new();

    // While `full_dirty` is already true on a fresh engine, scrolling must NOT leak non-zero `scroll_delta`!
    vt.feed(b"\x1b[4;1H\r\n", &mut ev);
    let d0 = vt.take_dirty_viewport();
    assert!(d0.full_dirty);
    assert_eq!(d0.scroll_delta, 0);
    assert!(d0.dirty_rows.is_empty());
    let base_line = vt.meta().first_active_line;
    let grid_gen0 = vt.generations().grid;

    // Partial update: touch row 0 and row 2 without scrolling
    vt.feed(b"\x1b[1;1Hrow0\x1b[3;1Hrow2", &mut ev);
    let d1 = vt.take_dirty_viewport();
    assert!(!d1.full_dirty);
    assert_eq!(d1.scroll_delta, 0);
    assert!(d1.dirty_rows.contains(&0));
    assert!(d1.dirty_rows.contains(&2));
    assert!(vt.generations().grid > grid_gen0);

    // Dirty ONLY row 2, then scroll by 1 BEFORE taking dirty -> row 2 shifts to row 1, row 3 is new!
    vt.feed(b"\x1b[3;1HB", &mut ev);
    let g_scroll0 = vt.generations().grid;
    vt.feed(b"\x1b[4;1H\r\n", &mut ev);
    let d2 = vt.take_dirty_viewport();
    assert!(!d2.full_dirty);
    assert_eq!(d2.scroll_delta, 1);
    assert_eq!(vt.meta().first_active_line, base_line + 1);
    assert_eq!(vt.generations().grid, g_scroll0 + 1);
    assert!(d2.dirty_rows.contains(&1));
    assert!(d2.dirty_rows.contains(&3));
    assert!(!d2.dirty_rows.contains(&0));
    assert!(!d2.dirty_rows.contains(&2));

    // Side-table hash checks during scroll (both short <=256B AND long >256B OSC 8 URIs):
    // Row 0 has styled text, Row 1 has combining grapheme `e\u{0301}` + short link, Row 2 has long OSC 8 hyperlink!
    let old_long = format!("https://old.example/{}", "a".repeat(270));
    let new_long = format!("https://new.example/{}", "b".repeat(270));
    vt.feed(
        format!(
            "\x1b[1;1H\x1b[2J\x1b[2;1H\x1b[31mS\x1b[0m\x1b[3;1He\u{0301}\x1b[4;1H\x1b]8;;{old_long}\x1b\\L\x1b]8;;\x1b\\"
        )
        .as_bytes(),
        &mut ev,
    );
    let _ = vt.take_dirty_viewport();
    vt.feed(
        format!(
            "\x1b[4;1H\r\n\x1b[1;1H\x1b[32mS\x1b[0m\x1b[2;1He\u{0300}\x1b[3;1H\x1b]8;;{new_long}\x1b\\L\x1b]8;;\x1b\\"
        )
        .as_bytes(),
        &mut ev,
    );
    let d_side = vt.take_dirty_viewport();
    assert!(!d_side.full_dirty);
    assert_eq!(d_side.scroll_delta, 1);
    assert!(d_side.dirty_rows.contains(&0), "style color change during scroll must be detected");
    assert!(d_side.dirty_rows.contains(&1), "grapheme combining mark change during scroll must be detected");
    assert!(d_side.dirty_rows.contains(&2), "long hyperlink URI change during scroll must be detected");
    assert!(d_side.dirty_rows.contains(&3));

    // Short (<=256B) hyperlink URI change during scroll exercises `Result::SUCCESS` arm of `hash_viewport_row`!
    vt.feed(b"\x1b[1;1H\x1b[2J\x1b[2;1H\x1b]8;;https://a.test\x1b\\K\x1b]8;;\x1b\\", &mut ev);
    let _ = vt.take_dirty_viewport();
    vt.feed(b"\x1b[4;1H\r\n\x1b[1;1H\x1b]8;;https://b.test\x1b\\K\x1b]8;;\x1b\\", &mut ev);
    let d_short_link = vt.take_dirty_viewport();
    assert!(!d_short_link.full_dirty);
    assert!(d_short_link.dirty_rows.contains(&0), "short hyperlink URI change during scroll must be detected");

    // Unchanged row stays clean; prompt flag change on row 1 detected during scroll
    vt.feed(b"\x1b[1;1H\x1b[2J\x1b[2;1HrowA\x1b[3;1HrowB", &mut ev);
    let _ = vt.take_dirty_viewport();
    vt.feed(b"\x1b[4;1H\r\n\x1b[2;1H\x1b]133;A\x07", &mut ev);
    let d_prompt = vt.take_dirty_viewport();
    assert!(!d_prompt.full_dirty);
    assert_eq!(d_prompt.scroll_delta, 1);
    assert!(!d_prompt.dirty_rows.contains(&0), "unchanged row 0 must stay clean");
    assert!(d_prompt.dirty_rows.contains(&1), "prompt flag change must mark row 1 dirty");
    assert!(d_prompt.dirty_rows.contains(&3));

    // Scroll by 1 twice on blank bottom rows -> hashes match, so ONLY `r >= keep` marks bottom row 3 dirty!
    vt.feed(b"\x1b[1;1H\x1b[2J\x1b[4;1H\r\n", &mut ev);
    let _ = vt.take_dirty_viewport();
    vt.feed(b"\x1b[4;1H\r\n", &mut ev);
    let d3c = vt.take_dirty_viewport();
    assert!(!d3c.full_dirty);
    assert_eq!(d3c.scroll_delta, 1);
    assert_eq!(d3c.dirty_rows, vec![3]);

    // Cumulative multi-feed scroll (2 + 2 >= 4 rows) upgrades to `full_dirty = true, scroll_delta = 0`!
    vt.feed(b"\x1b[4;1H\r\n\r\n", &mut ev);
    vt.feed(b"\x1b[4;1H\r\n\r\n", &mut ev);
    let d4 = vt.take_dirty_viewport();
    assert!(d4.full_dirty);
    assert_eq!(d4.scroll_delta, 0);
    assert!(d4.dirty_rows.is_empty());

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

    // Alt-screen enter, THEN scroll in SEPARATE feeds -> `first_active_line` MUST STAY 0!
    let modes_gen0 = vt.generations().modes;
    ev.clear();
    vt.feed(b"\x1b[?1049hALT", &mut ev);
    assert!(ev.contains(&VtEvent::EpochChanged));
    assert_eq!(vt.meta().epoch, 2);
    assert!(vt.meta().modes.alt_screen);
    assert_eq!(vt.meta().first_active_line, 0);
    assert_eq!(vt.generations().modes, modes_gen0 + 1);
    let _ = vt.take_dirty_viewport();
    ev.clear();
    vt.feed(b"\x1b[1;1HL0\x1b[2;1HL1\x1b[3;1HL2\x1b[4;1HL3", &mut ev);
    let _ = vt.take_dirty_viewport();
    vt.feed(b"\x1b[4;1H\r\n", &mut ev);
    assert_eq!(vt.meta().first_active_line, 0, "alt-screen scrolling must never advance first_active_line");
    assert_eq!(vt.meta().scrollback_lines, 0);
    let d_alt = vt.take_dirty_viewport();
    assert!(d_alt.full_dirty || d_alt.dirty_rows.contains(&0));
    ev.clear();
    vt.feed(b"\x1b[?1049l", &mut ev);
    assert!(ev.contains(&VtEvent::EpochChanged));
    assert_eq!(vt.meta().epoch, 3);
    assert!(!vt.meta().modes.alt_screen);
    assert_eq!(vt.generations().modes, modes_gen0 + 2);

    // Primary screen flood exceeding scrollback capacity in ONE feed (`active_top` evicted):
    // must bump `epoch` and emit `VtEvent::EpochChanged` instead of guessing scroll count!
    let mut cfg0 = GhosttyConfig::new(grid(80, 4));
    cfg0.scrollback_lines = 100;
    let mut vt0 = GhosttyEngine::new(cfg0).unwrap();
    ev.clear();
    let flood: String = (0..2_000).map(|i| format!("flood-line-{i:04} padding-text\r\n")).collect();
    vt0.feed(flood.as_bytes(), &mut ev);
    assert!(ev.contains(&VtEvent::EpochChanged), "evicting active_top in one feed must emit EpochChanged");
    assert_eq!(vt0.meta().epoch, 2);
    assert_eq!(vt0.meta().first_active_line, vt0.meta().scrollback_lines);

    // Resize: identical (size, cell_px) is a no-op; cell_px-only does NOT bump epoch; grid change bumps epoch
    // AND drains synchronous mode 2048 size reports!
    let ep = vt.meta().epoch;
    ev.clear();
    vt.resize(grid(20, 4), CellPx::default(), &mut ev);
    assert_eq!(vt.meta().epoch, ep);
    vt.resize(grid(20, 4), CellPx { width: 9, height: 18 }, &mut ev);
    assert_eq!(vt.meta().epoch, ep);
    ev.clear();
    vt.feed(b"\x1b[16t\x1b[?2048h", &mut ev);
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[6;18;9t")));
    ev.clear();
    vt.resize(grid(24, 5), CellPx { width: 9, height: 18 }, &mut ev);
    assert_eq!(vt.meta().epoch, ep + 1);
    assert_eq!(vt.meta().size, grid(24, 5));
    assert!(
        ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b.starts_with(b"\x1b[48;"))),
        "mode 2048 size report must be drained immediately during resize (got {ev:?})"
    );
}

#[test]
fn scrollback_eviction_compression_word_delimiters_rect_wide_grapheme_and_search() {
    let mut cfg = GhosttyConfig::new(grid(80, 4));
    cfg.scrollback_lines = 100;
    let mut vt = GhosttyEngine::new(cfg).unwrap();
    let mut ev = Vec::new();

    for i in 0..2_000 {
        vt.feed(format!("line-{i:04} foo/bar\r\n").as_bytes(), &mut ev);
    }
    let meta = vt.meta();
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
    assert!(steps < 64);

    // Out-of-range epoch line queries return empty/None cleanly
    assert!(vt.line_at(oldest - 1).is_none());
    assert!(vt.line_at(meta.total_lines()).is_none());
    let mut hist = Vec::new();
    vt.extract_history_rows(oldest, 2, &mut hist);
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].line, oldest);
    assert_eq!(hist[1].line, oldest + 1);

    // word_at honours custom `delimiters` (" " selects "foo/bar"; " /" selects "foo")!
    let w_space = vt.word_at(CellPos { line: oldest, col: 11 }, " ").unwrap();
    assert_eq!(vt.text_of(w_space, 4096), "foo/bar");
    let w_slash = vt.word_at(CellPos { line: oldest, col: 11 }, " /").unwrap();
    assert_eq!(vt.text_of(w_slash, 4096), "foo");
    let line_span = vt.line_at(oldest).unwrap();
    assert!(vt.text_of(line_span, 4096).contains("foo/bar"));

    // text_of_rect: handles wide CJK (no spurious spacer!), combining graphemes, Kitty placeholder -> ' ', and col clamping
    let mut rvt = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    rvt.feed(format!("中文e\u{0301}{}Z\r\nline2\r\n", PLACEHOLDER).as_bytes(), &mut ev);
    let rect =
        rvt.text_of_rect(CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 1, col: 99 } }, 4096);
    assert_eq!(rect, "中文e\u{0301} Z\nline2");

    // Search edge cases: empty needle, empty range, zero limit
    let mut out = Vec::new();
    assert!(!vt.search_lines(oldest, meta.total_lines(), "", true, 10, &mut out));
    assert!(!vt.search_lines(oldest, oldest, "foo/bar", true, 10, &mut out));
    assert!(!vt.search_lines(oldest, meta.total_lines(), "foo/bar", true, 0, &mut out));
    assert!(out.is_empty());

    // Exact range boundary filtering (`[oldest+1, oldest+4)` -> exactly 3 matches)
    let trunc_none = vt.search_lines(oldest + 1, oldest + 4, "foo/bar", true, 3, &mut out);
    assert!(!trunc_none);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].start.line, oldest + 1);
    assert_eq!(out[2].start.line, oldest + 3);

    // Truncation when matches > limit
    out.clear();
    let trunc_yes = vt.search_lines(oldest + 1, oldest + 4, "foo/bar", true, 2, &mut out);
    assert!(trunc_yes);
    assert_eq!(out.len(), 2);

    // Case-sensitive miss vs case-insensitive hit, plus completely absent needle
    out.clear();
    assert!(!vt.search_lines(oldest + 1, oldest + 4, "ZZZZZZ", true, 10, &mut out));
    assert!(out.is_empty());
    assert!(!vt.search_lines(oldest + 1, oldest + 4, "FOO/BAR", true, 10, &mut out));
    assert!(out.is_empty());
    assert!(!vt.search_lines(oldest + 1, oldest + 4, "FOO/BAR", false, 10, &mut out));
    assert_eq!(out.len(), 3);

    // Multi-page search (>1 page of scrollback) + wrapped case-sensitive match
    let mut cfg_big = GhosttyConfig::new(grid(40, 4));
    cfg_big.scrollback_lines = 4_000;
    let mut big = GhosttyEngine::new(cfg_big).unwrap();
    let mut big_ev = Vec::new();
    big.feed(format!("{}WrappedCaseNeedle\r\n", "x".repeat(32)).as_bytes(), &mut big_ev);
    for i in 0..2_000 {
        big.feed(format!("mp-row-{i:04} filler-text-for-pages\r\n").as_bytes(), &mut big_ev);
    }
    let bm = big.meta();
    let mut wrapped_hits = Vec::new();
    assert!(!big.search_lines(bm.oldest_line(), bm.total_lines(), "WrappedCaseNeedle", true, 10, &mut wrapped_hits));
    assert_eq!(wrapped_hits.len(), 1);
    assert!(wrapped_hits[0].end.line > wrapped_hits[0].start.line);
    let mut mp_hits = Vec::new();
    assert!(!big.search_lines(bm.oldest_line(), bm.total_lines(), "mp-row-", true, 2_500, &mut mp_hits));
    assert_eq!(mp_hits.len(), 2_000);

    // >20_000 matches in buffer: empty range (`from == to`) MUST return `false` upfront rather than hitting `wanted`!
    let mut dense = GhosttyEngine::new(GhosttyConfig::new(grid(80, 4))).unwrap();
    let row80 = format!("{}\r\n", "a".repeat(80));
    for _ in 0..300 {
        dense.feed(row80.as_bytes(), &mut big_ev);
    }
    let mut dense_out = Vec::new();
    assert!(!dense.search_lines(5, 5, "a", true, 10, &mut dense_out));
    assert!(dense_out.is_empty());
}

#[test]
fn callbacks_injection_safe_status_clear_osc133_escaping_coalescing_and_session_machine() {
    let mut cfg = GhosttyConfig::new(grid(40, 6));
    cfg.cell_px = CellPx { width: 8, height: 16 };
    let mut vt = GhosttyEngine::new(cfg).unwrap();
    let mut ev = Vec::new();

    assert!(!vt.meta().modes.mouse);

    // Base64 message containing hostile `;id=evil;state=done;app=spoof` MUST stay inside `message`!
    let hostile_msg = base64_encode(b"Allow?;id=evil;state=done;app=spoof");
    let title_b64 = base64_encode(b"Run;x=1");
    let stream = format!(
        concat!(
            "\x1b]2;old-title\x07\x1b]2;my-win\x07",
            "\x07\x07\x07\x07",
            "\x1b]7;file://localhost/tmp/hello%20world\x07",
            "\x1b]52;c;aGVsbG8=\x07",
            "\x1b]777;notify;Alert;Done!\x07",
            "\x1b]9;4;1;25\x07\x1b]9;4;1;75\x07",
            "\x1b]7501;state=blocked:kind=permission:progress=50:app=claude:title={}:msg={}\x1b\\",
            "\x1b]7501;state=clear\x1b\\",
            "\x1b[?2026h\x1b[?2026l",
            "\x1b[6n\x1b[16t",
            "\x1b]10;#112233\x07\x1b]4;1;#aa5500\x07",
        ),
        title_b64, hostile_msg,
    );
    vt.feed(stream.as_bytes(), &mut ev);

    // Coalescing verified: 2 consecutive titles -> 1 ("my-win"), 4 BELs -> 1 Bell, 2 consecutive progress -> 1 (75)!
    assert_eq!(ev.iter().filter(|e| matches!(e, VtEvent::Bell)).count(), 1);
    assert_eq!(ev.iter().filter(|e| matches!(e, VtEvent::Title(_))).count(), 1);
    assert!(ev.iter().any(|e| matches!(e, VtEvent::Title(t) if t == "my-win")));
    assert_eq!(ev.iter().filter(|e| matches!(e, VtEvent::Progress { .. })).count(), 1);
    assert!(ev.iter().any(|e| matches!(e, VtEvent::Progress { state: ProgressState::Running, percent: Some(75) })));

    assert!(ev.iter().any(|e| matches!(e, VtEvent::Cwd(c) if c == "/tmp/hello world")));
    assert!(
        ev.iter().any(|e| matches!(e, VtEvent::Clipboard { selection, data } if selection == "c" && data == "hello"))
    );
    assert!(
        ev.iter().any(|e| matches!(e, VtEvent::Notification { title, body } if title == "Alert" && body == "Done!"))
    );
    // Structured ProgramStatus: zero semicolon injection & explicit Clear!
    assert!(ev.iter().any(|e| matches!(
        e,
        VtEvent::ProgramStatus(ProgramStatusUpdate::Set {
            id,
            state: StatusState::Blocked,
            kind: Some(BlockedKind::Permission),
            progress: Some(50),
            app: Some(app),
            title: Some(title),
            message: Some(msg),
        }) if id.is_empty() && app == "claude" && title == "Run;x=1" && msg == "Allow?;id=evil;state=done;app=spoof"
    )));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::ProgramStatus(ProgramStatusUpdate::Clear { id }) if id.is_empty())));
    assert!(ev.contains(&VtEvent::SyncOutput(true)));
    assert!(ev.contains(&VtEvent::SyncOutput(false)));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[1;1R")));
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[6;16;8t")));

    let pal = vt.palette_overrides();
    assert_eq!(pal.fg, Some(apack_proto::types::Rgb([0x11, 0x22, 0x33])));
    assert!(pal.palette.contains(&(1, apack_proto::types::Rgb([0xAA, 0x55, 0x00]))));
    assert!(vt.generations().palette > 0);

    // OSC 7 file:// percent-decodes; bare path (OSC 9 / OSC 1337) is returned VERBATIM!
    assert_eq!(decode_osc7_cwd("file://host/a%2fb%2Fc%7ed%7Ee%0Af%zz%2"), "/a/b/c~d~e\nf%zz%2");
    assert_eq!(decode_osc7_cwd("/bare/100%20literal%2f"), "/bare/100%20literal%2f");

    // OSC 133 command escaping & truncated OSC 633 sanitization helpers
    assert_eq!(escape_osc633_field("echo a; C:\\foo\n\r"), "echo a\\x3b C:\\\\foo\\x0a\\x0d");
    assert_eq!(sanitize_osc633_body("P;Cwd=/tmp", false).as_deref(), Some("P;Cwd=/tmp"));
    assert_eq!(sanitize_osc633_body("P;Cwd=/tmp", true), None);
    assert_eq!(
        sanitize_osc633_body("V;F;1;0123456789abcdef0123456789abcdef;0;/half/trunc", true).as_deref(),
        Some("V;F;1;0123456789abcdef0123456789abcdef;0;")
    );
    assert_eq!(
        sanitize_osc633_body("V;S;1;0123456789abcdef0123456789abcdef;cmd", true).as_deref(),
        Some("V;S;1;0123456789abcdef0123456789abcdef;cmd")
    );
    let mut coal = Vec::new();
    coalesce_feed_events(
        vec![
            VtEvent::Bell,
            VtEvent::Title("a".into()),
            VtEvent::Title("b".into()),
            VtEvent::Bell,
            VtEvent::Progress { state: ProgressState::Running, percent: Some(1) },
            VtEvent::Progress { state: ProgressState::Running, percent: Some(2) },
        ],
        &mut coal,
    );
    assert_eq!(
        coal,
        vec![
            VtEvent::Bell,
            VtEvent::Title("b".into()),
            VtEvent::Progress { state: ProgressState::Running, percent: Some(2) }
        ]
    );

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

    // Standard legacy keys: ArrowUp, Ctrl+C, AND cross-platform composed vs non-composed Alt+e!
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

    // In legacy mode, `composed: true` with Alt+e consumes Alt -> emits `"é"` directly,
    // whereas `composed: false` does NOT consume Alt -> emits ESC-prefixed chord on ALL platforms!
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyE".into(),
            action: KeyAction::Press,
            modifiers: 4, // Alt
            text: "é".into(),
            unshifted_codepoint: u32::from('e'),
            composed: true,
        },
        &mut out,
    );
    let legacy_composed = out.clone();
    out.clear();
    vt.encode_key(
        &KeyInput {
            code: "KeyE".into(),
            action: KeyAction::Press,
            modifiers: 4, // Alt
            text: "é".into(),
            unshifted_codepoint: u32::from('e'),
            composed: false,
        },
        &mut out,
    );
    assert_eq!(legacy_composed, "é".as_bytes());
    assert_ne!(legacy_composed, out);

    // Enable Kitty keyboard full flags (`\x1b[>31u`)
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
    assert_eq!(out, b"\x1b[97;1:3u");

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

    // macOS Private Use fn char filtered by `typed()` -> no `;63232` text suffix
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
    assert_eq!(consumed(&mk_key("A", 1 | 2, u32::from('a'), false)), 0);
    assert_eq!(consumed(&mk_key("A", 1 | 4, u32::from('a'), false)), 1);
    assert_eq!(consumed(&mk_key("é", 1 | 4, u32::from('e'), true)), 1 | 4);
    assert_eq!(consumed(&mk_key("é", 4, u32::from('e'), true)), 4);

    // Pointer: SGR button-event mode (?1002;1006h) verifies buttons 0/1/2, per-view `held_buttons` bitmask, motion, wheel!
    assert!(!vt.meta().modes.mouse);
    vt.feed(b"\x1b[?1002;1006h", &mut ev);
    assert!(vt.meta().modes.mouse);
    let cell_px = CellPx { width: 8, height: 16 };
    let mk_ptr = |action, button, x_px, y_px| PointerInput { action, button, modifiers: 0, x_px, y_px, cell_px };
    let mut held = 0u8;

    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut held, &mut out);
    assert!(out.is_empty());

    // Press right (2) -> drag outside clamps to col 40 -> release right (2) -> outside drag silent
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Press, 2, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<2;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 2, 400, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<34;40;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Release, 2, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<2;2;2m");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 2, 400, 16), &mut held, &mut out);
    assert!(out.is_empty());

    // Press left (0) + press middle (1), release middle (1) -> left (0) STILL holds outside drag -> release left (0) -> silent!
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Press, 0, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<0;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Press, 1, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<1;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Release, 1, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<1;2;2m");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<32;40;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Release, 0, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<0;2;2m");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut held, &mut out);
    assert!(out.is_empty());

    for unnamed in [3u8, 5] {
        out.clear();
        vt.encode_pointer(&mk_ptr(PointerAction::Press, unnamed, 8, 16), &mut held, &mut out);
        vt.encode_pointer(&mk_ptr(PointerAction::Motion, 0, 400, 16), &mut held, &mut out);
        assert!(out.is_empty());
    }

    // Wheel up/down
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::WheelUp, 0, 8, 16), &mut held, &mut out);
    assert_eq!(out, b"\x1b[<64;2;2M");
    out.clear();
    vt.encode_pointer(&mk_ptr(PointerAction::WheelDown, 0, 8, 16), &mut held, &mut out);
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
fn image_store_o1_lru_eviction_recovery_placeholder_dirtying_and_hostile_rejection() {
    // 1. PNG codec & pixel budget
    assert!(encode_png_pixels(0, 2, 4, &[]).is_none());
    assert!(encode_png_pixels(2, 0, 4, &[]).is_none());
    assert!(encode_png_pixels(2, 2, 5, &[0; 20]).is_none());
    let rgb_png = encode_png_pixels(2, 1, 3, &[10, 20, 30, 40, 50, 60]).unwrap();
    let (w, h, rgba) = decode_png_rgba(&rgb_png).unwrap();
    assert_eq!((w, h), (2, 1));
    assert_eq!(rgba, vec![10, 20, 30, 255, 40, 50, 60, 255]);

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

    let mut big_hdr = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut big_hdr, 5000, 5000);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let _ = enc.write_header().unwrap();
    }
    assert!(decode_png_rgba(&big_hdr).is_none());
    let mut bad = rgb_png.clone();
    bad[0] = 0;
    assert!(decode_png_rgba(&bad).is_none());

    assert_eq!(png_rgba_pixel_count(0, 10, 64), None);
    assert_eq!(png_rgba_pixel_count(4, 4, 64), Some(16));
    assert_eq!(png_rgba_pixel_count(4, 4, 63), None);
    assert_eq!(png_rgba_pixel_count(u32::MAX, u32::MAX, u64::MAX), None);

    // 2. O(1) ImageStore LRU: touch(), dedup, get_chunk, queue compaction
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
    assert!(store.touch(h1));
    assert!(!store.touch(apack_proto::types::BlobHash([0xFF; 32])));

    // Promote h2 via dedup insert -> h3 MUST be evicted when h4 arrives!
    let c1 = store.get_chunk(h1, 0, 3).unwrap();
    assert_eq!(c1.data, vec![1, 1, 1]);
    assert_eq!(store.insert_encoded(ImageEncoding::Png, vec![2; 5]), Some(h2));
    let h4 = store.insert_encoded(ImageEncoding::Png, vec![4; 5]).unwrap();
    assert!(store.get_chunk(h1, 0, 5).is_some());
    assert!(store.get_chunk(h2, 0, 5).is_some());
    assert!(store.get_chunk(h3, 0, 5).is_none());
    assert!(store.get_chunk(h4, 0, 5).is_some());

    // Repeated touches exercise lazy LRU compaction cleanly without losing order
    for _ in 0..80 {
        assert!(store.touch(h2));
        assert!(store.touch(h4));
    }
    let h5 = store.insert_encoded(ImageEncoding::Png, vec![5; 5]).unwrap();
    assert!(store.get_chunk(h1, 0, 5).is_none());
    assert!(store.get_chunk(h2, 0, 5).is_some());
    assert!(store.get_chunk(h4, 0, 5).is_some());
    assert!(store.get_chunk(h5, 0, 5).is_some());

    // Pure `get_chunk()`-only LRU promotion (kills `clock += 1` -> `-=` / `*=` in `get_chunk`) & `insert_pixels`!
    let mut s_chunk = ImageStore::new(10);
    let ga = s_chunk.insert_encoded(ImageEncoding::Png, vec![41; 5]).unwrap();
    let gb = s_chunk.insert_encoded(ImageEncoding::Png, vec![42; 5]).unwrap();
    assert!(s_chunk.get_chunk(ga, 0, 5).is_some());
    assert!(s_chunk.get_chunk(gb, 0, 5).is_some()); // gb is now strictly newer than ga; with `clock *= 1`, gb keeps stale stamp 2 and gets evicted!
    let _gc = s_chunk.insert_encoded(ImageEncoding::Png, vec![43; 5]).unwrap();
    assert!(s_chunk.get_chunk(gb, 0, 5).is_some() && s_chunk.get_chunk(ga, 0, 5).is_none());
    let mut s_px = ImageStore::new(4096);
    let px_hash = s_px.insert_pixels(2, 1, 3, &[10, 20, 30, 40, 50, 60]).unwrap();
    assert!(s_px.get_chunk(px_hash, 0, 1024).is_some());

    // Pure `touch()`-only LRU promotion (kills `clock += 1` -> `*= 1` in `touch`) & exact `push_lru` compaction threshold!
    let mut s_touch = ImageStore::new(10);
    let ta = s_touch.insert_encoded(ImageEncoding::Png, vec![11; 5]).unwrap();
    let tb = s_touch.insert_encoded(ImageEncoding::Png, vec![22; 5]).unwrap();
    assert!(s_touch.touch(ta));
    let _tc = s_touch.insert_encoded(ImageEncoding::Png, vec![33; 5]).unwrap();
    assert!(s_touch.get_chunk(ta, 0, 5).is_some() && s_touch.get_chunk(tb, 0, 5).is_none());

    let mut s_cmp = ImageStore::new(10);
    let cm = s_cmp.insert_encoded(ImageEncoding::Png, vec![7; 5]).unwrap(); // blobs=1 -> threshold = 1*4 + 64 = 68
    for _ in 0..67 {
        assert!(s_cmp.touch(cm));
    }
    assert_eq!(s_cmp.lru_len(), 68, "68 entries == threshold must NOT compact yet");
    assert!(s_cmp.touch(cm));
    assert_eq!(s_cmp.lru_len(), 1, "69th entry > threshold must compact stale LRU duplicates");

    // Pure insert-only LRU promotion in both orders
    let mut s_ab = ImageStore::new(10);
    let ka = s_ab.insert_encoded(ImageEncoding::Png, vec![10; 5]).unwrap();
    let kb = s_ab.insert_encoded(ImageEncoding::Png, vec![20; 5]).unwrap();
    assert_eq!(s_ab.insert_encoded(ImageEncoding::Png, vec![10; 5]), Some(ka));
    let _kc = s_ab.insert_encoded(ImageEncoding::Png, vec![30; 5]).unwrap();
    assert!(s_ab.get_chunk(ka, 0, 5).is_some() && s_ab.get_chunk(kb, 0, 5).is_none());

    let h6 = store.insert_encoded(ImageEncoding::Png, vec![6; 15]).unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store.used_bytes(), 15);
    assert!(store.get_chunk(h6, 0, 15).is_some());

    // 3. GhosttyEngine: hostile t=f rejected, raw RGB + replacement, post-eviction recovery, AND placeholder dirtying!
    let shared = Arc::new(Mutex::new(ImageStore::new(4096)));
    let mut cfg = GhosttyConfig::new(grid(20, 6));
    cfg.cell_px = CellPx { width: 8, height: 16 };
    cfg.image_store = Some(Arc::clone(&shared));
    let mut vt = GhosttyEngine::new(cfg).unwrap();
    assert!(Arc::ptr_eq(vt.image_store(), &shared));
    let mut ev = Vec::new();

    let hostile = format!("\x1b_Ga=T,t=f,f=100,i=99;{}\x1b\\", base64_encode(b"/etc/passwd"));
    vt.feed(hostile.as_bytes(), &mut ev);
    assert!(vt.images().is_empty());

    let raw_rgb = [255u8, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
    let direct_rgb = format!("\x1b_Ga=T,f=24,s=2,v=2,i=7;{}\x1b\\", base64_encode(&raw_rgb));
    vt.feed(direct_rgb.as_bytes(), &mut ev);
    let imgs1 = vt.images();
    assert_eq!(imgs1.len(), 1);
    let blob1 = imgs1[0].blob;

    // Simulate another session evicting `blob1` from the shared global `ImageStore`:
    // next `vt.images()` MUST detect the eviction and re-populate `ImageStore` cleanly (zero 404)!
    {
        let mut s = shared.lock().unwrap();
        let _ = s.insert_encoded(ImageEncoding::Png, vec![0xAA; 4096]).unwrap();
        assert!(s.get_chunk(blob1, 0, 64).is_none());
    }
    let imgs_recovered = vt.images();
    assert_eq!(imgs_recovered.len(), 1);
    assert_eq!(imgs_recovered[0].blob, blob1);
    assert!(shared.lock().unwrap().get_chunk(blob1, 0, 64).is_some(), "evicted live image must re-populate ImageStore");

    // Re-transmit image id=7 with DIFFERENT pixels -> generation bumps and new blob replaces old
    let raw_rgb2 = [0u8, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110];
    let direct_rgb2 = format!("\x1b_Ga=T,f=24,s=2,v=2,i=7;{}\x1b\\", base64_encode(&raw_rgb2));
    vt.feed(direct_rgb2.as_bytes(), &mut ev);
    let imgs2 = vt.images();
    assert_eq!(imgs2.len(), 1);
    assert_ne!(imgs2[0].blob, blob1);

    // Put U+10EEEE placeholders on screen FIRST, drain dirty, THEN upload virtual image id=11 ->
    // placeholder-bearing rows MUST be marked dirty automatically when `images_gen` changes!
    let place_rows = format!(
        "\r\n\x1b[38;2;0;0;11;58;2;0;0;3m{}\x1b[0m\r\n\x1b[38;2;0;0;11;58;2;0;0;3m{}\u{030D}\u{030D}\x1b[0m",
        PLACEHOLDER, PLACEHOLDER
    );
    vt.feed(place_rows.as_bytes(), &mut ev);
    let row_b = vt.meta().cursor.row;
    let row_a = row_b - 1;
    let _ = vt.take_dirty_viewport();

    let g_before_virt = vt.generations().grid;
    let png_virt = encode_png_rgba(16, 32, &[99u8; 16 * 32 * 4]).unwrap();
    let virt_upload = format!("\x1b_Ga=T,U=1,f=100,i=11,p=3,c=2,r=2;{}\x1b\\", base64_encode(&png_virt));
    vt.feed(virt_upload.as_bytes(), &mut ev);
    let d_virt = vt.take_dirty_viewport();
    assert!(!d_virt.full_dirty);
    assert!(d_virt.dirty_rows.contains(&row_a) && d_virt.dirty_rows.contains(&row_b));
    assert!(!d_virt.dirty_rows.contains(&0), "non-placeholder row 0 must NOT be marked dirty");
    assert_eq!(vt.generations().grid, g_before_virt + 2);

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
    let st_rgb = PackedStyle { fg: 0x01_00_00_05, bg: 0, underline_color: 10, flags: 0 };
    let st_pal = PackedStyle { fg: 6, bg: 0, underline_color: 0, flags: 0 };
    let s0 = Said::of(&st_rgb, ['\u{0305}', '\u{030D}', '\u{030E}']);
    let mut run = PlaceholderRun::begin(3, s0);
    assert_eq!(run.image(), 5 + (2 << 24));
    assert_eq!(run.placement(), 9);
    assert_eq!(run.image_row, 0);
    assert_eq!(run.image_column, 1);

    assert!(!run.takes(5, Said::of(&st_rgb, [])));
    assert!(!run.takes(4, Said::of(&PackedStyle { fg: 0x01_00_00_06, ..st_rgb }, [])));
    assert!(!run.takes(4, Said::of(&PackedStyle { underline_color: 11, ..st_rgb }, [])));
    assert!(!run.takes(4, Said::of(&st_rgb, ['\u{030D}'])));
    assert!(!run.takes(4, Said::of(&st_rgb, ['\u{0305}', '\u{0305}'])));
    assert!(!run.takes(4, Said::of(&st_rgb, ['\u{0305}', '\u{030E}', '\u{0305}'])));
    assert!(run.takes(4, Said::of(&st_rgb, ['\u{0305}', '\u{030E}', '\u{030E}'])));
    assert_eq!(run.width, 2);

    let run_pal = PlaceholderRun::begin(0, Said::of(&st_pal, ['X']));
    assert_eq!(run_pal.image(), 5);
    assert_eq!(run_pal.placement(), 0);

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

    let f_auto = fit(&mk_run(0, 0, 2), (0, 0), (15, 25), (10, 10)).unwrap();
    assert_eq!(f_auto.dst_px, [18, 10]);

    assert!(fit(&mk_run(0, 0, 5), (5, 5), (100, 20), (10, 10)).is_none());
    assert!(fit(&mk_run(4, 0, 5), (5, 5), (100, 20), (10, 10)).is_none());
    let f_mid_w = fit(&mk_run(2, 0, 5), (5, 5), (100, 20), (10, 10)).unwrap();
    assert_eq!(f_mid_w.src_rect, [0, 0, 100, 20]);
    assert_eq!(f_mid_w.dst_px, [50, 10]);

    let f_top = fit(&mk_run(1, 0, 5), (5, 5), (100, 30), (10, 10)).unwrap();
    assert_eq!(f_top.src_rect, [0, 0, 100, 5]);
    assert_eq!(f_top.dst_px, [50, 3]);
    let f_int_y = fit(&mk_run(2, 0, 5), (5, 5), (100, 30), (10, 10)).unwrap();
    assert_eq!(f_int_y.src_rect, [0, 5, 100, 20]);
    assert_eq!(f_int_y.dst_px, [50, 10]);
    let f_bot = fit(&mk_run(3, 0, 5), (5, 5), (100, 30), (10, 10)).unwrap();
    assert_eq!(f_bot.src_rect, [0, 25, 100, 5]);
    assert_eq!(f_bot.dst_px, [50, 3]);

    let f_clamp_y = fit(&mk_run(0, 0, 4), (4, 1), (100, 10), (10, 20)).unwrap();
    assert_eq!(f_clamp_y.src_rect, [0, 0, 100, 10]);
    assert_eq!(f_clamp_y.dst_px, [40, 4]);

    assert!(fit(&mk_run(0, 0, 1), (5, 5), (30, 100), (10, 10)).is_none());
    let f_left = fit(&mk_run(0, 1, 1), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_left.src_rect, [0, 0, 5, 20]);
    assert_eq!(f_left.dst_px, [3, 10]);
    let f_int_x = fit(&mk_run(0, 2, 1), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_int_x.src_rect, [5, 0, 20, 20]);
    assert_eq!(f_int_x.dst_px, [10, 10]);
    let f_right = fit(&mk_run(0, 3, 1), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_right.src_rect, [25, 0, 5, 20]);
    assert_eq!(f_right.dst_px, [3, 10]);
    let f_clamp_x = fit(&mk_run(0, 0, 5), (5, 5), (30, 100), (10, 10)).unwrap();
    assert_eq!(f_clamp_x.src_rect, [0, 0, 30, 20]);
    assert_eq!(f_clamp_x.dst_px, [15, 10]);
}

#[test]
fn snapshot_export_mid_sequence_at_ground_corrupt_rejection_and_blink_preservation() {
    let mut src = GhosttyEngine::new(GhosttyConfig::new(grid(80, 4))).unwrap();
    let mut ev = Vec::new();
    assert!(src.at_ground());
    for i in 0..1_500 {
        src.feed(format!("snap-line-{i:04} padding-to-fill-multiple-64k-pages-cleanly\r\n").as_bytes(), &mut ev);
    }
    // Enable cursor blink (DEC mode 12) in source snapshot, and verify mid-CSI `at_ground() == false` still exports!
    src.feed(b"\x1b[?12h\x1b[38;2;", &mut ev);
    assert!(!src.at_ground());
    let snap = src.export_snapshot(1024 * 1024).unwrap();
    assert!(!snap.is_empty());
    assert_eq!(last_snapshot_record_end(&snap), snap.len());

    let mut synth = vec![0u8; 25];
    synth[12..16].copy_from_slice(&50u32.to_le_bytes());
    assert_eq!(last_snapshot_record_end(&synth), 10);
    assert_eq!(last_snapshot_record_end(&snap[..5]), 5);

    // Exceeding `max_bytes` must fail with `OUT_OF_SPACE` rather than returning an unrestorable truncated snapshot.
    assert!(src.export_snapshot(snap.len() / 2).is_err());

    // Destination has `cursor_defaults = (Bar, false)` -> restoring MUST preserve `src`'s blinking=true
    // WITHOUT corrupting the in-flight `\x1b[38;2;` CSI continuation!
    let mut dst_cfg = GhosttyConfig::new(grid(10, 2));
    dst_cfg.cell_px = CellPx { width: 8, height: 16 };
    let mut dst = GhosttyEngine::new(dst_cfg).unwrap();
    dst.set_cursor_defaults(CursorShape::Bar, false);

    // Corrupting a trailing record CRC MUST fail `restore_snapshot` AND leave `dst` completely intact!
    let mut corrupt = snap.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    assert!(dst.restore_snapshot(&corrupt).is_err());
    assert_eq!(dst.meta().epoch, 1);
    assert_eq!(dst.meta().size, grid(10, 2));

    // Valid restore succeeds twice (epoch 1 -> 2 -> 3), preserves blink=true, continuation tracking, & cell_px!
    dst.restore_snapshot(&snap).unwrap();
    assert_eq!(dst.meta().epoch, 2);
    assert_eq!(dst.meta().size, grid(80, 4));
    assert!(dst.meta().cursor.blinking, "restore_snapshot must preserve restored DEC mode 12 blink state");
    assert!(dst.meta().scrollback_lines >= 1_490);
    assert!(!dst.at_ground(), "restored mid-CSI terminal must retain continuation tracking");
    // Re-exporting mid-sequence right after restore must succeed because RETAIN_CONTINUATION was configured!
    assert!(dst.export_snapshot(1024 * 1024).is_ok());
    ev.clear();
    // Finish the mid-CSI sequence from `snap` (`0;0;0m`) then query cell pixel size via CSI 16t!
    dst.feed(b"0;0;0m\x1b[16t", &mut ev);
    assert!(dst.at_ground());
    assert!(ev.iter().any(|e| matches!(e, VtEvent::DeviceResponse(b) if b == b"\x1b[6;16;8t")));
    let mut active = Vec::new();
    dst.extract_viewport_rows(&[0, 1, 2, 3], &mut active);
    assert!(
        active.iter().all(|r| !r.text.contains("0;0;0m")),
        "mid-CSI continuation must not be broken by injected DEC mode 12 bytes: {active:?}"
    );
    let mut hist = Vec::new();
    dst.extract_history_rows(0, 2, &mut hist);
    assert!(hist[0].text.starts_with("snap-line-0000"));
    assert!(hist[1].text.starts_with("snap-line-0001"));

    dst.restore_snapshot(&snap).unwrap();
    assert_eq!(dst.meta().epoch, 3);

    // Verify `restore_snapshot` resets `InputEncoders` cached `last_cell` on identical-size/mode restore!
    let mut m_src = GhosttyEngine::new(GhosttyConfig::new(grid(10, 4))).unwrap();
    m_src.feed(b"\x1b[?1002;1006h", &mut ev);
    let m_snap = m_src.export_snapshot(64 * 1024).unwrap();
    let mut m_dst = GhosttyEngine::new(GhosttyConfig::new(grid(10, 4))).unwrap();
    m_dst.feed(b"\x1b[?1002;1006h", &mut ev);
    let cell_px = CellPx { width: 8, height: 16 };
    let mut held = 0u8;
    let mut pout = Vec::new();
    let press = PointerInput { action: PointerAction::Press, button: 0, modifiers: 0, x_px: 8, y_px: 16, cell_px };
    let drag = PointerInput { action: PointerAction::Motion, button: 0, modifiers: 0, x_px: 24, y_px: 16, cell_px };
    m_dst.encode_pointer(&press, &mut held, &mut pout);
    m_dst.encode_pointer(&drag, &mut held, &mut pout);
    pout.clear();
    // Same cell again is deduplicated before restore
    m_dst.encode_pointer(&drag, &mut held, &mut pout);
    assert!(pout.is_empty());
    // Restore same-size/same-mode snapshot -> `reset_cache()` forgets `last_cell`, so `(24,16)` emits again!
    m_dst.restore_snapshot(&m_snap).unwrap();
    m_dst.encode_pointer(&drag, &mut held, &mut pout);
    assert_eq!(pout, b"\x1b[<32;4;2M");
}

#[test]
fn review_mutants_batched_text_cell_px_resize_placeholder_bg_long_grapheme_and_search_truncation() {
    let mut ev = Vec::new();
    // 1. text_of_batched across soft-wrapped logical lines, multi-byte UTF-8, and max_bytes boundaries
    let mut vt = GhosttyEngine::new(GhosttyConfig::new(grid(6, 10))).unwrap();
    // Line 0: "hi"; Lines 1-2: soft-wrapped 10-char line "abcdefghij"; Line 3: "éé"; Line 4: "world"
    vt.feed(b"hi\r\nabcdefghij\r\n\xc3\xa9\xc3\xa9\r\nworld", &mut ev);
    let full = CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 4, col: 3 } };
    assert_eq!(vt.text_of_batched(full, 0, 2), "");
    assert_eq!(
        vt.text_of_batched(CellSpan { start: CellPos { line: 3, col: 0 }, end: CellPos { line: 1, col: 0 } }, 64, 2),
        ""
    );
    // With batch_lines=2: batch 1 starts at line 0 and initially ends at line 1 (mid soft-wrap!),
    // so `line_at(1)` MUST expand batch_end to 2 so "abcdefghij" is NOT split by a spurious '\n'!
    let expected = vt.text_of_batched(full, 1024, 256);
    assert_eq!(expected, "hi\nabcdefghij\n\u{00e9}\u{00e9}\nworl");
    assert_eq!(vt.text_of_batched(full, 1024, 2), expected);
    // Exact length of batch 1 ("hi\nabcdefghij" = 13 bytes): must stop without appending trailing '\n'!
    assert_eq!(vt.text_of_batched(full, 13, 2), "hi\nabcdefghij");
    // Exact byte cap mid-2-byte UTF-8 char
    let t_trunc = vt.text_of_batched(full, 15, 2);
    assert!(t_trunc.len() <= 15);
    assert_eq!(t_trunc, &expected[..expected.floor_char_boundary(15)]);
    // text_of_rect respects max_bytes cleanly
    let rect = vt.text_of_rect(CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 2, col: 4 } }, 4);
    assert!(rect.len() <= 4);
    assert_eq!(rect, "hi\na");

    // 2. >16-codepoint grapheme cluster exercises read_grapheme_codepoints OUT_OF_SPACE heap retry!
    let mut g_vt = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    let mut cluster = String::from("a");
    for _ in 0..20 {
        cluster.push('\u{0301}');
    }
    g_vt.feed(cluster.as_bytes(), &mut ev);
    let mut g_rows = Vec::new();
    g_vt.extract_viewport_rows(&[0], &mut g_rows);
    assert!(
        g_rows[0].text.chars().count() >= 18,
        "long grapheme cluster (>16 codepoints) must survive OUT_OF_SPACE retry, got {} chars",
        g_rows[0].text.chars().count()
    );

    // 3. U+10EEEE placeholder (both bare CODEPOINT and CODEPOINT_GRAPHEME with combining diacritic)
    //    preserves non-default RGB bg while stripping fg/flags, AND cell_px-only resize bumps
    //    both `gens.grid` and `gens.images` cleanly!
    let mut p_vt = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    // 3a. Terminal with ZERO images: cell_px-only resize must NOT bump images_gen!
    let no_img_gen = p_vt.generations().images;
    p_vt.resize(grid(20, 4), CellPx { width: 9, height: 18 }, &mut ev);
    assert_eq!(p_vt.generations().images, no_img_gen);
    // 3b. Direct (non-placeholder) Kitty placement: cell_px-only resize still bumps images_gen!
    let px_b64 = base64_encode(&[255, 0, 0, 255]);
    p_vt.feed(format!("\x1b_Ga=T,f=32,s=1,v=1,i=2,c=2,r=1,q=2;{px_b64}\x1b\\").as_bytes(), &mut ev);
    let direct_gen = p_vt.generations().images;
    p_vt.resize(grid(20, 4), CellPx { width: 10, height: 20 }, &mut ev);
    assert!(p_vt.generations().images > direct_gen, "direct Kitty placement must bump images_gen on cell_px resize");
    // 3c. Unicode placeholder (bare on row 0, grapheme with combining U+0305 on row 1)
    p_vt.feed(format!("\x1b_Ga=T,f=32,s=1,v=1,i=1,U=1,c=2,r=1,q=2;{px_b64}\x1b\\").as_bytes(), &mut ev);
    p_vt.feed(
        "\r\n\x1b[1;38;5;1;48;2;10;20;30m\u{10EEEE}\x1b[0m\r\n\x1b[1;38;5;1;48;2;40;50;60m\u{10EEEE}\u{0305}\x1b[0m"
            .as_bytes(),
        &mut ev,
    );
    let _ = p_vt.take_dirty_viewport();
    let mut p_rows = Vec::new();
    p_vt.extract_viewport_rows(&[1, 2], &mut p_rows);
    let bg1 = 0x01_00_00_00 | (10 << 16) | (20 << 8) | 30;
    let bg2 = 0x01_00_00_00 | (40 << 16) | (50 << 8) | 60;
    assert!(p_rows[0].styles.iter().any(|s| s.bg == bg1 && s.fg == 0 && s.flags == 0));
    assert!(p_rows[1].styles.iter().any(|s| s.bg == bg2 && s.fg == 0 && s.flags == 0));
    let gen_before = p_vt.generations();
    p_vt.resize(grid(20, 4), CellPx { width: 12, height: 24 }, &mut ev);
    let gen_after = p_vt.generations();
    assert!(gen_after.images > gen_before.images, "cell_px-only resize must bump images_gen");
    assert_eq!(gen_after.grid, gen_before.grid + 2, "each placeholder row must increment gens.grid by 1");
    let dirty = p_vt.take_dirty_viewport();
    assert!(dirty.dirty_rows.contains(&1) && dirty.dirty_rows.contains(&2));

    // 4. set_cursor_defaults toggles blinking cleanly at ground
    let mut c_vt = GhosttyEngine::new(GhosttyConfig::new(grid(10, 4))).unwrap();
    c_vt.set_cursor_defaults(CursorShape::Bar, true);
    assert!(c_vt.meta().cursor.blinking);
    c_vt.set_cursor_defaults(CursorShape::Bar, false);
    assert!(!c_vt.meta().cursor.blinking);

    // 5. search_lines_bounded enforces max_alloc without OOM (signalling truncated=true) and
    //    returns all matches when total <= max_alloc!
    let mut s_vt = GhosttyEngine::new(GhosttyConfig::new(grid(40, 10))).unwrap();
    for _ in 0..5 {
        s_vt.feed(b"xxxxxxxxxx\r\n", &mut ev);
    }
    let mut found = Vec::new();
    // 50 matches, max_alloc = 50 -> exact boundary succeeds with 50 matches and truncated = false!
    assert!(!s_vt.search_lines_bounded(0, s_vt.meta().total_lines(), "x", false, 100, 100, 50, &mut found));
    assert_eq!(found.len(), 50);
    // 50 matches, max_alloc = 49 -> refuses unbounded allocation and reports truncated = true!
    found.clear();
    assert!(s_vt.search_lines_bounded(0, s_vt.meta().total_lines(), "x", false, 100, 100, 49, &mut found));
    assert!(found.is_empty());

    // 6. text_of_batched preserves separating '\n' when the first batch formats to empty string!
    let mut b_vt = GhosttyEngine::new(GhosttyConfig::new(grid(20, 4))).unwrap();
    b_vt.feed(b"\r\nhello", &mut ev);
    let b_span = CellSpan { start: CellPos { line: 0, col: 0 }, end: CellPos { line: 1, col: 19 } };
    assert_eq!(b_vt.text_of_batched(b_span, 1024, 1), "\nhello");
}
