//! A view that applies every frame it is sent holds exactly what the terminal holds.
//!
//! That is the whole contract between the broker and something drawing a session, and it is the kind that
//! a list of cases cannot establish: a frame is a *difference*, so a row left out of one is wrong only for
//! the view that was behind in exactly the right way. So the streams are generated, and so is when each
//! view acknowledges — a view that draws every frame and one that falls far behind are sent different
//! frames from the same session and must end up holding the same rows.
//!
//! The model below is the smallest thing that applies frames as the protocol says to. After every frame it
//! is compared, row by row, with the terminal read directly: the screen, and every line of history the
//! model still holds.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, channel};

use apack_terminal::screen::Screen;
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::wire::{Block, Frame, Style, ToView, body};
use apack_terminal::vt::rows::{RowRead, Space};
use proptest::prelude::*;

/// What a view keeps: its rows by line, and where the screen is.
#[derive(Default)]
struct Model {
    epoch: Option<u32>,
    first_active: u64,
    screen_rows: u16,
    lines: BTreeMap<u64, Line>,
    /// The sequence of the last frame applied, which is what an acknowledgement names
    drawn: Option<u64>,
    frames: usize,
}

/// A row as something that draws it holds one: each character where it is, with its own style and link.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    wrapped: bool,
    prompt: bool,
    cells: Vec<(u16, u8, String, Style, Option<String>)>,
}

/// A row read straight out of the terminal, as a view would hold it.
fn line_of(row: &RowRead) -> Line {
    Line {
        wrapped: row.wrapped,
        prompt: row.prompt,
        cells: row
            .cells()
            .map(|cell| (cell.column, cell.width, cell.text.to_owned(), cell.style, cell.link.map(str::to_owned)))
            .collect(),
    }
}

/// The rows of a block with their styles and links looked up, which is what the terminal's own rows are.
fn rows_of(block: &Block) -> Vec<(u64, Line)> {
    block
        .rows
        .iter()
        .map(|row| {
            let cells = row
                .glyphs
                .iter()
                .map(|glyph| {
                    let link = row.link_of(glyph).map(str::to_owned);
                    (glyph.column, glyph.width, glyph.text.clone(), row.style_of(glyph), link)
                })
                .collect();
            (row.line, Line { wrapped: row.wrapped, prompt: row.prompt, cells })
        })
        .collect()
}

impl Model {
    fn apply(&mut self, frame: &Frame) {
        if self.epoch != Some(frame.epoch) {
            self.lines.clear();
            self.epoch = Some(frame.epoch);
        }
        if let Some(stale) = frame.stale_from {
            self.lines.retain(|line, _| *line < stale || *line >= frame.first_active);
        }
        let oldest = frame.first_active.saturating_sub(u64::from(frame.scrollback));
        self.lines.retain(|line, _| *line >= oldest);
        for (line, row) in rows_of(&frame.block) {
            self.lines.insert(line, row);
        }
        self.first_active = frame.first_active;
        self.screen_rows = frame.screen_rows;
        self.drawn = Some(frame.sequence);
        self.frames += 1;
    }

    /// Compares everything held with the terminal, which must not have been written to since the frame.
    fn check(&self, screen: &Screen, when: &str) -> Result<(), TestCaseError> {
        let scrollback = u64::from(screen.scrollback_rows());
        for index in 0..u64::from(self.screen_rows) {
            let line = self.first_active + index;
            prop_assert!(self.lines.contains_key(&line), "{when}: the view holds no row for screen row {index}");
        }
        for (line, held) in &self.lines {
            // The screen's first row is `first_active`, and is `scrollback` rows down the terminal
            let row = (line + scrollback).checked_sub(self.first_active);
            let truth = match row.and_then(|row| u32::try_from(row).ok()) {
                Some(row) => screen.row(Space::Screen, row).expect("a row reads").as_ref().map(line_of),
                None => None,
            };
            prop_assert_eq!(
                Some(held),
                truth.as_ref(),
                "{}: line {} (first active {}, {} of history)",
                when,
                line,
                self.first_active,
                scrollback
            );
        }
        Ok(())
    }
}

/// One view: its model, and the end of the channel the broker writes its frames to.
struct Viewer {
    id: ViewId,
    model: Model,
    incoming: Receiver<Vec<u8>>,
}

impl Viewer {
    fn attached(screen: &mut Screen, channel_number: u32) -> Self {
        let (sink, incoming) = channel();
        let id = ViewId { connection: 1, channel: channel_number };
        screen.attach_view(id, sink).expect("a view attaches");
        Self { id, model: Model::default(), incoming }
    }

    /// Applies whatever has arrived, checking after each frame.
    fn take(&mut self, screen: &Screen, when: &str) -> Result<(), TestCaseError> {
        while let Ok(encoded) = self.incoming.try_recv() {
            match ToView::decode(body(&encoded)) {
                Ok(ToView::Frame(frame)) => {
                    prop_assert_eq!(frame.channel, self.id.channel);
                    self.model.apply(&frame);
                    self.model.check(screen, when)?;
                }
                other => prop_assert!(false, "{when}: a view was sent {other:?}"),
            }
        }
        Ok(())
    }

    fn acknowledge(&mut self, screen: &mut Screen) {
        if let Some(sequence) = self.model.drawn {
            screen.acknowledge(self.id, sequence).expect("an acknowledgement is taken");
        }
    }
}

#[derive(Debug, Clone)]
enum Step {
    Feed(Vec<u8>),
    /// The view with this index says it drew its last frame
    Acknowledge(usize),
    Resize(u16, u16),
    /// A view asks for lines by number, `back` lines above the screen
    Fetch(u16, u16),
}

fn word() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => "[a-z]{1,9}",
        1 => Just("\u{4e16}\u{754c}".to_owned()),
        1 => Just("e\u{301}".to_owned()),
        1 => Just("\u{1F44D}\u{1F3FD}".to_owned()),
    ]
}

/// Bytes a program might write: mostly lines of text, and every kind of thing that moves rows about.
fn output() -> impl Strategy<Value = Vec<u8>> {
    let line = prop::collection::vec(word(), 0..14).prop_map(|words| format!("{}\r\n", words.join(" ")));
    let styled = (0u8..8, any::<bool>(), word())
        .prop_map(|(color, bold, word)| format!("\x1b[3{color}{}m{word}\x1b[0m ", if bold { ";1" } else { "" }));
    let rgb = (any::<(u8, u8, u8)>(), word())
        .prop_map(|((r, g, b), word)| format!("\x1b[48;2;{r};{g};{b}m{word}\x1b[K\x1b[0m\r\n"));
    let moved = (1u8..12, 1u8..40, word()).prop_map(|(row, column, word)| format!("\x1b[{row};{column}H{word}"));
    let region = (1u8..5, 5u8..10, 1usize..6)
        .prop_map(|(top, bottom, feeds)| format!("\x1b[{top};{bottom}r\x1b[{bottom};1H{}\x1b[r", "x\n".repeat(feeds)));
    let linked = word().prop_map(|word| format!("\x1b]8;;https://example.com/{word}\x1b\\{word}\x1b]8;;\x1b\\ "));
    let flood =
        (20usize..2600).prop_map(|lines| (0..lines).map(|line| format!("flood {line}\r\n")).collect::<String>());
    prop_oneof![
        10 => line,
        4 => styled,
        2 => rgb,
        3 => moved,
        2 => region,
        2 => linked,
        1 => flood,
        1 => Just("\x1b[2J".to_owned()),
        1 => Just("\x1b[3J".to_owned()),
        1 => Just("\x1b[H\x1bM\x1bM".to_owned()),
        1 => Just("\x1b[3L".to_owned()),
        1 => Just("\x1b[2M".to_owned()),
        1 => Just("\x1b[?1049h\x1b[2;2Halternate".to_owned()),
        1 => Just("\x1b[?1049l".to_owned()),
        1 => Just("\x1b[?2026h".to_owned()),
        1 => Just("\x1b[?2026l".to_owned()),
        1 => Just("\x1b[?1000h".to_owned()),
        1 => Just("\x1b[?25l".to_owned()),
        1 => Just("\x1b[?25h\x1b[5 q".to_owned()),
        1 => Just("\x1bc".to_owned()),
    ]
    .prop_map(String::into_bytes)
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        12 => output().prop_map(Step::Feed),
        5 => (0usize..2).prop_map(Step::Acknowledge),
        1 => (2u16..60, 2u16..16).prop_map(|(cols, rows)| Step::Resize(cols, rows)),
        2 => (0u16..400, 1u16..40).prop_map(|(back, count)| Step::Fetch(back, count)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn a_view_that_applies_its_frames_holds_what_the_terminal_holds(
        steps in prop::collection::vec(step(), 1..60),
        late in 0usize..40,
    ) {
        // A history short enough that a flood runs it out, which is what makes a row mark go missing and
        // every line number start again
        let mut screen = Screen::with_history(40, 12, 2_000);
        // One view from the start; another arriving part way through, which is sent the screen whole
        let mut viewers = vec![Viewer::attached(&mut screen, 1)];
        viewers[0].take(&screen, "on attaching")?;

        for (index, step) in steps.iter().enumerate() {
            if index == late.min(steps.len() - 1) && viewers.len() == 1 {
                viewers.push(Viewer::attached(&mut screen, 2));
                viewers[1].take(&screen, "on attaching late")?;
            }
            match step {
                Step::Feed(bytes) => {
                    screen.feed(bytes);
                    screen.publish();
                }
                Step::Acknowledge(which) => {
                    if let Some(viewer) = viewers.get_mut(*which) {
                        viewer.acknowledge(&mut screen);
                    }
                }
                Step::Resize(cols, rows) => {
                    screen.resize(*cols, *rows, 0, 0);
                    screen.publish();
                }
                Step::Fetch(back, count) => {
                    // A view names lines in the epoch its frames are in, so only one that is up to date
                    // can ask; the answer is compared with the terminal where the screen starts *now*
                    let asked_from = viewers[0].model.first_active.saturating_sub(u64::from(*back));
                    let (epoch, first_active) = screen.view_position().expect("a position reads");
                    let fetched = screen.fetch(epoch, asked_from, u32::from(*count)).expect("a fetch reads");
                    // A fetch this small is answered whole: it says so, and a line it left out is one the
                    // terminal does not hold
                    prop_assert_eq!((fetched.first, fetched.count), (asked_from, u32::from(*count)));
                    let block = fetched.rows.block().expect("the rows read back");
                    let scrollback = u64::from(screen.scrollback_rows());
                    let rows: BTreeMap<u64, Line> = rows_of(&block).into_iter().collect();
                    for line in asked_from..asked_from + u64::from(*count) {
                        let truth = (line + scrollback)
                            .checked_sub(first_active)
                            .and_then(|row| u32::try_from(row).ok())
                            .and_then(|row| screen.row(Space::Screen, row).expect("a row reads"))
                            .as_ref()
                            .map(line_of);
                        prop_assert_eq!(rows.get(&line), truth.as_ref(), "line {} of a fetch", line);
                    }
                    prop_assert_eq!(rows.len(), block.rows.len(), "a fetch answered a line twice");
                    // And asked in a numbering that is over, it is refused rather than answered about
                    // whichever lines have those numbers now
                    prop_assert!(screen.fetch(epoch.wrapping_add(1), asked_from, u32::from(*count)).is_err());
                }
            }
            for viewer in &mut viewers {
                viewer.take(&screen, &format!("after step {index}: {step:?}"))?;
            }
        }

        // However it went, a view that keeps acknowledging ends up with the screen: let go of a hold a
        // program may have left, and let each view catch up
        screen.feed(b"\x1b[?2026l");
        screen.publish();
        for viewer in &mut viewers {
            for _ in 0..3 {
                viewer.take(&screen, "catching up")?;
                viewer.acknowledge(&mut screen);
            }
            viewer.take(&screen, "at the end")?;
            prop_assert!(viewer.model.frames > 0, "a view was never sent a frame");
            viewer.model.check(&screen, "at the end")?;
        }
    }
}

/// **The cell a wide character did not fit in is the same row to a view whatever becomes of the
/// character.** A character two cells wide with one cell left on its row goes to the next, and the cell
/// it left is empty: drawn, when it has a colour, as any empty cell is. Writing over the character
/// changes nothing about how that cell is drawn and is a write to the other row, so a view that was sent
/// the row before is holding the row as it is after — which it is only if both read alike.
#[test]
fn the_cell_a_wide_character_did_not_fit_in_reads_alike_before_and_after_the_character_is_written_over() {
    let mut screen = Screen::with_history(4, 3, 100);
    // Three cells, a background, and then a character that needs two
    screen.feed("aaa\x1b[41m\u{4e16}\x1b[0m".as_bytes());
    let before = line_of(&screen.row(Space::Screen, 0).expect("a row reads").expect("the first row"));
    assert_eq!(
        before.cells.last().map(|(column, width, text, ..)| (*column, *width, text.as_str())),
        Some((3, 1, " ")),
        "the cell left empty is one the row holds, with the background it was left: {before:?}",
    );

    screen.feed(b"\x1b[2;1Hxy");
    let after = line_of(&screen.row(Space::Screen, 0).expect("a row reads").expect("the first row"));
    assert_eq!(before, after);
}
