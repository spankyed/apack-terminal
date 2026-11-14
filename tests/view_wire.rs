//! The view protocol's bytes, read back and attacked.
//!
//! A view connection is the second place a stranger reaches (`frames` is the first), and what it reaches
//! here is hand-written decoding of lengths and counts — exactly the code that reads past a buffer or
//! allocates what a count claims. So, as for a client's frames:
//!
//! - **Every message round-trips.** What one side encodes the other decodes to the same value, for
//!   generated messages of every kind.
//! - **No bytes get anything but a message or a refusal.** Arbitrary bodies, and valid ones cut short or
//!   with one byte changed, never panic and never allocate from a count the payload could not hold.

use apack_terminal::protocol::{CommandOutcome, Unreported};
use apack_terminal::view::wire::{
    Block, Color, ColorChanges, CommandRow, Cursor, Frame, FrameHead, Glyph, Incoming, MAX_MESSAGE_BYTES, Malformed,
    Piece, Placement, Position, Row, Rows, Span, Style, ToBroker, ToView, TooLong, body, frame, read_message,
    write_row,
};
use proptest::prelude::*;

fn color() -> impl Strategy<Value = Color> {
    prop_oneof![
        Just(Color::Default),
        any::<u8>().prop_map(Color::Palette),
        any::<(u8, u8, u8)>().prop_map(|(r, g, b)| Color::Rgb(r, g, b)),
    ]
}

fn style() -> impl Strategy<Value = Style> {
    (color(), color(), color(), 0u16..2048).prop_map(|(fg, bg, underline, flags)| Style { fg, bg, underline, flags })
}

/// A character as a row holds one: a plain letter, something wide, or several code points in a cell.
fn character() -> impl Strategy<Value = (u8, String)> {
    prop_oneof![
        4 => "[a-z ]".prop_map(|text| (1u8, text)),
        1 => Just((2u8, "\u{4e16}".to_owned())),
        1 => Just((1u8, "e\u{301}".to_owned())),
        1 => Just((2u8, "\u{1F468}\u{200D}\u{1F469}".to_owned())),
    ]
}

/// A row whose glyphs are in order and do not overlap, with gaps, as a screen's are, each drawn in one
/// of the row's own styles or the plain one and linked to one of its own links or to nothing.
fn row() -> impl Strategy<Value = Row> {
    (prop::collection::vec(style(), 0..4), prop::collection::vec("[a-z:/.]{1,12}", 0..3)).prop_flat_map(
        |(styles, links): (Vec<Style>, Vec<String>)| {
            let glyph = (character(), 0u16..3, 0..=styles.len(), 0..=links.len());
            (any::<u32>(), any::<bool>(), any::<bool>(), prop::collection::vec(glyph, 0..24), pieces()).prop_map(
                move |(line, wrapped, prompt, cells, (pieces, cell))| {
                    let mut column = 0u16;
                    let glyphs = cells
                        .into_iter()
                        .map(|((width, text), gap, style, link)| {
                            column += gap;
                            let glyph = Glyph { column, width, text, style: style as u16, link: link as u16 };
                            column += u16::from(width);
                            glyph
                        })
                        .collect();
                    Row {
                        line: u64::from(line),
                        wrapped,
                        prompt,
                        styles: styles.clone(),
                        links: links.clone(),
                        glyphs,
                        pieces,
                        cell,
                    }
                },
            )
        },
    )
}

/// The parts of images a row shows and the cell they are measured in, which is said only of a row that
/// shows any.
fn pieces() -> impl Strategy<Value = (Vec<Piece>, (u16, u16))> {
    let piece = (
        (any::<u32>(), any::<u64>(), any::<u16>(), any::<u16>()),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()),
    )
        .prop_map(|((image, generation, column, columns), drawn, source)| Piece {
            image,
            generation,
            column,
            columns,
            offset_x: drawn.0,
            offset_y: drawn.1,
            width: drawn.2,
            height: drawn.3,
            source_x: source.0,
            source_y: source.1,
            source_width: source.2,
            source_height: source.3,
        });
    (prop::collection::vec(piece, 0..4), any::<(u16, u16)>())
        .prop_map(|(pieces, cell)| if pieces.is_empty() { (pieces, (0, 0)) } else { (pieces, cell) })
}

fn block() -> impl Strategy<Value = Block> {
    prop::collection::vec(row(), 0..6).prop_map(|rows| Block { rows })
}

fn position() -> impl Strategy<Value = Position> {
    (any::<u64>(), any::<u16>()).prop_map(|(line, column)| Position { line, column })
}

fn span() -> impl Strategy<Value = Span> {
    (position(), position()).prop_map(|(start, end)| Span { start, end })
}

/// How a command ended: each kind there is, with a status of either sign where one is carried.
fn outcome() -> impl Strategy<Value = CommandOutcome> {
    prop_oneof![
        Just(CommandOutcome::Running),
        any::<i32>().prop_map(|code| CommandOutcome::Exited { code }),
        prop_oneof![Just(Unreported::NoStatus), Just(Unreported::Superseded), Just(Unreported::SessionEnded)]
            .prop_map(|why| CommandOutcome::Unknown { why }),
        prop::option::of(any::<i32>()).prop_map(|code| CommandOutcome::Claimed { code }),
    ]
}

/// A command with any of its fields present or absent, ended any way one can.
fn command() -> impl Strategy<Value = CommandRow> {
    (
        any::<u64>(),
        (prop::option::of(".{0,12}"), prop::option::of("[a-z/]{0,12}")),
        (prop::option::of(any::<u64>()), prop::option::of(any::<u64>()), outcome()),
        (prop::option::of(any::<u64>()), prop::option::of(any::<u64>()), prop::option::of(any::<u64>())),
    )
        .prop_map(|(id, (command, cwd), (started_ms, finished_ms, outcome), (prompt, output, end))| CommandRow {
            id,
            command,
            cwd,
            started_ms,
            finished_ms,
            outcome,
            prompt,
            output,
            end,
        })
}

/// Colours a program changed: any of the three, and palette entries in order of index, each once.
fn colors() -> impl Strategy<Value = ColorChanges> {
    let rgb = || prop::option::of(any::<[u8; 3]>());
    (rgb(), rgb(), rgb(), prop::collection::btree_map(any::<u8>(), any::<[u8; 3]>(), 0..20)).prop_map(
        |(foreground, background, cursor, palette)| ColorChanges {
            foreground,
            background,
            cursor,
            palette: palette.into_iter().collect(),
        },
    )
}

fn to_view() -> impl Strategy<Value = ToView> {
    let cursor = prop::option::of(
        (any::<u16>(), any::<u16>(), 0u8..4, any::<bool>(), any::<bool>())
            .prop_map(|(column, row, shape, blinking, hidden)| Cursor { column, row, shape, blinking, hidden }),
    );
    let frame = (
        (any::<u32>(), any::<u64>(), any::<u32>(), any::<u64>()),
        (any::<u16>(), any::<u16>(), any::<u32>(), any::<u32>(), any::<u32>()),
        (cursor, any::<bool>(), any::<bool>(), prop::option::of(any::<u64>())),
        (any::<bool>(), prop::option::of(colors())),
        block(),
    )
        .prop_map(|(ids, grid, state, shown, block)| {
            ToView::Frame(Frame {
                channel: ids.0,
                sequence: ids.1,
                epoch: ids.2,
                first_active: ids.3,
                columns: grid.0,
                screen_rows: grid.1,
                scrollback: grid.2,
                commands: grid.3,
                images: grid.4,
                cursor: state.0,
                alternate_screen: state.1,
                mouse: state.2,
                reverse_video: shown.0,
                stale_from: state.3,
                colors: shown.1,
                block,
            })
        });
    prop_oneof![
        frame,
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u64>(), any::<u32>(), block()).prop_map(
            |(channel, request, epoch, first, count, block)| ToView::Rows {
                channel,
                request,
                epoch,
                first,
                count,
                block
            }
        ),
        any::<u32>().prop_map(|channel| ToView::Ended { channel }),
        (any::<u32>(), any::<u32>(), 0u8..2).prop_map(|(channel, request, outcome)| ToView::Pasted {
            channel,
            request,
            outcome
        }),
        (any::<u32>(), any::<u32>(), any::<u32>(), prop::option::of(span()))
            .prop_map(|(channel, request, epoch, span)| ToView::Span { channel, request, epoch, span }),
        (any::<u32>(), any::<u32>(), any::<u32>(), ".{0,40}")
            .prop_map(|(channel, request, epoch, text)| { ToView::Text { channel, request, epoch, text } }),
        (any::<u32>(), any::<u32>(), any::<u32>(), prop::collection::vec(span(), 0..5))
            .prop_map(|(channel, request, epoch, spans)| ToView::Spans { channel, request, epoch, spans }),
        (any::<u32>(), any::<u32>(), ".{0,40}").prop_map(|(channel, request, message)| ToView::Failed {
            channel,
            request,
            message
        }),
        (any::<u32>(), any::<u32>(), any::<u32>(), prop::collection::vec(command(), 0..4))
            .prop_map(|(channel, request, epoch, commands)| ToView::Commands { channel, request, epoch, commands }),
        (
            (any::<u32>(), any::<u32>(), any::<u32>()),
            (any::<u16>(), any::<u16>()),
            prop::collection::vec(placement(), 0..4)
        )
            .prop_map(|((channel, request, epoch), (cell_width, cell_height), placements)| ToView::Images {
                channel,
                request,
                epoch,
                cell_width,
                cell_height,
                placements,
            }),
        (
            (any::<u32>(), any::<u32>(), any::<u32>(), any::<u64>()),
            (any::<u32>(), any::<u32>(), 1u8..=4, any::<u32>(), any::<u32>()),
            prop::collection::vec(any::<u8>(), 0..64),
        )
            .prop_map(
                |((channel, request, image, generation), (width, height, channels, total, offset), bytes)| {
                    ToView::Image { channel, request, image, generation, width, height, channels, total, offset, bytes }
                }
            ),
    ]
}

fn placement() -> impl Strategy<Value = Placement> {
    (
        (any::<u32>(), any::<u64>(), any::<i32>(), any::<u64>(), any::<u16>()),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>()),
    )
        .prop_map(|(at, grid, source)| Placement {
            image: at.0,
            generation: at.1,
            z: at.2,
            line: at.3,
            column: at.4,
            columns: grid.0,
            rows: grid.1,
            offset_x: grid.2,
            offset_y: grid.3,
            width: grid.4,
            height: grid.5,
            source_x: source.0,
            source_y: source.1,
            source_width: source.2,
            source_height: source.3,
        })
}

fn to_broker() -> impl Strategy<Value = ToBroker> {
    prop_oneof![
        (any::<u32>(), "[a-z0-9-]{1,8}").prop_map(|(channel, session)| ToBroker::Attach { channel, session }),
        any::<u32>().prop_map(|channel| ToBroker::Detach { channel }),
        (any::<u32>(), any::<u64>()).prop_map(|(channel, sequence)| ToBroker::Acknowledge { channel, sequence }),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u64>(), any::<u32>()).prop_map(
            |(channel, request, epoch, first, count)| ToBroker::Fetch { channel, request, epoch, first, count }
        ),
        (any::<u32>(), 0u8..3, any::<u16>(), "[A-Za-z0-9]{0,12}", ".{0,4}", any::<u32>(), any::<bool>()).prop_map(
            |(channel, action, modifiers, code, text, unshifted, composed)| ToBroker::Key {
                channel,
                action,
                modifiers,
                code,
                text,
                unshifted,
                composed
            }
        ),
        (any::<u32>(), ".{0,40}").prop_map(|(channel, text)| ToBroker::Text { channel, text }),
        (any::<u32>(), 0u8..5, any::<u8>(), any::<u16>(), (0f32..4000.0, 0f32..4000.0, 1f32..40.0, 1f32..40.0))
            .prop_map(|(channel, action, button, modifiers, (x, y, cell_width, cell_height))| ToBroker::Pointer {
                channel,
                action,
                button,
                modifiers,
                x,
                y,
                cell_width,
                cell_height
            }),
        (any::<u32>(), any::<u32>(), any::<bool>(), ".{0,60}").prop_map(|(channel, request, allow_unsafe, text)| {
            ToBroker::Paste { channel, request, allow_unsafe, text }
        }),
        (any::<u32>(), any::<bool>()).prop_map(|(channel, gained)| ToBroker::Focus { channel, gained }),
        (any::<u32>(), any::<u32>(), any::<u32>(), position())
            .prop_map(|(channel, request, epoch, at)| ToBroker::WordAt { channel, request, epoch, at }),
        (any::<u32>(), any::<u32>(), any::<u32>(), position())
            .prop_map(|(channel, request, epoch, at)| ToBroker::LineAt { channel, request, epoch, at }),
        (any::<u32>(), any::<u32>(), any::<u32>(), span())
            .prop_map(|(channel, request, epoch, span)| ToBroker::TextOf { channel, request, epoch, span }),
        (any::<u32>(), any::<u32>(), ".{0,20}", any::<bool>()).prop_map(
            |(channel, request, needle, case_sensitive)| ToBroker::Search { channel, request, needle, case_sensitive }
        ),
        (any::<u32>(), any::<u32>()).prop_map(|(channel, request)| ToBroker::Commands { channel, request }),
        (any::<u32>(), any::<u32>()).prop_map(|(channel, request)| ToBroker::Images { channel, request }),
        (any::<u32>(), any::<u32>(), any::<u32>(), any::<u32>())
            .prop_map(|(channel, request, image, offset)| ToBroker::Image { channel, request, image, offset }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn what_the_broker_sends_a_view_reads(message in to_view()) {
        let encoded = message.encode().expect("a message this small is one");
        prop_assert_eq!(ToView::decode(body(&encoded)), Ok(message));
    }

    #[test]
    fn what_a_view_sends_the_broker_reads(message in to_broker()) {
        let encoded = message.encode();
        prop_assert_eq!(ToBroker::decode(body(&encoded)), Ok(message));
    }

    /// The length in front is the length that follows, which is what lets a reader find the next message.
    #[test]
    fn a_message_is_read_off_a_stream_whole(first in to_broker(), second in to_broker()) {
        let mut stream = first.encode();
        stream.extend(second.encode());
        let mut reader = stream.as_slice();
        for expected in [first, second] {
            match read_message(&mut reader) {
                Incoming::Message(read) => prop_assert_eq!(ToBroker::decode(&read), Ok(expected)),
                other => prop_assert!(false, "read {other:?}"),
            }
        }
        prop_assert!(matches!(read_message(&mut reader), Incoming::Ended));
    }

    #[test]
    fn arbitrary_bytes_are_a_message_or_a_refusal(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        let _ = ToBroker::decode(&bytes);
        let _ = ToView::decode(&bytes);
    }

    /// Random bytes are never nearly a message, so this is the half that reaches the middle of one: a
    /// count that now claims more than is there, a tag that is no tag, a string cut inside a character.
    #[test]
    fn a_message_cut_short_or_changed_is_refused_not_trusted(
        message in to_view(),
        cut in any::<prop::sample::Index>(),
        at in any::<prop::sample::Index>(),
        with in any::<u8>(),
    ) {
        let encoded = message.encode().expect("a message this small is one");
        let whole = body(&encoded);
        let short = &whole[..cut.index(whole.len())];
        prop_assert!(ToView::decode(short).is_err(), "a message {} bytes short decoded", whole.len() - short.len());

        let mut changed = whole.to_vec();
        let position = at.index(changed.len());
        changed[position] = with;
        let _ = ToView::decode(&changed);
    }
}

/// A count is checked against what is left before anything is allocated for it: this claims four billion
/// links in a payload of a few bytes.
#[test]
fn a_count_the_payload_cannot_hold_is_refused() {
    let row = Row { line: 0, ..Row::default() };
    let one = ToView::Rows { channel: 0, request: 0, epoch: 0, first: 0, count: 1, block: Block { rows: vec![row] } };
    let mut forged = body(&one.encode().expect("a message")).to_vec();
    // kind, channel, request, epoch, first, count, rows, line, flags, then the count of the row's styles
    forged.truncate(9);
    forged.extend([0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    assert_eq!(ToView::decode(&forged), Err(Malformed::Truncated));
}

#[test]
fn a_glyph_naming_a_style_the_block_lacks_is_refused() {
    let glyph = Glyph { column: 0, width: 1, text: "a".to_owned(), style: 0, link: 0 };
    let block = Block { rows: vec![Row { line: 0, glyphs: vec![glyph], ..Row::default() }] };
    let encoded =
        ToView::Rows { channel: 0, request: 0, epoch: 0, first: 0, count: 1, block }.encode().expect("a message");
    let mut forged = body(&encoded).to_vec();
    // kind, channel, request, epoch, first, count, rows, line, flags, styles, links, runs, column, then the
    // style's index: the row has no style but the plain one, so the first is one it lacks
    forged[13] = 1;
    assert_eq!(ToView::decode(&forged), Err(Malformed::Value("a style's index")));
}

/// How a command ended is a kind of its own for each way there is, and a byte that is none of them is
/// refused rather than read as the nearest: a status that was only claimed is never one wrong byte from
/// reading as an exit.
#[test]
fn a_command_that_ended_no_way_the_format_defines_is_refused() {
    let commands = |outcome| {
        let command = CommandRow { id: 1, outcome, ..CommandRow::default() };
        let listed = ToView::Commands { channel: 0, request: 0, epoch: 0, commands: vec![command] };
        body(&listed.encode().expect("a message")).to_vec()
    };
    // kind, channel, request, epoch, how many, id, which fields, then how it ended
    let mut forged = commands(CommandOutcome::Running);
    forged[7] = 5;
    assert_eq!(ToView::decode(&forged), Err(Malformed::Value("how a command ended")));

    // And then why its end is unknown
    let mut forged = commands(CommandOutcome::Unknown { why: Unreported::NoStatus });
    forged[8] = 3;
    assert_eq!(ToView::decode(&forged), Err(Malformed::Value("why a command's end is unknown")));

    // A field the format does not have is not skipped over
    let mut forged = commands(CommandOutcome::Running);
    forged[6] = 0x80;
    assert_eq!(ToView::decode(&forged), Err(Malformed::Value("a command's fields")));
}

#[test]
fn a_message_past_the_cap_is_not_read() {
    let length = u32::try_from(MAX_MESSAGE_BYTES + 1).expect("the cap fits a length");
    let mut stream: &[u8] = &length.to_le_bytes();
    assert!(matches!(read_message(&mut stream), Incoming::TooLong));
}

/// The cap is the longest message there is, and a message of exactly that length is one.
#[test]
fn a_message_of_exactly_the_cap_is_read() {
    let length = u32::try_from(MAX_MESSAGE_BYTES).expect("the cap fits a length");
    let mut stream = length.to_le_bytes().to_vec();
    stream.resize(4 + MAX_MESSAGE_BYTES, 7);
    match read_message(&mut stream.as_slice()) {
        Incoming::Message(body) => assert_eq!(body.len(), MAX_MESSAGE_BYTES),
        other => panic!("a message at the cap was {other:?}"),
    }
}

/// Each way bytes can fail to be a message is said in words of its own.
#[test]
fn what_is_wrong_with_a_message_is_said() {
    let said = [
        Malformed::Truncated,
        Malformed::UnknownKind(250),
        Malformed::Text,
        Malformed::Overflow,
        Malformed::Value("a flag"),
        Malformed::Trailing,
    ]
    .map(|wrong| wrong.to_string());
    assert!(said.iter().all(|sentence| sentence.len() > 10), "{said:?}");
    assert_eq!(said.iter().collect::<std::collections::BTreeSet<_>>().len(), said.len(), "{said:?}");
    assert!(said[1].contains("250") && said[4].contains("a flag"), "{said:?}");
}

#[test]
fn bytes_after_a_message_are_refused() {
    let mut encoded = ToBroker::Detach { channel: 3 }.encode();
    encoded.push(0);
    assert_eq!(ToBroker::decode(body(&encoded)), Err(Malformed::Trailing));
}

#[test]
fn a_kind_no_message_has_is_named() {
    assert_eq!(ToBroker::decode(&[200]), Err(Malformed::UnknownKind(200)));
    assert_eq!(ToView::decode(&[1]), Err(Malformed::UnknownKind(1)));
}

/// A number is at most ten bytes, the tenth holding the one bit left of sixty-four. Anything more in it
/// is a value that does not fit, and is refused rather than read as its low bits.
#[test]
fn a_number_with_more_than_sixty_four_bits_is_refused() {
    let encoded = ToBroker::Detach { channel: 3 }.encode();
    let kind = body(&encoded)[0];
    let mut forged = vec![kind, 0x83];
    forged.extend([0x80; 8]);
    forged.push(0x02);
    assert_eq!(ToBroker::decode(&forged), Err(Malformed::Overflow));
}

/// **A message longer than a view reads is not made, and one exactly as long as a view reads is.**
/// Whatever asked for the longer one is told how long it came to and how long a message may be, and
/// nothing is written with a length a view would have to hang up on.
#[test]
fn an_answer_longer_than_a_message_is_refused_rather_than_written() {
    // A text's kind, channel, request, epoch and its own length are eight bytes of its message
    let at_the_cap = ToView::Text { channel: 1, request: 2, epoch: 3, text: "x".repeat(MAX_MESSAGE_BYTES - 8) };
    let written = at_the_cap.encode().expect("a message of exactly the length a view reads is one");
    match read_message(&mut written.as_slice()) {
        Incoming::Message(read) => assert_eq!(read.len(), MAX_MESSAGE_BYTES),
        other => panic!("a message at the cap was {other:?}"),
    }

    let past = ToView::Text { channel: 1, request: 2, epoch: 3, text: "x".repeat(MAX_MESSAGE_BYTES - 7) };
    let refused = past.encode().expect_err("a message a byte past the cap was written");
    assert_eq!(refused, TooLong(MAX_MESSAGE_BYTES + 1));
    let said = refused.to_string();
    assert!(
        said.contains(&(MAX_MESSAGE_BYTES + 1).to_string()) && said.contains(&MAX_MESSAGE_BYTES.to_string()),
        "{said}"
    );

    // What a view sends is written as it is: the broker reads its length first, and refuses it there
    let pasted = ToBroker::Paste { channel: 1, request: 2, allow_unsafe: false, text: "x".repeat(MAX_MESSAGE_BYTES) };
    assert!(matches!(read_message(&mut pasted.encode().as_slice()), Incoming::TooLong));
}

/// **Rows are taken until a message is full, and full allows for everything else a frame says.** The
/// most a frame can carry beside its rows — every number at its longest, a cursor, a line to forget
/// from and all 256 palette entries changed with the three colours beside them — still fits with rows
/// taken to the last byte that is, and what is then left of the message is nothing a row could have used.
#[test]
fn the_longest_head_there_is_fits_with_rows_taken_to_the_last_byte() {
    let mut rows = Rows::default();
    // Rows with nothing on them, enough that their count is written in three bytes: no message holds
    // the rows it would take to make a count longer than that
    let mut empty = Vec::new();
    write_row(&mut empty, false, false, &Row::default());
    for _ in 0..1 << 14 {
        assert!(rows.takes(empty.len()));
        rows.push(u64::MAX, &empty);
    }
    // And one more, as long as a row may be and still be taken: a byte longer is refused
    let (mut taken, mut refused) = (0, MAX_MESSAGE_BYTES);
    assert!(rows.takes(taken) && !rows.takes(refused));
    while refused - taken > 1 {
        let between = taken + (refused - taken) / 2;
        if rows.takes(between) {
            taken = between;
        } else {
            refused = between;
        }
    }
    rows.push(u64::MAX, &vec![0; taken]);

    let colors = ColorChanges {
        foreground: Some([1, 2, 3]),
        background: Some([4, 5, 6]),
        cursor: Some([7, 8, 9]),
        palette: (0..=u8::MAX).map(|index| (index, [index; 3])).collect(),
    };
    let head = FrameHead {
        channel: u32::MAX,
        sequence: u64::MAX,
        epoch: u32::MAX,
        first_active: u64::MAX,
        columns: u16::MAX,
        screen_rows: u16::MAX,
        scrollback: u32::MAX,
        commands: u32::MAX,
        images: u32::MAX,
        cursor: Some(Cursor { column: u16::MAX, row: u16::MAX, shape: 3, blinking: true, hidden: true }),
        alternate_screen: true,
        mouse: true,
        reverse_video: true,
        stale_from: Some(u64::MAX),
        colors: Some(&colors),
    };
    let message = frame(&head, &rows).expect("a frame of the rows that were taken is one that can be sent");
    // **Nothing was left on the table.** A count of rows may be five bytes and this one is three, so two
    // bytes at most went unused: the row a byte longer was refused because it would not have fitted
    // beside a count at its longest, and for no other reason
    let unused = MAX_MESSAGE_BYTES - body(&message).len();
    assert!(unused <= 2, "{unused} bytes of a message went unused under the longest row that was taken");
    // A fetch says less beside its rows than a frame does, so what a frame can carry it can
    let fetched = apack_terminal::view::wire::rows(u32::MAX, u32::MAX, u32::MAX, u64::MAX, u32::MAX, &rows);
    assert!(fetched.is_ok_and(|answer| body(&answer).len() < body(&message).len()));
}
