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
    let mut terminal = terminal(40, 3, (10, 20));
    terminal.feed(&sent(6, 80, 20, 4, 1, Some(11)));
    // A second placement of the same image, twice as wide
    terminal.feed(b"\x1b_Ga=p,U=1,i=6,p=12,c=8,r=1,q=2\x1b\\");
    let named = |placement: u32| format!("\x1b[58;5;{placement}m");
    terminal.feed(format!("{}{}{}{}", naming(6), named(11), cell(0, 0), cell(0, 1)).as_bytes());
    terminal.feed(format!("\x1b[2;1H{}{}{}", named(12), cell(0, 0), cell(0, 1)).as_bytes());
    terminal.feed(format!("\x1b[3;1H{}{}{}", named(13), cell(0, 0), cell(0, 1)).as_bytes());

    let (found, said) = both(&terminal);
    assert_eq!(found, said, "a row is read to show something other than the core says it does");
    // Two cells of four show half the picture, and two of eight a quarter of it
    assert_eq!(found[0].iter().map(|piece| piece.source_width).collect::<Vec<_>>(), [40]);
    assert_eq!(found[1].iter().map(|piece| piece.source_width).collect::<Vec<_>>(), [20]);
    assert!(found[2].is_empty(), "cells naming a placement the image does not have show one");
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
}

/// One screen a program might write: an image of some size shown in a grid of some size, at a cell of
/// some size, with its cells written whole, in part, or out of step.
#[derive(Debug, Clone)]
struct Shown {
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
    ((1u32..200, 1u32..200), (0u32..10, 0u32..10), (1u16..24, 1u16..40), prop::collection::vec(row, 1..6))
        .prop_map(|(picture, grid, cell, rows)| Shown { picture, grid, cell, rows })
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
            terminal.feed(&sent(21, shown.picture.0, shown.picture.1, shown.grid.0, shown.grid.1, None));
            for (index, (start, cells)) in shown.rows.iter().enumerate() {
                terminal.feed(format!("\x1b[{};{}H{}", index + 1, start + 1, naming(21)).as_bytes());
                for written in cells {
                    let mut text = String::from(PLACEHOLDER);
                    // A column cannot be said without a row before it, which is the protocol's order
                    if let Some(row) = written.row {
                        text.push(MARKS[row]);
                        if let Some(column) = written.column {
                            text.push(MARKS[column]);
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
