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

use apack_terminal::view::wire::{
    Block, Color, CommandRow, Cursor, Frame, Glyph, Incoming, MAX_MESSAGE_BYTES, Malformed, Placement, Position, Row,
    Span, Style, ToBroker, ToView, body, read_message,
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

/// A row whose glyphs are in order and do not overlap, with gaps, as a screen's are.
fn row(styles: usize, links: usize) -> impl Strategy<Value = Row> {
    let glyph = (character(), 0u16..3, 0..styles, 0..links);
    (any::<u32>(), any::<bool>(), any::<bool>(), prop::collection::vec(glyph, 0..24)).prop_map(
        |(line, wrapped, prompt, cells)| {
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
            Row { line: u64::from(line), wrapped, prompt, glyphs }
        },
    )
}

fn block() -> impl Strategy<Value = Block> {
    (prop::collection::vec(style(), 0..4), prop::collection::vec("[a-z:/.]{1,12}", 0..3)).prop_flat_map(
        |(styles, links)| {
            let styles: Vec<Style> = std::iter::once(Style::default()).chain(styles).collect();
            let links: Vec<String> = std::iter::once(String::new()).chain(links).collect();
            prop::collection::vec(row(styles.len(), links.len()), 0..6).prop_map(move |rows| Block {
                styles: styles.clone(),
                links: links.clone(),
                rows,
            })
        },
    )
}

fn position() -> impl Strategy<Value = Position> {
    (any::<u64>(), any::<u16>()).prop_map(|(line, column)| Position { line, column })
}

fn span() -> impl Strategy<Value = Span> {
    (position(), position()).prop_map(|(start, end)| Span { start, end })
}

/// A command with any of its fields present or absent, and a status of either sign.
fn command() -> impl Strategy<Value = CommandRow> {
    (
        any::<u64>(),
        (prop::option::of(".{0,12}"), prop::option::of("[a-z/]{0,12}")),
        (prop::option::of(any::<u64>()), prop::option::of(any::<u64>()), prop::option::of(any::<i32>())),
        (prop::option::of(any::<u64>()), prop::option::of(any::<u64>()), prop::option::of(any::<u64>())),
    )
        .prop_map(|(id, (command, cwd), (started_ms, finished_ms, exit_status), (prompt, output, end))| {
            CommandRow { id, command, cwd, started_ms, finished_ms, exit_status, prompt, output, end }
        })
}

fn to_view() -> impl Strategy<Value = ToView> {
    let cursor = prop::option::of(
        (any::<u16>(), any::<u16>(), 0u8..4, any::<bool>()).prop_map(|(column, row, shape, blinking)| Cursor {
            column,
            row,
            shape,
            blinking,
        }),
    );
    let frame = (
        (any::<u32>(), any::<u64>(), any::<u32>(), any::<u64>()),
        (any::<u16>(), any::<u16>(), any::<u32>(), any::<u32>(), any::<u32>()),
        (cursor, any::<bool>(), any::<bool>(), prop::option::of(any::<u64>())),
        block(),
    )
        .prop_map(|(ids, grid, state, block)| {
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
                stale_from: state.3,
                block,
            })
        });
    prop_oneof![
        frame,
        (any::<u32>(), any::<u32>(), any::<u32>(), block()).prop_map(|(channel, request, epoch, block)| ToView::Rows {
            channel,
            request,
            epoch,
            block
        }),
        any::<u32>().prop_map(|channel| ToView::Ended { channel }),
        (any::<u32>(), any::<u32>(), 0u8..2).prop_map(|(channel, request, outcome)| ToView::Pasted {
            channel,
            request,
            outcome
        }),
        (any::<u32>(), any::<u32>(), any::<u32>(), prop::option::of(span()))
            .prop_map(|(channel, request, epoch, span)| ToView::Span { channel, request, epoch, span }),
        (any::<u32>(), any::<u32>(), ".{0,40}").prop_map(|(channel, request, text)| ToView::Text {
            channel,
            request,
            text
        }),
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
        (any::<u32>(), 0u8..3, any::<u16>(), "[A-Za-z0-9]{0,12}", ".{0,4}", any::<u32>()).prop_map(
            |(channel, action, modifiers, code, text, unshifted)| ToBroker::Key {
                channel,
                action,
                modifiers,
                code,
                text,
                unshifted
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
        (any::<u32>(), any::<u32>(), ".{0,20}").prop_map(|(channel, request, needle)| ToBroker::Search {
            channel,
            request,
            needle
        }),
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
        let encoded = message.encode();
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
        let encoded = message.encode();
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
    let empty = ToView::Rows { channel: 0, request: 0, epoch: 0, block: Block::default() }.encode();
    let mut forged = body(&empty).to_vec();
    // channel, request, epoch, then the count of styles
    forged.truncate(4);
    forged.extend([0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    assert_eq!(ToView::decode(&forged), Err(Malformed::Truncated));
}

#[test]
fn a_glyph_naming_a_style_the_block_lacks_is_refused() {
    let glyph = Glyph { column: 0, width: 1, text: "a".to_owned(), style: 0, link: 0 };
    let block =
        Block { rows: vec![Row { line: 0, wrapped: false, prompt: false, glyphs: vec![glyph] }], ..Block::default() };
    let encoded = ToView::Rows { channel: 0, request: 0, epoch: 0, block }.encode();
    let mut forged = body(&encoded).to_vec();
    // kind, channel, request, epoch, styles, links, rows, line, flags, runs, column, then the style's index
    forged[11] = 5;
    assert_eq!(ToView::decode(&forged), Err(Malformed::Value("a style's index")));
}

#[test]
fn a_message_past_the_cap_is_not_read() {
    let length = u32::try_from(MAX_MESSAGE_BYTES + 1).expect("the cap fits a length");
    let mut stream: &[u8] = &length.to_le_bytes();
    assert!(matches!(read_message(&mut stream), Incoming::TooLong));
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
