//! Images a program places on the screen: where each is told to be, that the line stays true as the screen
//! scrolls, the pixels a view is given, and that a session not asked to take images takes none.

use apack_terminal::screen::Screen;
use apack_terminal::view::wire::{ToView, body};
use apack_terminal::vt::images::IMAGE_CHUNK_BYTES;

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

/// A picture sent and shown where the cursor is, as a program sends one: in pieces of the size the
/// protocol allows, the last of them marked as the last.
fn shown(id: u32, width: u32, height: u32, rgba: &[u8], more: &str) -> Vec<u8> {
    let encoded = base64(rgba);
    let pieces: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    let mut bytes = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        let last = index + 1 == pieces.len();
        let control = if index == 0 {
            format!("a=T,f=32,i={id},s={width},v={height},q=2{more},m={}", u8::from(!last))
        } else {
            format!("m={}", u8::from(!last))
        };
        bytes.extend_from_slice(format!("\x1b_G{control};").as_bytes());
        bytes.extend_from_slice(piece);
        bytes.extend_from_slice(b"\x1b\\");
    }
    bytes
}

fn picture(width: u32, height: u32) -> Vec<u8> {
    (0..width * height).flat_map(|pixel| [(pixel % 251) as u8, (pixel % 241) as u8, (pixel % 239) as u8, 255]).collect()
}

/// A screen that takes images, with a view attached so lines are numbered, and a cell of ten by twenty.
fn viewed(cols: u16, rows: u16) -> (Screen, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut screen = Screen::new(cols, rows);
    screen.allow_images().expect("images are allowed");
    screen.resize(cols, rows, 10, 20);
    let (sink, sent) = std::sync::mpsc::channel();
    screen
        .attach_view(apack_terminal::view::publish::ViewId { connection: 1, channel: 1 }, sink)
        .expect("a view attaches");
    (screen, sent)
}

fn last_frame(sent: &std::sync::mpsc::Receiver<Vec<u8>>) -> apack_terminal::view::wire::Frame {
    let mut last = None;
    for message in sent.try_iter() {
        if let Ok(ToView::Frame(frame)) = ToView::decode(body(&message)) {
            last = Some(frame);
        }
    }
    last.expect("a frame was sent")
}

#[test]
fn an_image_is_told_where_it_is_and_stays_on_its_line_as_the_screen_scrolls() {
    let (mut screen, sent) = viewed(40, 10);
    let before = last_frame(&sent);

    screen.feed(b"one\r\ntwo\r\n   ");
    // Forty pixels by sixty is four cells by three, at a cell of ten by twenty
    screen.feed(&shown(7, 40, 60, &picture(40, 60), ""));
    screen
        .acknowledge(apack_terminal::view::publish::ViewId { connection: 1, channel: 1 }, before.sequence)
        .expect("an acknowledgement is taken");
    let after = last_frame(&sent);
    assert_ne!(after.images, before.images, "a view is told the images changed");

    let (epoch, cell, placed) = screen.images().expect("the images read");
    assert_eq!((epoch, cell), (after.epoch, (10, 20)));
    assert_eq!(placed.len(), 1);
    let (line, placement) = &placed[0];
    assert_eq!((placement.image, placement.column, placement.columns, placement.rows), (7, 3, 4, 3));
    assert_eq!(*line, after.first_active + 2);
    assert_eq!(placement.source, (0, 0, 40, 60));
    assert_eq!(placement.size, (40, 60));

    // Thirty more lines: the image is in the history now, on the line it was always on
    for line in 0..30 {
        screen.feed(format!("line {line}\r\n").as_bytes());
    }
    let (_, _, scrolled) = screen.images().expect("the images read");
    assert_eq!(scrolled.len(), 1);
    assert_eq!(scrolled[0].0, *line);
    assert_eq!(scrolled[0].1.generation, placement.generation);
}

#[test]
fn an_images_pixels_are_read_in_parts_that_join_into_the_picture() {
    let (mut screen, _sent) = viewed(80, 24);
    // Larger than one read answers with
    let (width, height) = (1_200, 1_000);
    let pixels = picture(width, height);
    assert!(pixels.len() > IMAGE_CHUNK_BYTES);
    screen.feed(&shown(3, width, height, &pixels, ""));

    let mut joined = Vec::new();
    loop {
        let read = screen.image(3, joined.len() as u32).expect("the image is there");
        assert_eq!((read.width, read.height, read.channels, read.total as usize), (width, height, 4, pixels.len()));
        assert!(read.bytes.len() <= IMAGE_CHUNK_BYTES);
        joined.extend_from_slice(&read.bytes);
        if joined.len() >= read.total as usize {
            break;
        }
        assert!(!read.bytes.is_empty(), "a read past the end would never finish");
    }
    assert!(joined == pixels, "the parts do not join into the picture that was sent");
    assert!(screen.image(4, 0).is_none());
}

#[test]
fn part_of_a_picture_is_placed_with_the_part_named() {
    let (mut screen, _sent) = viewed(40, 10);
    // A window of twenty by twenty out of the picture, starting ten in and five down, under the text
    screen.feed(&shown(9, 40, 60, &picture(40, 60), ",x=10,y=5,w=20,h=20,z=-3"));
    let (_, _, placed) = screen.images().expect("the images read");
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].1.source, (10, 5, 20, 20));
    assert_eq!(placed[0].1.z, -3);
}

/// A session that was not asked to take images holds none, whatever a program sends.
#[test]
fn a_session_not_asked_to_take_images_takes_none() {
    let mut screen = Screen::new(40, 10);
    screen.resize(40, 10, 10, 20);
    let (sink, _sent) = std::sync::mpsc::channel();
    screen
        .attach_view(apack_terminal::view::publish::ViewId { connection: 1, channel: 1 }, sink)
        .expect("a view attaches");
    screen.feed(&shown(7, 40, 60, &picture(40, 60), ""));
    assert!(screen.images().expect("the images read").2.is_empty());
    assert!(screen.image(7, 0).is_none());
}
