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

/// A PNG of `width` by `height`, written with the colour type and depth named from the samples given.
fn png_of(width: u32, height: u32, colour: png::ColorType, depth: png::BitDepth, samples: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    let mut encoder = png::Encoder::new(&mut encoded, width, height);
    encoder.set_color(colour);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header().expect("a header is written");
    writer.write_image_data(samples).expect("the pixels are written");
    writer.finish().expect("the file is finished");
    encoded
}

/// A PNG sent and shown where the cursor is, in pieces as `shown` sends raw pixels.
fn shown_png(id: u32, encoded: &[u8]) -> Vec<u8> {
    let text = base64(encoded);
    let pieces: Vec<&[u8]> = text.as_bytes().chunks(4096).collect();
    let mut bytes = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        let last = index + 1 == pieces.len();
        let control = if index == 0 {
            format!("a=T,f=100,i={id},q=2,m={}", u8::from(!last))
        } else {
            format!("m={}", u8::from(!last))
        };
        bytes.extend_from_slice(format!("\x1b_G{control};").as_bytes());
        bytes.extend_from_slice(piece);
        bytes.extend_from_slice(b"\x1b\\");
    }
    bytes
}

/// **Most pictures arrive as a PNG**, which is what an image viewer sends when the file is one. The core
/// leaves decoding to whoever embeds it, so a PNG is a picture only because this decodes it — to the same
/// red, green, blue and alpha a view is sent for any other.
#[test]
fn a_png_is_decoded_into_the_pixels_a_view_is_sent() {
    let (mut screen, _sent) = viewed(40, 10);
    let pixels = picture(40, 60);
    screen.feed(&shown_png(5, &png_of(40, 60, png::ColorType::Rgba, png::BitDepth::Eight, &pixels)));

    let (_, _, placed) = screen.images().expect("the images read");
    assert_eq!(
        placed.iter().map(|(_, placement)| (placement.image, placement.size)).collect::<Vec<_>>(),
        [(5, (40, 60))]
    );
    let read = screen.image(5, 0).expect("the image is there");
    assert_eq!((read.width, read.height, read.channels), (40, 60, 4));
    assert!(read.bytes == pixels, "the pixels are not the picture that was sent");
}

/// Whatever a PNG holds — no alpha, grey, sixteen bits a sample, a palette — comes out as four bytes a
/// pixel, which is the one layout the core takes from a decoder.
#[test]
fn a_png_of_any_kind_becomes_four_bytes_a_pixel() {
    use apack_terminal::vt::images::rgba_of_png;
    use png::{
        BitDepth::{Eight, Sixteen},
        ColorType,
    };

    let rgb = png_of(2, 1, ColorType::Rgb, Eight, &[10, 20, 30, 40, 50, 60]);
    assert_eq!(rgba_of_png(&rgb), Some((2, 1, vec![10, 20, 30, 255, 40, 50, 60, 255])));

    let grey = png_of(2, 1, ColorType::Grayscale, Eight, &[7, 200]);
    assert_eq!(rgba_of_png(&grey), Some((2, 1, vec![7, 7, 7, 255, 200, 200, 200, 255])));

    let grey_alpha = png_of(1, 2, ColorType::GrayscaleAlpha, Eight, &[9, 100, 250, 0]);
    assert_eq!(rgba_of_png(&grey_alpha), Some((1, 2, vec![9, 9, 9, 100, 250, 250, 250, 0])));

    // Sixteen bits a sample, most significant byte first: the high byte is the one kept
    let deep = png_of(1, 1, ColorType::Rgba, Sixteen, &[0xAB, 0x01, 0xCD, 0x02, 0xEF, 0x03, 0x80, 0x04]);
    assert_eq!(rgba_of_png(&deep), Some((1, 1, vec![0xAB, 0xCD, 0xEF, 0x80])));

    let mut indexed = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut indexed, 2, 1);
        encoder.set_color(ColorType::Indexed);
        encoder.set_depth(Eight);
        encoder.set_palette(vec![1, 2, 3, 4, 5, 6]);
        encoder.set_trns(vec![255, 128]);
        let mut writer = encoder.write_header().expect("a header is written");
        writer.write_image_data(&[1, 0]).expect("the pixels are written");
        writer.finish().expect("the file is finished");
    }
    assert_eq!(rgba_of_png(&indexed), Some((2, 1, vec![4, 5, 6, 128, 1, 2, 3, 255])));
}

/// Bytes that are not a PNG, a PNG cut short, and one whose header claims more pixels than a session may
/// hold are each no picture — and the terminal carries on, taking the next one.
#[test]
fn what_is_not_a_whole_png_of_a_size_a_session_may_hold_is_no_picture() {
    use apack_terminal::vt::images::rgba_of_png;

    let whole = png_of(40, 60, png::ColorType::Rgba, png::BitDepth::Eight, &picture(40, 60));
    assert_eq!(rgba_of_png(b"not a picture at all"), None);
    assert_eq!(rgba_of_png(&whole[..whole.len() / 2]), None);
    // 4,097 by 4,096 is one row past the sixty-four megabytes a session's images may hold; one row
    // fewer is exactly at it, and is a picture
    let large = |width: u32| {
        png_of(width, 4_096, png::ColorType::Grayscale, png::BitDepth::Eight, &vec![0u8; (width * 4_096) as usize])
    };
    assert_eq!(rgba_of_png(&large(4_097)), None);
    assert!(rgba_of_png(&large(4_096)).is_some());

    let (mut screen, _sent) = viewed(40, 10);
    screen.feed(&shown_png(5, b"not a picture at all"));
    screen.feed(&shown_png(6, &whole[..whole.len() / 2]));
    assert!(screen.images().expect("the images read").2.is_empty());
    screen.feed(&shown_png(7, &whole));
    assert_eq!(screen.images().expect("the images read").2.len(), 1);
    assert!(screen.image(7, 0).is_some());
}

/// A picture sent as three bytes a pixel — red, green and blue, with no alpha — is held as it was sent,
/// and a view is told it is three bytes a pixel.
#[test]
fn a_picture_with_no_alpha_is_three_bytes_a_pixel() {
    let (mut screen, _sent) = viewed(80, 24);
    let rgb: Vec<u8> = (0..6u8 * 4 * 3).collect();
    screen.feed(format!("\x1b_Ga=T,f=24,i=8,s=6,v=4,q=2;{}\x1b\\", base64(&rgb)).as_bytes());
    let read = screen.image(8, 0).expect("the image is there");
    assert_eq!((read.width, read.height, read.channels, read.total), (6, 4, 3, 72));
    assert_eq!(read.bytes, rgb);
}

/// A view keeps an image's pixels until it is told they changed, and what tells it is the image's
/// generation: sent again under the same number with other pixels, an image is a different generation.
#[test]
fn an_image_sent_again_is_another_generation() {
    let (mut screen, _sent) = viewed(80, 24);
    screen.feed(&shown(3, 4, 4, &picture(4, 4), ""));
    let (_, _, placed) = screen.images().expect("the images read");
    let first = (placed[0].1.generation, screen.image(3, 0).expect("the image is there").generation);
    assert_eq!(first.0, first.1, "an image and where it is placed are of one generation");

    let other: Vec<u8> = picture(4, 4).into_iter().rev().collect();
    screen.feed(&shown(3, 4, 4, &other, ""));
    let (_, _, placed) = screen.images().expect("the images read");
    let second = (placed[0].1.generation, screen.image(3, 0).expect("the image is there").generation);
    assert_eq!(second.0, second.1);
    assert_ne!(second, first);
}
