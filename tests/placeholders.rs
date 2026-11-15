//! Images shown through placeholder cells: what a row of them is read to show.
//!
//! **Held to the core.** The core works the same thing out for the screen (`Rendering::shown`), and a row
//! is not read from there because a view is sent rows the screen has left (`vt::placeholders`). So the
//! reader here is a second answer, and what keeps it the same answer is this file: for whatever a program
//! writes, every row of the screen has to read as the core says it shows.

use apack_terminal::view::wire::Piece;
use apack_terminal::vt::Terminal;
use apack_terminal::vt::placeholders::PLACEHOLDER;
use apack_terminal::vt::rows::{Rendering, RowRead, Space};
use proptest::prelude::*;

/// The first few of the protocol's row and column marks, which is as many as any case here needs.
const MARKS: [char; 12] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}', '\u{0346}',
    '\u{034A}', '\u{034B}', '\u{034C}',
];

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

/// A picture of one colour sent with a virtual placement of `columns` by `rows` cells, as a program sends one: in
/// pieces of the size the protocol allows. A grid of zero is left to the picture's own size.
fn sent(id: u32, width: u32, height: u32, columns: u32, rows: u32, placement: Option<u32>) -> Vec<u8> {
    let encoded = base64(&vec![0x80; (width * height * 3) as usize]);
    let pieces: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    let mut bytes = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        let more = u8::from(index + 1 != pieces.len());
        let control = if index == 0 {
            let grid = format!(
                "{}{}{}",
                if columns > 0 { format!(",c={columns}") } else { String::new() },
                if rows > 0 { format!(",r={rows}") } else { String::new() },
                placement.map_or(String::new(), |placement| format!(",p={placement}")),
            );
            format!("a=T,U=1,f=24,i={id},s={width},v={height},q=2{grid},m={more}")
        } else {
            format!("m={more}")
        };
        bytes.extend_from_slice(format!("\x1b_G{control};").as_bytes());
        bytes.extend_from_slice(piece);
        bytes.extend_from_slice(b"\x1b\\");
    }
    bytes
}

/// The colour that spells an image's number: a palette entry for a small one, three bytes otherwise.
fn naming(id: u32) -> String {
    if id < 256 {
        format!("\x1b[38;5;{id}m")
    } else {
        format!("\x1b[38;2;{};{};{}m", (id >> 16) & 255, (id >> 8) & 255, id & 255)
    }
}

/// A placeholder cell that says its row and column.
fn cell(row: usize, column: usize) -> String {
    format!("{PLACEHOLDER}{}{}", MARKS[row], MARKS[column])
}

/// A terminal that takes images, with a cell of `cell` pixels.
fn terminal(cols: u16, rows: u16, cell: (u16, u16)) -> Terminal {
    let mut terminal = Terminal::new(cols, rows).expect("a terminal");
    terminal.allow_images(false).expect("images are allowed");
    terminal.resize(cols, rows, cell.0, cell.1).expect("a resize");
    terminal
}

/// What each row of the screen is read to show, beside what the core says it shows.
fn both(terminal: &Terminal) -> (Vec<Vec<Piece>>, Vec<Vec<Piece>>) {
    let rows = terminal.rows().expect("rows");
    let mut rendering = Rendering::new().expect("a render state");
    rendering.look(terminal, &mut Vec::new()).expect("a look");
    let mut said = vec![Vec::new(); usize::from(rows)];
    for shown in rendering.shown().expect("the core's images") {
        said[usize::from(shown.row)].push(shown.piece);
    }
    let mut read = RowRead::default();
    let mut found = Vec::new();
    for row in 0..rows {
        assert!(terminal.read_row_into(Space::Active, u32::from(row), &mut read).expect("a row"));
        found.push(read.pieces().to_vec());
    }
    (found, said)
}

/// A picture that fills its cells exactly is cut into the cells' rows: each row of cells shows the
/// strip of the picture that is its height, at the size of the cells.
#[test]
fn a_row_of_placeholder_cells_shows_its_strip_of_the_picture() {
    let mut terminal = terminal(40, 6, (10, 20));
    // Forty pixels by sixty is four cells by three
    terminal.feed(&sent(7, 40, 60, 4, 3, None));
    terminal.feed(b"\r\n  ");
    for row in 0..3 {
        terminal.feed(format!("\x1b[{};3H{}", row + 3, naming(7)).as_bytes());
        terminal.feed((0..4).map(|column| cell(row, column)).collect::<String>().as_bytes());
        terminal.feed(b"\x1b[39m");
    }

    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    assert!(found[0].is_empty() && found[1].is_empty() && found[5].is_empty());
    for row in 0..3u32 {
        let pieces = &found[2 + row as usize];
        assert_eq!(pieces.len(), 1, "four cells side by side are one run");
        let piece = pieces[0];
        assert_eq!((piece.image, piece.column, piece.columns), (7, 2, 4));
        assert_eq!((piece.offset_x, piece.offset_y, piece.width, piece.height), (0, 0, 40, 20));
        assert_eq!((piece.source_x, piece.source_y, piece.source_width, piece.source_height), (0, row * 20, 40, 20));
        assert_ne!(piece.generation, 0);
    }
}

/// The cells that stand for a picture are not characters: a view is sent them blank, in the colours
/// they were written in, whether or not there is a picture for them to show.
#[test]
fn a_placeholder_cell_is_read_as_a_blank_one() {
    let mut terminal = terminal(20, 3, (10, 20));
    // No such image: the cells show nothing, and are still not drawn as the character they hold
    terminal.feed(format!("a{}{}{}\x1b[39mb", naming(9), cell(0, 0), cell(0, 1)).as_bytes());
    let mut read = RowRead::default();
    assert!(terminal.read_row_into(Space::Active, 0, &mut read).expect("a row"));
    let text: Vec<(u16, &str)> = read.cells().map(|cell| (cell.column, cell.text)).collect();
    assert_eq!(text, [(0, "a"), (1, " "), (2, " "), (3, "b")]);
    assert!(read.pieces().is_empty(), "cells naming an image that is not there were read to show one");
}

/// A cell may leave out its row and column and carry on from the cell before it, which is how a program
/// that writes a row of them saves the bytes; and a cell that names another image, another row, or a
/// column that is not the next begins a run of its own.
#[test]
fn cells_that_say_less_carry_on_from_the_cell_before_them() {
    let mut terminal = terminal(40, 4, (10, 20));
    terminal.feed(&sent(3, 80, 40, 8, 2, None));
    terminal.feed(&sent(4, 20, 20, 2, 1, None));
    // The row and column once, then two that say nothing, then one that says only its row
    terminal.feed(format!("{}{}{PLACEHOLDER}{PLACEHOLDER}{PLACEHOLDER}{}", naming(3), cell(1, 2), MARKS[1]).as_bytes());
    // A column that is not the next, then another image
    terminal.feed(format!("{}{}{}{}", cell(1, 7), naming(4), cell(0, 0), cell(0, 1)).as_bytes());

    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    let runs: Vec<(u32, u16, u16, u32)> =
        found[0].iter().map(|piece| (piece.image, piece.column, piece.columns, piece.source_x)).collect();
    assert_eq!(runs, [(3, 0, 4, 20), (3, 4, 1, 70), (4, 5, 2, 0)]);
}

/// An image is shown where its cells are wherever that is: a row that has scrolled into the history
/// reads as it did on the screen, and so does one a resize has reflowed.
#[test]
fn a_row_shows_its_picture_in_the_history_as_it_did_on_the_screen() {
    let mut terminal = terminal(30, 4, (10, 20));
    terminal.feed(&sent(5, 30, 40, 3, 2, None));
    for row in 0..2 {
        terminal.feed(format!("{}{}{}{}\x1b[39m\r\n", naming(5), cell(row, 0), cell(row, 1), cell(row, 2)).as_bytes());
    }
    let (on_screen, _) = both(&terminal);
    let on_screen: Vec<Vec<Piece>> = on_screen.into_iter().filter(|pieces| !pieces.is_empty()).collect();
    assert_eq!(on_screen.len(), 2);

    for line in 0..20 {
        terminal.feed(format!("line {line}\r\n").as_bytes());
    }
    let history = |terminal: &Terminal| {
        let mut read = RowRead::default();
        let mut found = Vec::new();
        for row in 0..terminal.scrollback_rows().expect("the history") {
            assert!(terminal.read_row_into(Space::Screen, row, &mut read).expect("a row"));
            if !read.pieces().is_empty() {
                found.push(read.pieces().to_vec());
            }
        }
        found
    };
    assert_eq!(history(&terminal), on_screen, "a row that scrolled into the history shows something else there");

    // Wider, which reflows; the rows are not wrapped, so each is still a row, and shows what it did
    terminal.resize(50, 4, 10, 20).expect("a resize");
    assert_eq!(history(&terminal), on_screen, "a row that was reflowed shows something else");
}

/// What a run shows is the image as it stands: gone when the image is deleted, and another generation
/// when it is sent again.
#[test]
fn a_row_shows_the_image_as_it_now_is() {
    let mut terminal = terminal(20, 3, (10, 20));
    terminal.feed(&sent(2, 20, 20, 2, 1, None));
    terminal.feed(format!("{}{}{}\x1b[39m", naming(2), cell(0, 0), cell(0, 1)).as_bytes());
    let first = both(&terminal).0[0].clone();
    assert_eq!(first.len(), 1);

    terminal.feed(&sent(2, 20, 20, 2, 1, None));
    let again = both(&terminal).0[0].clone();
    assert_eq!(again.len(), 1);
    assert_ne!(again[0].generation, first[0].generation, "an image sent again is the same generation");

    terminal.feed(b"\x1b_Ga=d,d=I,i=2,q=2\x1b\\");
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert!(found[0].is_empty(), "cells still show an image that was deleted");
}

/// A cell names its placement in its underline colour, and shows that one of the image's; a placement
/// the image does not have shows nothing.
#[test]
fn cells_that_name_a_placement_show_that_one() {
    let mut terminal = terminal(40, 4, (10, 20));
    // The wider placement first, so the one shown where none is named is not simply the first made
    terminal.feed(&sent(6, 80, 20, 8, 1, Some(12)));
    terminal.feed(b"\x1b_Ga=p,U=1,i=6,p=11,c=4,r=1,q=2\x1b\\");
    let named = |placement: u32| format!("\x1b[58;5;{placement}m");
    terminal.feed(format!("{}{}{}{}", naming(6), named(11), cell(0, 0), cell(0, 1)).as_bytes());
    terminal.feed(format!("\x1b[2;1H{}{}{}", named(12), cell(0, 0), cell(0, 1)).as_bytes());
    terminal.feed(format!("\x1b[3;1H{}{}{}", named(13), cell(0, 0), cell(0, 1)).as_bytes());
    // And none named: whichever the image has, which with two is the lower numbered
    terminal.feed(format!("\x1b[4;1H\x1b[59m{}{}", cell(0, 0), cell(0, 1)).as_bytes());

    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    // Two cells of four show half the picture, and two of eight a quarter of it
    assert_eq!(found[0].iter().map(|piece| piece.source_width).collect::<Vec<_>>(), [40]);
    assert_eq!(found[1].iter().map(|piece| piece.source_width).collect::<Vec<_>>(), [20]);
    assert!(found[2].is_empty(), "cells naming a placement the image does not have show one");
    assert_eq!(found[3].iter().map(|piece| piece.source_width).collect::<Vec<_>>(), [40]);
}

/// Cells show an image only through a placement made to be shown through cells: one put at a cell of
/// its own is drawn there, and cells that name its image show nothing; and an image whose pixels have
/// not all arrived shows nothing until they have.
#[test]
fn cells_show_only_an_image_that_is_there_to_be_shown_through_them() {
    let mut terminal = terminal(40, 4, (10, 20));
    // Placed where the cursor is, with no placement for cells
    let placed = base64(&[0x80; 20 * 20 * 3]);
    terminal.feed(format!("\x1b_Ga=T,f=24,i=8,s=20,v=20,c=2,r=1,q=2;{placed}\x1b\\").as_bytes());
    terminal.feed(format!("\x1b[2;1H{}{}{}\x1b[39m", naming(8), cell(0, 0), cell(0, 1)).as_bytes());
    // The first part of an image sent in two, and the cells that will show it
    let (first, rest) = placed.split_at(placed.len() / 2 / 4 * 4);
    terminal.feed(format!("\x1b_Ga=T,U=1,f=24,i=9,s=20,v=20,c=2,r=1,q=2,m=1;{first}\x1b\\").as_bytes());
    terminal.feed(format!("\x1b[3;1H{}{}{}\x1b[39m", naming(9), cell(0, 0), cell(0, 1)).as_bytes());

    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    assert!(found[1].is_empty(), "cells show an image that was placed at a cell of its own");
    assert!(found[2].is_empty(), "cells show an image whose pixels have not all arrived");

    terminal.feed(format!("\x1b_Gm=0;{rest}\x1b\\").as_bytes());
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert_eq!(found[2].len(), 1, "an image whose pixels have all arrived is not shown");
}

/// An image's number is spelled in three bytes of colour — a palette entry for one under 256, a colour
/// given outright for the rest — and its fourth byte in a third mark after the row and column.
#[test]
fn an_images_number_is_its_colour_and_the_mark_after_its_column() {
    let mut terminal = terminal(40, 4, (10, 20));
    let (small, wide, widest) = (200, 0x0001_0203, 0x0500_0007);
    for id in [small, wide, widest] {
        terminal.feed(&sent(id, 20, 20, 2, 1, None));
    }
    let top = MARKS[5];
    terminal.feed(format!("{}{}{}\x1b[39m", naming(small), cell(0, 0), cell(0, 1)).as_bytes());
    terminal.feed(format!("\x1b[2;1H{}{}{}\x1b[39m", naming(wide), cell(0, 0), cell(0, 1)).as_bytes());
    // The top byte on the first cell; the second leaves it out, and carries on with the first's
    terminal.feed(format!("\x1b[3;1H{}{}{top}{}\x1b[39m", naming(widest), cell(0, 0), cell(0, 1)).as_bytes());
    // The same three bytes of colour and no top byte is another image, which is not there
    terminal.feed(format!("\x1b[4;1H{}{}{}\x1b[39m", naming(widest), cell(0, 0), cell(0, 1)).as_bytes());

    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    let shown: Vec<Vec<(u32, u16)>> =
        found.iter().map(|row| row.iter().map(|piece| (piece.image, piece.columns)).collect()).collect();
    assert_eq!(shown, [vec![(small, 2)], vec![(wide, 2)], vec![(widest, 2)], vec![]]);
}

/// A cell that says another top byte than the run before it shows another image, and begins a run.
#[test]
fn a_cell_with_another_top_byte_begins_a_run() {
    let mut terminal = terminal(40, 2, (10, 20));
    let (one, other) = (0x0100_0007, 0x0200_0007);
    terminal.feed(&sent(one, 20, 20, 2, 1, None));
    terminal.feed(&sent(other, 20, 20, 2, 1, None));
    terminal.feed(format!("{}{}{}{}{}\x1b[39m", naming(one), cell(0, 0), MARKS[1], cell(0, 1), MARKS[2]).as_bytes());
    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    let shown: Vec<(u32, u16, u16)> = found[0].iter().map(|piece| (piece.image, piece.column, piece.columns)).collect();
    assert_eq!(shown, [(one, 0, 1), (other, 1, 1)]);
}

/// What cells can name is the storage as it stands at each read: a placement made after a row was last
/// read is one its cells show at the next, and the other screen's images are not this screen's.
#[test]
fn cells_show_what_the_storage_holds_at_each_read() {
    let mut terminal = terminal(40, 4, (10, 20));
    terminal.feed(&sent(6, 80, 20, 8, 1, Some(12)));
    let named = |placement: u32| format!("\x1b[58;5;{placement}m");
    terminal.feed(format!("{}{}{}{}\x1b[59m", naming(6), named(13), cell(0, 0), cell(0, 1)).as_bytes());
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert!(found[0].is_empty(), "cells show a placement that has not been made");

    // Made now, with no cell written: the same cells show it
    terminal.feed(b"\x1b_Ga=p,U=1,i=6,p=13,c=4,r=1,q=2\x1b\\");
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert_eq!(found[0].iter().map(|piece| piece.source_width).collect::<Vec<_>>(), [40]);

    // The alternate screen has images of its own: the same cells there name nothing
    terminal
        .feed(format!("\x1b[?1049h\x1b[H{}{}{}{}\x1b[59m", naming(6), named(13), cell(0, 0), cell(0, 1)).as_bytes());
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert!(found[0].is_empty(), "cells on one screen show an image sent to the other");
    terminal.feed(b"\x1b[?1049l");
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert_eq!(found[0].len(), 1, "the screen a program came back to lost what its cells show");
}

/// What cells can name is read out of the storage once for each change to the images, however many
/// rows and runs are read meanwhile: a row of placeholder cells costs a lookup a run, and not a walk of
/// every placement a program has made.
#[test]
fn what_cells_can_name_is_read_out_once_for_each_change_to_the_images() {
    let mut terminal = terminal(40, 6, (10, 20));
    terminal.feed(b"text\r\n");
    let mut read = RowRead::default();
    let mut read_screen = |terminal: &Terminal| {
        let mut pieces = 0;
        for row in 0..terminal.rows().expect("rows") {
            assert!(terminal.read_row_into(Space::Active, u32::from(row), &mut read).expect("a row"));
            pieces += read.pieces().len();
        }
        pieces
    };
    assert_eq!(read_screen(&terminal), 0);
    assert_eq!(terminal.virtual_reads(), 0, "a screen with no placeholder cells read the storage");

    terminal.feed(&sent(6, 40, 20, 4, 1, None));
    // Every cell a run of its own: the same column named again, so none carries on from the last
    for row in 2..5 {
        terminal.feed(format!("\x1b[{row};1H{}{}", naming(6), cell(0, 0).repeat(8)).as_bytes());
    }
    assert_eq!(read_screen(&terminal), 24);
    assert_eq!(read_screen(&terminal), 24);
    assert_eq!(terminal.virtual_reads(), 1, "the storage was read again though nothing in it changed");

    // Another placement is another generation of the storage, which is read once more
    terminal.feed(b"\x1b_Ga=p,U=1,i=6,p=9,c=2,r=1,q=2\x1b\\");
    assert_eq!(read_screen(&terminal), 24);
    assert_eq!(read_screen(&terminal), 24);
    assert_eq!(terminal.virtual_reads(), 2);
}

/// Nothing is shown where no window has said how large a cell is: there is no size to draw at.
#[test]
fn nothing_is_shown_where_a_cell_has_no_size() {
    let mut terminal = Terminal::new(20, 3).expect("a terminal");
    terminal.allow_images(false).expect("images are allowed");
    terminal.feed(&sent(2, 20, 20, 2, 1, None));
    terminal.feed(format!("{}{}{}\x1b[39m", naming(2), cell(0, 0), cell(0, 1)).as_bytes());
    let (found, said) = both(&terminal);
    assert_eq!(found, said);
    assert!(found[0].is_empty());

    // Nor where it has a height and no width, or a width and no height
    for cell in [(0, 20), (10, 0)] {
        terminal.resize(20, 3, cell.0, cell.1).expect("a resize");
        let (found, said) = both(&terminal);
        assert_eq!(found, said);
        assert!(found[0].is_empty(), "a cell of {cell:?} has a size to draw at");
    }
    terminal.resize(20, 3, 10, 20).expect("a resize");
    assert_eq!(both(&terminal).0[0].len(), 1);
}

/// One screen a program might write: an image of some size shown in a grid of some size, at a cell of
/// some size, with its cells written whole, in part, or out of step.
#[derive(Debug, Clone)]
struct Shown {
    /// The image's number: one a palette entry spells, one a colour does, one with a top byte
    image: u32,
    picture: (u32, u32),
    /// Zero is a grid left to the picture's size
    grid: (u32, u32),
    cell: (u16, u16),
    /// Each row written: where it starts on the screen, and the cells of the image it holds
    rows: Vec<(u16, Vec<Written>)>,
}

/// One placeholder cell as a program writes it.
#[derive(Debug, Clone, Copy)]
struct Written {
    row: Option<usize>,
    column: Option<usize>,
}

fn shown() -> impl Strategy<Value = Shown> {
    let written = (prop::option::weighted(0.8, 0..MARKS.len()), prop::option::weighted(0.8, 0..MARKS.len()))
        .prop_map(|(row, column)| Written { row, column });
    // Mostly a row written in step, as a program writes one; sometimes cells in any order
    let in_step = (0..4usize, 0..3usize, 1..9usize).prop_map(|(row, from, length)| {
        (from..(from + length).min(MARKS.len()))
            .map(|column| Written { row: Some(row), column: Some(column) })
            .collect::<Vec<_>>()
    });
    let row = (0u16..8, prop_oneof![3 => in_step, 1 => prop::collection::vec(written, 1..8)]);
    (
        prop_oneof![Just(21u32), Just(0x00AB_CDEF), Just(0x0300_0102)],
        (1u32..200, 1u32..200),
        (0u32..10, 0u32..10),
        (1u16..24, 1u16..40),
        prop::collection::vec(row, 1..6),
    )
        .prop_map(|(image, picture, grid, cell, rows)| Shown { image, picture, grid, cell, rows })
}

/// Whatever a program writes, each row of the screen reads as the core says it shows: the same runs,
/// each the same part of the picture at the same place and size.
///
/// **And it has to have shown something.** Two readers that both find nothing agree, so the cases are
/// counted: a run that compared no picture at all is a strategy that stopped writing any, not a pass.
#[test]
fn every_row_reads_as_the_core_says_it_shows() {
    let pictured = std::cell::Cell::new(0u32);
    let cases = 400;
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig::with_cases(cases));
    runner
        .run(&shown(), |shown| {
            let mut terminal = terminal(40, 8, shown.cell);
            terminal.feed(&sent(shown.image, shown.picture.0, shown.picture.1, shown.grid.0, shown.grid.1, None));
            for (index, (start, cells)) in shown.rows.iter().enumerate() {
                terminal.feed(format!("\x1b[{};{}H{}", index + 1, start + 1, naming(shown.image)).as_bytes());
                for written in cells {
                    let mut text = String::from(PLACEHOLDER);
                    // A column cannot be said without a row before it, which is the protocol's order
                    if let Some(row) = written.row {
                        text.push(MARKS[row]);
                        if let Some(column) = written.column {
                            text.push(MARKS[column]);
                            // And the image's top byte after both, where it has one
                            if shown.image >> 24 != 0 {
                                text.push(MARKS[(shown.image >> 24) as usize]);
                            }
                        }
                    }
                    terminal.feed(text.as_bytes());
                }
                terminal.feed(b"\x1b[39m");
            }
            let (found, said) = both(&terminal);
            if said.iter().any(|row| !row.is_empty()) {
                pictured.set(pictured.get() + 1);
            }
            prop_assert_eq!(found, said);
            Ok(())
        })
        .expect("every row reads as the core says it shows");
    assert!(
        pictured.get() > cases / 2,
        "only {} of {cases} screens showed any picture: the comparison is of nothing",
        pictured.get()
    );
}

/// The core's own answer for an image placed relative to another, which is what a frame carries
/// (`view::publish`): beside a parent at a cell of its own, and beside one shown through cells, where it
/// is measured from the top-left of those cells on the screen.
#[test]
fn the_core_places_an_image_anchored_to_another_on_the_screen() {
    let mut terminal = terminal(40, 8, (10, 20));
    let pixels = base64(&[0x80; 20 * 20 * 3]);
    // A parent at row 2, column 5, and a child two columns right of it and one row down
    terminal.feed(format!("\x1b[3;6H\x1b_Ga=T,f=24,i=6,p=3,s=20,v=20,c=2,r=1,C=1,q=2;{pixels}\x1b\\").as_bytes());
    terminal.feed(format!("\x1b_Ga=t,f=24,i=7,s=20,v=20,q=2;{pixels}\x1b\\").as_bytes());
    terminal.feed(b"\x1b_Ga=p,i=7,p=9,P=6,Q=3,H=2,V=1,c=2,r=1,z=4,q=2\x1b\\");
    // A parent shown through cells at row 5, columns 10 and 11, and a child one column left of it
    terminal.feed(&sent(8, 20, 20, 2, 1, Some(4)));
    terminal.feed(b"\x1b_Ga=p,i=7,p=10,P=8,Q=4,H=-1,V=0,c=1,r=1,z=-3,q=2\x1b\\");
    terminal
        .feed(format!("\x1b[6;11H{}\x1b[58;5;4m{}{}\x1b[59m\x1b[39m", naming(8), cell(0, 0), cell(0, 1)).as_bytes());

    let mut rendering = Rendering::new().expect("a render state");
    rendering.look(&terminal, &mut Vec::new()).expect("a look");
    let anchored: Vec<(u32, i32, i32, i32, u32, u32)> = rendering
        .anchored()
        .expect("the core's anchored images")
        .iter()
        .map(|one| (one.image, one.z, one.row, one.column, one.columns, one.rows))
        .collect();
    assert_eq!(anchored, [(7, -3, 5, 9, 1, 1), (7, 4, 3, 7, 2, 1)]);
    // And neither is among what the storage can place
    assert_eq!(terminal.placements().expect("placements").iter().map(|one| one.image).collect::<Vec<_>>(), [6]);
}
