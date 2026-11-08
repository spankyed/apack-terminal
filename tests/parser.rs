//! The VT core tested **as a parser**, which is what Phase P of apack's terminal-ownership goal asks for.
//!
//! Two things, and the second is the one that finds real bugs:
//!
//! - **The grid a byte stream produces is snapshotted** (`insta`), so a change in what the parser does to a
//!   stream is a reviewable diff rather than an assertion someone has to word.
//! - **The same stream is split at every byte boundary and must produce the same grid.** Terminal output
//!   arrives in whatever sizes a `read` returned, so an escape sequence, a UTF-8 code point or an OSC string
//!   is routinely delivered in pieces. This is where VT parsers break, and a wrapper that buffered a chunk
//!   or reset anything per call would pass every test that feeds a whole stream.
//!
//! The streams carry what the goal names — UTF-8, CSI, OSC, DCS, APC — because each is parsed by a different
//! part of the state machine and each has a different way of being cut in half.

use apack_terminal::screen::Screen;
use proptest::prelude::*;

/// One stream per thing that is parsed differently, each named by what cutting it in half would break.
fn streams() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("plain and wrapping", b"hello world\r\nsecond line\r\n".to_vec()),
        // A cursor move, an erase and SGR: the CSI parameter buffer carries across a split
        ("csi", b"aaaaaaaaaa\x1b[1;1H\x1b[31mred\x1b[0m\x1b[2;3Hhere\x1b[K".to_vec()),
        // Multi-byte code points, including one that is two cells wide
        ("utf-8", "naïve → 日本語 🚀 done\r\n".as_bytes().to_vec()),
        // A title and a cwd report: an OSC string terminated two different ways (BEL and ST)
        ("osc", b"\x1b]0;a title\x07\x1b]7;file:///tmp/somewhere\x1b\\after osc".to_vec()),
        // A DCS string, which has its own terminator and must not reach the screen
        ("dcs", b"before\x1bP1$r0m\x1b\\after dcs".to_vec()),
        // An APC string, which xterm ignores and so must this
        ("apc", b"before\x1b_some application command\x1b\\after apc".to_vec()),
        // The alternate screen entered, drawn on and left: what `vim` and `less` do
        ("alternate screen", b"scrollback line\r\n\x1b[?1049h\x1b[2J\x1b[HTUI content\x1b[?1049l".to_vec()),
        // A scroll region, which moves lines into history
        ("scrolling", b"\x1b[1;3r\x1b[Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\n".to_vec()),
    ]
}

fn screen_after(chunks: &[&[u8]], cols: u16, rows: u16) -> apack_terminal::screen::Snapshot {
    let mut screen = Screen::new(cols, rows);
    for chunk in chunks {
        screen.feed(chunk);
    }
    screen.snapshot(20)
}

#[test]
fn a_byte_stream_produces_a_grid() {
    for (name, stream) in streams() {
        let snapshot = screen_after(&[&stream], 20, 4);
        // Each line is drawn between bars so a blank row and a dropped row are not the same picture, and
        // so trailing whitespace is visible in the review rather than trimmed out of it
        let grid: String = snapshot.lines.iter().map(|line| format!("|{line}|\n")).collect();
        insta::assert_snapshot!(
            name,
            format!(
                "alt_screen: {}\ncursor: {},{}\nrows: {}\n---\n{grid}",
                snapshot.alt_screen,
                snapshot.cursor_row,
                snapshot.cursor_column,
                snapshot.lines.len(),
            )
        );
    }
}

#[test]
fn every_single_byte_boundary_produces_the_same_grid() {
    for (name, stream) in streams() {
        let whole = screen_after(&[&stream], 20, 4);
        // Exhaustive for one split, which is cheap and covers the boundary inside every sequence
        for at in 1..stream.len() {
            let split = screen_after(&[&stream[..at], &stream[at..]], 20, 4);
            assert_eq!(split, whole, "{name}: split at {at} of {} produced a different grid", stream.len());
        }
        // And one byte at a time, which is every boundary at once
        let singles: Vec<&[u8]> = stream.iter().map(std::slice::from_ref).collect();
        assert_eq!(screen_after(&singles, 20, 4), whole, "{name}: one byte at a time");
    }
}

// Arbitrary splits, however many: the exhaustive case above cuts once, and a sequence can be cut twice.
proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn any_chunking_produces_the_same_grid(
        which in 0usize..8,
        mut cuts in prop::collection::vec(0usize..200, 0..6),
    ) {
        let (name, stream) = streams().swap_remove(which);
        let whole = screen_after(&[&stream], 20, 4);

        cuts.retain(|at| *at < stream.len());
        cuts.sort_unstable();
        cuts.dedup();
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut from = 0;
        for at in cuts {
            if at > from {
                chunks.push(&stream[from..at]);
                from = at;
            }
        }
        chunks.push(&stream[from..]);

        prop_assert_eq!(screen_after(&chunks, 20, 4), whole, "{}", name);
    }
}

/// A resize under an alternate-screen app is where coalescing and ordering mistakes surface, and the
/// broker's side of the check Phase B item 5 asks for — `tests/broker.rs` drives a real `less` through it.
#[test]
fn an_alternate_screen_survives_a_resize() {
    let mut screen = Screen::new(40, 10);
    screen.feed(b"shell scrollback\r\n");
    screen.feed(b"\x1b[?1049h\x1b[2J\x1b[HTUI content here");
    assert!(screen.snapshot(0).alt_screen, "the alternate screen is showing");

    screen.resize(20, 5);
    let resized = screen.snapshot(0);
    assert_screen_holds(&resized.lines, "TUI content");
    assert!(resized.alt_screen, "still on the alternate screen after a resize");
    assert_eq!(resized.lines.len(), 5, "the viewport is the new size");

    // Leaving it restores the primary screen, and the scrollback that was there is still there
    screen.feed(b"\x1b[?1049l");
    let left = screen.snapshot(20);
    assert!(!left.alt_screen, "back on the primary screen");
    assert_screen_holds(&left.lines, "shell scrollback");
}

fn assert_screen_holds(lines: &[String], wanted: &str) {
    assert!(lines.iter().any(|line| line.contains(wanted)), "wanted {wanted:?} among {lines:?}");
}
