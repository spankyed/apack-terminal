//! Images a program places on the screen: where each is told to be, that the line stays true as the screen
//! scrolls, the pixels a view is given, and that a session not asked to take images takes none.

use apack_terminal::screen::Screen;
use apack_terminal::view::wire::{ANCHORED_LISTED, ToView, body};
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
    screen.allow_images(false).expect("images are allowed");
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

/// The cells that show image `id`'s row `row`, as a program writes them: the colour that spells the
/// image's number, then a placeholder for each column with its row and column after it.
fn cells(id: u8, row: usize, columns: usize) -> String {
    const MARKS: [char; 4] = ['\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}'];
    let cells: String = (0..columns).map(|column| format!("\u{10EEEE}{}{}", MARKS[row], MARKS[column])).collect();
    format!("\x1b[38;5;{id}m{cells}\x1b[39m")
}

/// The row a frame carried for `line`, if it carried one.
fn row_of(frame: &apack_terminal::view::wire::Frame, line: u64) -> Option<&apack_terminal::view::wire::Row> {
    frame.block.rows.iter().find(|row| row.line == line)
}

/// **A picture shown through placeholder cells is part of the rows that hold them.** It has no position
/// of its own: it is wherever the program writes those cells, so a view is sent what they show with the
/// row, in the frame that brings the row, and it is not among the placements a view asks after. The same
/// picture put where the cursor is, is.
#[test]
fn a_picture_shown_through_placeholder_cells_is_sent_with_the_rows_that_hold_them() {
    let view = apack_terminal::view::publish::ViewId { connection: 1, channel: 1 };
    let (mut screen, sent) = viewed(40, 10);
    let attached = last_frame(&sent);
    // Forty pixels by forty in a grid of four cells by two, at a cell of ten by twenty
    screen.feed(format!("\x1b_Ga=T,U=1,f=32,i=7,s=40,v=40,c=4,r=2,q=2;{}\x1b\\", base64(&picture(40, 40))).as_bytes());
    screen.feed(format!("one\r\n{}\r\n{}\r\n", cells(7, 0, 4), cells(7, 1, 4)).as_bytes());
    screen.acknowledge(view, attached.sequence).expect("an acknowledgement is taken");
    let shown = last_frame(&sent);

    assert!(screen.images().expect("the images read").2.is_empty(), "it is told as placed as well");
    for (index, line) in [shown.first_active + 1, shown.first_active + 2].into_iter().enumerate() {
        let row = row_of(&shown, line).expect("the frame carries the row");
        assert_eq!(row.pictured.as_ref().map(|pictured| pictured.cell), Some((10, 20)));
        assert_eq!(row.pieces().len(), 1);
        let piece = row.pieces()[0];
        assert_eq!((piece.image, piece.column, piece.columns), (7, 0, 4));
        assert_eq!((piece.width, piece.height), (40, 20));
        assert_eq!(
            (piece.source_x, piece.source_y, piece.source_width, piece.source_height),
            (0, index as u32 * 20, 40, 20)
        );
        // What a view draws of the cells themselves is nothing
        assert!(row.glyphs.iter().all(|glyph| glyph.text == " "), "a placeholder is drawn as a character");
    }
    assert!(row_of(&shown, shown.first_active).is_some_and(|row| row.pictured.is_none()));

    screen.feed(b"\x1b_Ga=p,i=7,p=2,q=2\x1b\\");
    let (_, _, placed) = screen.images().expect("the images read");
    assert_eq!(placed.iter().map(|(_, placement)| placement.image).collect::<Vec<_>>(), [7]);
}

/// A row is sent again when what it shows changes with no cell of it written: the image sent again,
/// which is other pixels; a window measuring its cells anew, which is another size; and the image
/// deleted, which is nothing to show.
#[test]
fn a_row_is_sent_again_when_the_picture_it_shows_changes() {
    let view = apack_terminal::view::publish::ViewId { connection: 1, channel: 1 };
    let (mut screen, sent) = viewed(40, 10);
    let attached = last_frame(&sent);
    let image = format!("\x1b_Ga=T,U=1,f=32,i=7,s=20,v=20,c=2,r=1,q=2;{}\x1b\\", base64(&picture(20, 20)));
    screen.feed(image.as_bytes());
    screen.feed(cells(7, 0, 2).as_bytes());
    screen.acknowledge(view, attached.sequence).expect("an acknowledgement is taken");
    let first = last_frame(&sent);
    let line = first.first_active;
    let piece = row_of(&first, line).expect("the row").pieces()[0];
    assert_eq!((piece.width, piece.height), (20, 20));

    // Sent again: the same cells, and other pixels
    screen.feed(image.as_bytes());
    screen.acknowledge(view, first.sequence).expect("an acknowledgement is taken");
    let again = last_frame(&sent);
    let resent = row_of(&again, line).expect("a row whose picture was sent again is not sent again").pieces()[0];
    assert_ne!(resent.generation, piece.generation);

    // A cell half as wide: the same grid, and half the size. A view is told the images changed, since
    // where each is drawn did, though no image was touched
    screen.resize(40, 10, 5, 20);
    screen.acknowledge(view, again.sequence).expect("an acknowledgement is taken");
    let measured = last_frame(&sent);
    assert_ne!(measured.images, again.images, "a cell of another size is not a change a view is told of");
    let row = row_of(&measured, line).expect("a row whose cells changed size is not sent again");
    let pictured = row.pictured.as_ref().expect("a row of placeholder cells says what they show");
    assert_eq!((pictured.cell, pictured.pieces[0].width, pictured.pieces[0].height), ((5, 20), 10, 10));

    screen.feed(b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
    screen.acknowledge(view, measured.sequence).expect("an acknowledgement is taken");
    let deleted = last_frame(&sent);
    let emptied = row_of(&deleted, line).expect("a row whose picture was deleted is not sent again");
    assert!(emptied.pieces().is_empty(), "a row still shows a picture that was deleted");
    // And is still said to hold the cells, which is what a view asks after it by when the images change
    assert!(emptied.pictured.is_some(), "a row whose cells show nothing is sent as a row with none");
}

/// **A row of placeholder cells says it holds them while they show nothing.** Cells written before their
/// image has arrived show nothing yet, and a view that keeps the row has to know to ask after it when the
/// images change: so the row is sent as one that holds such cells, and sent again with what they show
/// once there is something.
#[test]
fn a_row_whose_cells_show_nothing_yet_says_it_holds_them() {
    let view = apack_terminal::view::publish::ViewId { connection: 1, channel: 1 };
    let (mut screen, sent) = viewed(40, 10);
    let attached = last_frame(&sent);
    screen.feed(format!("text\r\n{}", cells(7, 0, 2)).as_bytes());
    screen.acknowledge(view, attached.sequence).expect("an acknowledgement is taken");
    let before = last_frame(&sent);
    let line = before.first_active + 1;
    let waiting = row_of(&before, line).expect("the row");
    assert!(waiting.pieces().is_empty());
    assert_eq!(waiting.pictured.as_ref().map(|pictured| pictured.cell), Some((10, 20)));
    assert!(
        row_of(&before, before.first_active).is_some_and(|row| row.pictured.is_none()),
        "a row of text says so too"
    );

    screen.feed(format!("\x1b_Ga=T,U=1,f=32,i=7,s=20,v=20,c=2,r=1,q=2;{}\x1b\\", base64(&picture(20, 20))).as_bytes());
    screen.acknowledge(view, before.sequence).expect("an acknowledgement is taken");
    let after = last_frame(&sent);
    assert_eq!(row_of(&after, line).expect("the row is sent again once its image arrives").pieces().len(), 1);
}

/// What a view is sent after `bytes`, having drawn `last`.
fn after(
    screen: &mut Screen,
    sent: &std::sync::mpsc::Receiver<Vec<u8>>,
    last: &apack_terminal::view::wire::Frame,
    bytes: &[u8],
) -> apack_terminal::view::wire::Frame {
    screen.feed(bytes);
    screen
        .acknowledge(apack_terminal::view::publish::ViewId { connection: 1, channel: 1 }, last.sequence)
        .expect("an acknowledgement is taken");
    last_frame(sent)
}

/// Image 6 at `row` and `column` (counted from one, as a program counts), two cells by one, and image 7
/// sent and shown nowhere: a parent and something to place beside it.
fn parent_and_picture(row: u16, column: u16) -> Vec<u8> {
    let pixels = base64(&[0x80; 20 * 20 * 3]);
    format!(
        "\x1b[{row};{column}H\x1b_Ga=T,f=24,i=6,p=3,s=20,v=20,c=2,r=1,C=1,q=2;{pixels}\x1b\\\x1b_Ga=t,f=24,i=7,s=20,v=20,q=2;{pixels}\x1b\\"
    )
    .into_bytes()
}

/// Where a frame says each image placed relative to another is: its image, depth, row and column.
fn beside(frame: &apack_terminal::view::wire::Frame) -> Option<Vec<(u32, i32, i32, i32)>> {
    frame.anchored.as_ref().map(|told| told.list.iter().map(|one| (one.image, one.z, one.row, one.column)).collect())
}

/// **An image placed relative to another is told in the frame, where the core says it is on the screen.**
/// It has no cell of its own: it is so many cells from its parent, and the storage a view's placements
/// are read from cannot say where that is. So it is not among those placements, and a frame carries all
/// of them, with the size of a cell they were measured at.
#[test]
fn an_image_placed_relative_to_another_is_told_in_the_frame_that_shows_it() {
    let (mut screen, sent) = viewed(40, 10);
    let first = last_frame(&sent);
    assert_eq!(beside(&first), Some(vec![]), "a view's first frame says there are none");

    let placed = after(&mut screen, &sent, &first, &parent_and_picture(3, 6));
    assert_eq!(beside(&placed), None, "nothing is placed relative to another yet");

    // Two columns right of its parent and one row down, over the text
    let shown = after(&mut screen, &sent, &placed, b"\x1b_Ga=p,i=7,p=9,P=6,Q=3,H=2,V=1,c=2,r=1,z=4,q=2\x1b\\");
    let told = shown.anchored.as_ref().expect("the frame tells it");
    assert_eq!((told.cell, told.omitted), ((10, 20), 0));
    let [one] = told.list.as_slice() else { panic!("one image is placed relative to another, not {told:?}") };
    assert_eq!((one.image, one.z, one.row, one.column, one.columns, one.rows), (7, 4, 3, 7, 2, 1));
    assert_eq!((one.offset_x, one.offset_y, one.width, one.height), (0, 0, 20, 20));
    assert_eq!((one.source_x, one.source_y, one.source_width, one.source_height), (0, 0, 20, 20));
    let (_, _, placements) = screen.images().expect("the images read");
    assert_eq!(placements.iter().map(|(_, placement)| placement.image).collect::<Vec<_>>(), [6]);

    // The image sent again is other pixels, which a view has to ask for
    let pixels = base64(&[0x40; 20 * 20 * 3]);
    let again =
        after(&mut screen, &sent, &shown, format!("\x1b_Ga=t,f=24,i=7,s=20,v=20,q=2;{pixels}\x1b\\").as_bytes());
    // Sending an image again removes what showed it, as the protocol has it
    assert_eq!(beside(&again), Some(vec![]));
    let back = after(&mut screen, &sent, &again, b"\x1b_Ga=p,i=7,p=9,P=6,Q=3,H=2,V=1,c=2,r=1,z=4,q=2\x1b\\");
    let told = back.anchored.as_ref().expect("the frame tells it");
    assert_eq!(beside(&back), Some(vec![(7, 4, 3, 7)]));
    assert_ne!(told.list[0].generation, one.generation, "an image sent again is another generation");

    // A view that attaches now is told in its first frame
    let (sink, second) = std::sync::mpsc::channel();
    screen
        .attach_view(apack_terminal::view::publish::ViewId { connection: 2, channel: 1 }, sink)
        .expect("a view attaches");
    assert_eq!(beside(&last_frame(&second)), Some(vec![(7, 4, 3, 7)]));
}

/// **It stays on its line as the screen scrolls, and is not told again for that.** A view holds it by
/// line, as it holds rows, so a frame that only moved the screen up says nothing of it. It is told again
/// when it is somewhere else or gone.
#[test]
fn an_image_placed_relative_to_another_is_told_again_only_when_it_moved() {
    let (mut screen, sent) = viewed(40, 10);
    let first = last_frame(&sent);
    let mut bytes = parent_and_picture(3, 6);
    bytes.extend_from_slice(b"\x1b_Ga=p,i=7,p=9,P=6,Q=3,H=2,V=1,c=2,r=1,q=2\x1b\\");
    let shown = after(&mut screen, &sent, &first, &bytes);
    assert_eq!(beside(&shown), Some(vec![(7, 0, 3, 7)]));

    // Two lines leave the screen: the image is two rows higher, on the line it was on
    let scrolled = after(&mut screen, &sent, &shown, b"\x1b[10;1Hnine\r\nten\r\neleven");
    assert_eq!(scrolled.first_active, shown.first_active + 2);
    assert_eq!(beside(&scrolled), None, "an image that stayed on its line is told again");

    // Another beside it: both are told, each by its row from the first line the screen now has
    let joined = after(&mut screen, &sent, &scrolled, b"\x1b_Ga=p,i=7,p=11,P=6,Q=3,H=4,V=1,c=2,r=1,q=2\x1b\\");
    assert_eq!(joined.first_active, scrolled.first_active);
    assert_eq!(beside(&joined), Some(vec![(7, 0, 1, 7), (7, 0, 1, 9)]));

    // Its parent gone, so are they
    let gone = after(&mut screen, &sent, &joined, b"\x1b_Ga=d,d=I,i=6,q=2\x1b\\");
    assert_eq!(beside(&gone), Some(vec![]));
}

/// **Beside an image shown through cells, it is wherever those cells are**, and moves in the frame that
/// brings the rows they were written on: a program that redraws its screen moves both at once.
#[test]
fn an_image_placed_relative_to_one_shown_through_cells_moves_with_the_cells() {
    let (mut screen, sent) = viewed(40, 10);
    let first = last_frame(&sent);
    let pixels = base64(&[0x80; 20 * 20 * 3]);
    let mut bytes = format!(
        "\x1b_Ga=T,U=1,f=24,i=8,s=20,v=20,c=2,r=1,q=2;{pixels}\x1b\\\x1b_Ga=t,f=24,i=7,s=20,v=20,q=2;{pixels}\x1b\\"
    )
    .into_bytes();
    // One column left of its parent, under the text
    bytes.extend_from_slice(b"\x1b_Ga=p,i=7,p=10,P=8,H=-1,c=1,r=1,z=-3,q=2\x1b\\");
    bytes.extend_from_slice(format!("\x1b[6;11H{}", cells(8, 0, 2)).as_bytes());
    let shown = after(&mut screen, &sent, &first, &bytes);
    assert_eq!(beside(&shown), Some(vec![(7, -3, 5, 9)]));

    let redrawn = format!("\x1b[6;1H\x1b[2K\x1b[8;21H{}", cells(8, 0, 2));
    let moved = after(&mut screen, &sent, &shown, redrawn.as_bytes());
    assert_eq!(beside(&moved), Some(vec![(7, -3, 7, 19)]));
    assert!(
        row_of(&moved, moved.first_active + 7).is_some_and(|row| row.pieces().len() == 1),
        "the frame brings the row its parent is now on"
    );

    // And with no cells showing its parent it is nowhere
    let erased = after(&mut screen, &sent, &moved, b"\x1b[8;1H\x1b[2K");
    assert_eq!(beside(&erased), Some(vec![]));
}

/// A frame lists no more than `ANCHORED_LISTED` of them, the lowest first, and says how many it left out.
#[test]
fn a_frame_lists_so_many_images_placed_relative_to_another_and_counts_the_rest() {
    let (mut screen, sent) = viewed(40, 10);
    let first = last_frame(&sent);
    let mut bytes = parent_and_picture(1, 1);
    let places: Vec<(i32, i32)> = (0..7).flat_map(|row| (0..40).map(move |column| (row, column))).collect();
    for (index, (row, column)) in places.iter().enumerate() {
        bytes.extend_from_slice(
            format!("\x1b_Ga=p,i=7,p={},P=6,Q=3,H={column},V={row},c=1,r=1,q=2\x1b\\", index + 1).as_bytes(),
        );
    }
    let shown = after(&mut screen, &sent, &first, &bytes);
    let told = shown.anchored.as_ref().expect("the frame tells them");
    assert_eq!(told.list.len(), ANCHORED_LISTED);
    assert_eq!(told.omitted as usize, places.len() - ANCHORED_LISTED);
    let listed: Vec<(i32, i32)> = told.list.iter().map(|one| (one.row, one.column)).collect();
    assert_eq!(listed, places[..ANCHORED_LISTED]);
}

/// A file of raw pixels, and the request that names it as image `id`'s source.
fn named_file(directory: &std::path::Path, id: u32, action: &str) -> (std::path::PathBuf, Vec<u8>) {
    let path = directory.join(format!("picture-{id}.rgba"));
    std::fs::write(&path, picture(4, 4)).expect("a file is written");
    let request = format!(
        "\x1b_Ga={action},f=32,i={id},s=4,v=4,t=f;{}\x1b\\",
        base64(path.to_str().expect("a path that is text").as_bytes())
    );
    (path, request.into_bytes())
}

/// **An image named as a file is read only where the session was started to take one that way.** A
/// program that names a file is asking this machine to open a path its output chose, which is the
/// caller's to allow (`Request::Spawn`); where it is not, the program is told no, which is how one finds
/// out to send the bytes instead.
#[test]
fn an_image_named_as_a_file_is_read_only_where_the_session_takes_files() {
    let directory = tempfile::tempdir().expect("a directory");

    let mut refusing = Screen::new(40, 10);
    refusing.allow_images(false).expect("images are allowed");
    let (path, request) = named_file(directory.path(), 7, "T");
    refusing.feed(&request);
    assert!(refusing.image(7, 0).is_none(), "a session not started to read files read one");
    assert!(
        refusing.take_replies().concat().contains("\x1b_Gi=7;E"),
        "a program whose file was refused is not told so",
    );
    assert!(path.exists());

    let mut taking = Screen::new(40, 10);
    taking.allow_images(true).expect("images are allowed");
    taking.feed(&request);
    let read = taking.image(7, 0).expect("a session started to read files did not read one");
    assert_eq!((read.width, read.height, read.bytes), (4, 4, picture(4, 4)));
    assert!(taking.take_replies().concat().contains("\x1b_Gi=7;OK"));
    assert!(path.exists(), "a file a program named was removed");

    // Asked whether it can, as a program does before it sends one this way
    let (_, asked) = named_file(directory.path(), 8, "q");
    taking.feed(&asked);
    assert!(taking.take_replies().concat().contains("\x1b_Gi=8;OK"));
    refusing.feed(&asked);
    assert!(refusing.take_replies().concat().contains("\x1b_Gi=8;E"));
}

/// **A shared memory object is read once and unlinked**, as the protocol says: it is the program's way
/// of handing pixels over without writing them down the pty, and the name is the terminal's to let go.
#[cfg(unix)]
#[test]
fn an_image_named_as_shared_memory_is_read_and_unlinked() {
    let pixels = picture(4, 4);
    // Short enough for macOS, which holds a name to 31 bytes
    let name = format!("/apack-test-{}", std::process::id());
    let c_name = std::ffi::CString::new(name.clone()).expect("a name");
    // SAFETY: the name is a valid C string; the descriptor is used for this object only and closed below
    let made = unsafe {
        let descriptor = libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600);
        assert!(descriptor >= 0, "a shared memory object is made");
        assert_eq!(libc::ftruncate(descriptor, pixels.len() as libc::off_t), 0);
        let mapped = libc::mmap(std::ptr::null_mut(), pixels.len(), libc::PROT_WRITE, libc::MAP_SHARED, descriptor, 0);
        assert_ne!(mapped, libc::MAP_FAILED);
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), mapped.cast::<u8>(), pixels.len());
        libc::munmap(mapped, pixels.len());
        libc::close(descriptor);
        true
    };
    assert!(made);
    let request = format!("\x1b_Ga=T,f=32,i=9,s=4,v=4,t=s;{}\x1b\\", base64(name.as_bytes()));

    let mut refusing = Screen::new(40, 10);
    refusing.allow_images(false).expect("images are allowed");
    refusing.feed(request.as_bytes());
    assert!(refusing.image(9, 0).is_none(), "a session not started to read shared memory read some");
    // SAFETY: as above
    let still = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
    assert!(still >= 0, "a session that refused an object unlinked it");
    // SAFETY: a descriptor this test opened
    unsafe { libc::close(still) };

    let mut taking = Screen::new(40, 10);
    taking.allow_images(true).expect("images are allowed");
    taking.feed(request.as_bytes());
    let read = taking.image(9, 0).expect("a session started to read shared memory did not");
    assert_eq!(read.bytes[..pixels.len()], pixels[..]);
    // SAFETY: as above
    let gone = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
    if gone >= 0 {
        // SAFETY: left behind by a failure of what this asserts, and removed so the next run can make it
        unsafe {
            libc::close(gone);
            libc::shm_unlink(c_name.as_ptr());
        }
    }
    assert!(gone < 0, "an object that was read is still there to be read again");
}

/// The checksum a PNG's chunk ends with.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A PNG made an animation of one frame, that frame being its picture and said to be `width` by `height`.
fn animated(encoded: &[u8], width: u32, height: u32) -> Vec<u8> {
    let chunk = |kind: &[u8; 4], data: &[u8]| {
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
        chunk
    };
    // One frame, played for ever; then that frame: the first, its size, at the top left, with no delay,
    // nothing disposed of and nothing blended
    let control = [0, 0, 0, 1, 0, 0, 0, 0];
    let frame = [&0u32.to_be_bytes()[..], &width.to_be_bytes(), &height.to_be_bytes(), &[0; 14]].concat();
    // Both go before the picture's pixels, whose chunk begins four bytes ahead of its name
    let pixels = encoded.windows(4).position(|name| name == b"IDAT").expect("a PNG has pixels") - 4;
    [&encoded[..pixels], &chunk(b"acTL", &control), &chunk(b"fcTL", &frame), &encoded[pixels..]].concat()
}

/// **A frame that is not the whole picture is no picture.** An animated PNG says how large each of its
/// frames is, apart from how large the picture is, and the first is the one a terminal shows. One that
/// covers part of the picture would be drawn as all of it, the rest whatever the buffer held.
#[test]
fn an_animated_png_whose_first_frame_is_part_of_the_picture_is_no_picture() {
    use apack_terminal::vt::images::rgba_of_png;

    let pixels = picture(4, 2);
    let still = png_of(4, 2, png::ColorType::Rgba, png::BitDepth::Eight, &pixels);
    assert_eq!(rgba_of_png(&animated(&still, 4, 2)), Some((4, 2, pixels)), "a frame of the whole picture is it");
    assert_eq!(rgba_of_png(&animated(&still, 2, 2)), None);
    assert_eq!(rgba_of_png(&animated(&still, 4, 1)), None);

    let grey = png_of(4, 2, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight, &[9; 16]);
    assert!(rgba_of_png(&animated(&grey, 4, 2)).is_some());
    assert_eq!(rgba_of_png(&animated(&grey, 2, 2)), None);
}
