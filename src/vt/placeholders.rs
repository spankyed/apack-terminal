//! Images shown through placeholder cells (the Kitty graphics protocol's Unicode placeholders): which
//! image a run of cells shows, and which part of it.
//!
//! A program that wants an image to move with its text does not place it at a cell. It makes a *virtual*
//! placement, which has no position, and prints the character `PLACEHOLDER` where the image is to be.
//! Each such cell says which image it shows in its foreground colour, which placement in its underline
//! colour, and which cell of the image it is in the combining characters after it: a row, a column, and
//! the top byte of the image's number. A cell may leave out its row and column and carry on from the cell
//! to its left.
//!
//! **Read where the row is read, for a row anywhere.** The core's render state works the same thing out,
//! and only for the screen; a view holds lines the screen has left and is sent the same row when it asks
//! again (`rows`). So this is the one answer, for a row on the screen and for one far up the history, and
//! `tests/placeholders.rs` holds it to the core's for every row the core answers for.
//!
//! **Two things here are the core's rules rather than the protocol's**, kept so the two agree: a
//! combining character that is not one of the protocol's is read as absent, and a picture is fitted into
//! its cells with the arithmetic `fit` repeats step for step, since a different order of the same
//! operations rounds to a different pixel.

use crate::view::wire::{Color, Style};

/// The character a program prints where an image is to be shown.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// What one placeholder cell says of itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Said {
    /// The low three bytes of the image's number, which is the foreground colour
    image_low: u32,
    /// Its top byte, which is the third combining character
    image_high: Option<u8>,
    /// The placement's number, which is the underline colour; absent is whichever the image has
    placement: Option<u32>,
    /// Which cell of the image this is, counted from zero
    row: Option<u32>,
    column: Option<u32>,
}

impl Said {
    /// Reads a placeholder cell: how it is drawn, and the characters that follow the placeholder in it.
    pub fn of(style: &Style, marks: impl IntoIterator<Item = char>) -> Self {
        let mut marks = marks.into_iter().map(index_of);
        let placement = number_of(style.underline);
        Self {
            image_low: number_of(style.fg),
            placement: (placement != 0).then_some(placement),
            row: marks.next().flatten(),
            column: marks.next().flatten(),
            image_high: marks.next().flatten().and_then(|high| u8::try_from(high).ok()),
        }
    }
}

/// Cells side by side that show one stretch of one row of one image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// The first cell of the run on its row, and how many cells it is
    pub column: u16,
    pub width: u16,
    image_low: u32,
    image_high: Option<u8>,
    placement: Option<u32>,
    /// The cell of the image the run begins at
    pub image_row: u32,
    pub image_column: u32,
}

impl Run {
    /// A run that begins with the cell at `column`. A cell that leaves out its row or column carries on
    /// from the cell to its left, and at the start of a run there is none: what is left out is zero.
    pub fn begin(column: u16, said: Said) -> Self {
        Self {
            column,
            width: 1,
            image_low: said.image_low,
            image_high: said.image_high,
            placement: said.placement,
            image_row: said.row.unwrap_or(0),
            image_column: said.column.unwrap_or(0),
        }
    }

    /// Adds the cell at `column` to the run, when it is the next cell and shows the next cell of the
    /// same row of the same image. `false` leaves the run as it was, and the cell begins another.
    pub fn takes(&mut self, column: u16, said: Said) -> bool {
        let next = u32::from(self.column) + u32::from(self.width);
        let follows = u32::from(column) == next
            && said.image_low == self.image_low
            && said.placement == self.placement
            && said.row.is_none_or(|row| row == self.image_row)
            && said.column.is_none_or(|at| at == self.image_column + u32::from(self.width))
            && said.image_high.is_none_or(|high| Some(high) == self.image_high);
        if follows {
            self.width += 1;
        }
        follows
    }

    /// The image the run shows.
    pub fn image(&self) -> u32 {
        self.image_low + (u32::from(self.image_high.unwrap_or(0)) << 24)
    }

    /// The placement the run names, or zero for whichever virtual placement the image has.
    pub fn placement(&self) -> u32 {
        self.placement.unwrap_or(0)
    }
}

/// Where a run's part of its picture is drawn, and which part of the picture that is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    /// How far into the run's first cell the picture starts, and how large it is drawn, in pixels
    pub offset: (u32, u32),
    pub size: (u32, u32),
    /// Left, top, width and height of the part drawn, in the image's own pixels
    pub source: (u32, u32, u32, u32),
}

/// Fits a run's part of a picture into its cells: the picture is scaled to fit the cells its placement
/// covers, keeping its shape and centred, and the run shows the slice of that its cells are.
///
/// `grid` is the columns and rows the placement was given, zero for one left to the picture's own size;
/// `picture` its width and height; `cell` one cell in pixels. `None` when there is nothing to draw: no
/// cell size, a grid past what a terminal holds, or a run that lies wholly in the margin beside a
/// picture narrower than its cells.
pub fn fit(run: &Run, grid: (u32, u32), picture: (u32, u32), cell: (u32, u32)) -> Option<Fit> {
    let (cell_width, cell_height) = cell;
    if cell_width.min(cell_height) == 0 {
        return None;
    }
    // A placement left to the picture's size takes as many cells as the picture needs
    let columns = if grid.0 == 0 { picture.0.checked_add(cell_width - 1)? / cell_width } else { grid.0 };
    let rows = if grid.1 == 0 { picture.1.checked_add(cell_height - 1)? / cell_height } else { grid.1 };
    let (columns, rows) = (u32::from(u16::try_from(columns).ok()?), u32::from(u16::try_from(rows).ok()?));

    let (picture_width, picture_height) = (f64::from(picture.0), f64::from(picture.1));
    let grid_width = f64::from(columns * cell_width);
    let grid_height = f64::from(rows * cell_height);

    // The scale that fits the picture in the grid, and the margin left on the side it does not fill
    let (scale, margin_x, margin_y) = if picture_width * grid_height > picture_height * grid_width {
        let scale = grid_width / picture_width.max(1.0);
        (scale, 0.0, (grid_height - picture_height * scale) / 2.0)
    } else {
        let scale = grid_height / picture_height.max(1.0);
        (scale, (grid_width - picture_width * scale) / 2.0, 0.0)
    };
    // The grid measured in the picture's pixels: the picture with its margins either side
    let (padding_x, padding_y) = (margin_x / scale, margin_y / scale);
    let padded_width = picture_width + padding_x * 2.0;
    let padded_height = picture_height + padding_y * 2.0;

    // The run's slice of that
    let mut source_width = padded_width * (f64::from(run.width) / f64::from(columns));
    let mut source_height = padded_height * (1.0 / f64::from(rows));
    let mut source_x = padded_width * (f64::from(run.image_column) / f64::from(columns));
    let mut source_y = padded_height * (f64::from(run.image_row) / f64::from(rows));

    let (mut offset_x, mut offset_y) = (0.0, 0.0);
    let mut width = f64::from(u32::from(run.width) * cell_width);
    let mut height = f64::from(cell_height);

    // The margins hold no pixels, so a slice that begins in one begins where the picture does, and one
    // that ends in one ends where the picture does
    if source_y < padding_y {
        let over = padding_y - source_y;
        source_height -= over;
        offset_y = over;
        height -= over * scale;
        source_y = 0.0;
        if source_height > picture_height {
            source_height = picture_height;
            height = picture_height * scale;
        }
    } else if source_y + source_height > padded_height - padding_y {
        source_y -= padding_y;
        source_height = padded_height - padding_y - source_y;
        source_height -= padding_y;
        height = source_height * scale;
    } else {
        source_y -= padding_y;
    }
    if source_x < padding_x {
        let over = padding_x - source_x;
        source_width -= over;
        offset_x = over;
        width -= over * scale;
        source_x = 0.0;
        if source_width > picture_width {
            source_width = picture_width;
            width = picture_width * scale;
        }
    } else if source_x + source_width > padded_width - padding_x {
        source_x -= padding_x;
        source_width = padded_width - padding_x - source_x;
        source_width -= padding_x;
        width = source_width * scale;
    } else {
        source_x -= padding_x;
    }
    // A slice that lies wholly in a margin has nothing of the picture in it: its width or its height
    // came out at nothing or less, which is a size of nothing once it is whole pixels
    let whole = |value: f64| value.round() as u32;
    let size = (whole(width), whole(height));
    if size.0.min(size.1) == 0 {
        return None;
    }
    Some(Fit {
        offset: (whole(offset_x * scale), whole(offset_y * scale)),
        size,
        source: (whole(source_x), whole(source_y), whole(source_width), whole(source_height)),
    })
}

/// A colour as the number a placeholder cell spells with it: a palette entry is its index, and a colour
/// given outright its three bytes.
fn number_of(color: Color) -> u32 {
    match color {
        Color::Default => 0,
        Color::Palette(index) => u32::from(index),
        Color::Rgb(red, green, blue) => u32::from_be_bytes([0, red, green, blue]),
    }
}

/// The number a combining character stands for, or `None` for one that is not among the protocol's.
fn index_of(mark: char) -> Option<u32> {
    DIACRITICS.binary_search(&u32::from(mark)).ok().and_then(|index| u32::try_from(index).ok())
}

/// The combining characters that number a row or a column, in order: the first is zero. From the
/// protocol's `rowcolumn-diacritics.txt`.
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C, 0x0350, 0x0351,
    0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369, 0x036A, 0x036B, 0x036C, 0x036D,
    0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592, 0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599,
    0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1, 0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611,
    0x0612, 0x0613, 0x0614, 0x0615, 0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6,
    0x06D7, 0x06D8, 0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743, 0x0745, 0x0747,
    0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3, 0x0816, 0x0817, 0x0818, 0x0819,
    0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822, 0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A,
    0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954, 0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD,
    0x193A, 0x1A17, 0x1A75, 0x1A76, 0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F,
    0x1B70, 0x1B71, 0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5, 0x1DD6, 0x1DD7,
    0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1, 0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5,
    0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7, 0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0,
    0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2, 0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA,
    0x2DEB, 0x2DEC, 0x2DED, 0x2DEE, 0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8,
    0x2DF9, 0x2DFA, 0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED, 0xA8EE, 0xA8EF,
    0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1, 0xFE20, 0xFE21, 0xFE22, 0xFE23,
    0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186, 0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC,
    0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];
