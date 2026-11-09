//! The byte streams both parser suites read, defined once.
//!
//! **One definition, because two suites compare against it.** `parser.rs` snapshots the grid each stream
//! produces and splits it at every byte boundary; `conformance.rs` drives the same streams through
//! `libghostty-vt` and diffs the result against them. Duplicating the list would let the two drift, and
//! the second copy would be the one nobody updated.
//!
//! Not a test target: cargo builds the files directly under `tests/`, and a subdirectory is only reachable
//! through the `#[path]` both suites use.
#![allow(dead_code)]

/// One stream per thing that is parsed differently, each named by what cutting it in half would break.
pub fn streams() -> Vec<(&'static str, Vec<u8>)> {
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
