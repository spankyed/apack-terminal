//! A keyframe for a case to replay, of a screen that was fed what the case says.
//!
//! **Taken from a screen, as a recording's are.** A keyframe is the core's account of a terminal and is not
//! something a case can write out by hand, so one is made the way a session makes one: a screen of that
//! size, fed, and written out.
//!
//! Not a test target: cargo builds the files directly under `tests/`, and a subdirectory is only reachable
//! through the `#[path]` the suite's root uses.
#![allow(dead_code)]

use apack_terminal::recording::Entry;
use apack_terminal::screen::Screen;

/// The keyframe of a `cols` by `rows` screen that was fed `drawn`.
pub fn keyframe(cols: u16, rows: u16, drawn: &[u8]) -> Entry {
    let mut screen = Screen::new(cols, rows);
    screen.feed(drawn);
    keyframe_of(&screen, cols, rows)
}

/// The keyframe of `screen` as it stands, which is `cols` by `rows`.
pub fn keyframe_of(screen: &Screen, cols: u16, rows: u16) -> Entry {
    Entry::Keyframe { cols, rows, state: screen.keyframe().expect("a screen between sequences is written out") }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(bytes: &[u8]) -> String {
    let mut text = String::new();
    for chunk in bytes.chunks(3) {
        let padded = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let bits = (u32::from(padded[0]) << 16) | (u32::from(padded[1]) << 8) | u32::from(padded[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                text.push(char::from(BASE64[((bits >> (18 - 6 * index)) & 63) as usize]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

/// A picture of `width` by `height`, four bytes a pixel, sent as a program sends one — in pieces of the
/// size the protocol allows, the last marked as the last — and each piece a write of its own. `how` is
/// what the program asks be done with it: `T` to show it where the cursor is, `t` to keep it.
pub fn picture(id: u32, width: u32, height: u32, how: char) -> Vec<Vec<u8>> {
    let pixels: Vec<u8> = (0..width * height)
        .flat_map(|pixel| [(pixel % 251) as u8, (pixel % 241) as u8, (pixel % 239) as u8, 255])
        .collect();
    let encoded = base64(&pixels);
    let pieces: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    pieces
        .iter()
        .enumerate()
        .map(|(index, piece)| {
            let more = u8::from(index + 1 != pieces.len());
            let control = if index == 0 {
                format!("a={how},f=32,i={id},s={width},v={height},q=2,m={more}")
            } else {
                format!("m={more}")
            };
            [format!("\x1b_G{control};").as_bytes(), piece, b"\x1b\\"].concat()
        })
        .collect()
}
