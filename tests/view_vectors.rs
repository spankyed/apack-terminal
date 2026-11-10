//! The view protocol's bytes, written down for the other side to read.
//!
//! **The broker encodes in Rust and a view decodes in TypeScript**, in another repository, so neither
//! suite can see the other's half. What they share is `node/view-vectors.json`: each message here as the
//! bytes this encoder produces and as the value those bytes mean. This case fails when the file is not
//! what the encoder now writes; the view's own suite decodes every vector for a view and encodes every
//! one for the broker, and fails the same way from its side.
//!
//! Re-record with `APACK_UPDATE_VECTORS=1 cargo nextest run --test view_vectors`, and read the diff: every
//! line of it is a change a view has to be taught.

use apack_terminal::view::wire::{
    Block, Color, CommandRow, Cursor, Frame, Glyph, NO_BUTTON, Placement, Position, Row, Span, Style, ToBroker, ToView,
    body,
};
use serde_json::{Value, json};

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/node/view-vectors.json");

/// The name a view knows a numbered value by.
fn named(names: &[&'static str], index: u8) -> &'static str {
    names[usize::from(index)]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn color(color: Color) -> Value {
    match color {
        Color::Default => Value::Null,
        Color::Palette(index) => json!({ "palette": index }),
        Color::Rgb(r, g, b) => json!({ "rgb": (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b) }),
    }
}

fn position(at: Position) -> Value {
    json!({ "line": at.line, "column": at.column })
}

fn span(span: Span) -> Value {
    json!({ "start": position(span.start), "end": position(span.end) })
}

fn block(block: &Block) -> (Value, Value, Value) {
    let styles: Vec<Value> = block
        .styles
        .iter()
        .map(|style| {
            json!({ "fg": color(style.fg), "bg": color(style.bg), "underline": color(style.underline), "flags": style.flags })
        })
        .collect();
    let rows: Vec<Value> = block
        .rows
        .iter()
        .map(|row| {
            let glyphs: Vec<Value> = row
                .glyphs
                .iter()
                .map(|glyph| {
                    json!({ "column": glyph.column, "width": glyph.width, "text": glyph.text, "style": glyph.style, "link": glyph.link })
                })
                .collect();
            json!({ "line": row.line, "wrapped": row.wrapped, "prompt": row.prompt, "glyphs": glyphs })
        })
        .collect();
    (json!(styles), json!(block.links), json!(rows))
}

fn to_view(message: &ToView) -> Value {
    match message {
        ToView::Frame(frame) => {
            let (styles, links, rows) = block(&frame.block);
            let cursor = frame.cursor.map(|cursor| {
                let shape = ["block", "bar", "underline", "hollow"][usize::from(cursor.shape)];
                json!({ "column": cursor.column, "row": cursor.row, "shape": shape, "blinking": cursor.blinking })
            });
            json!({
                "kind": "frame", "channel": frame.channel, "sequence": frame.sequence, "epoch": frame.epoch,
                "firstActive": frame.first_active, "columns": frame.columns, "screenRows": frame.screen_rows,
                "scrollback": frame.scrollback, "commands": frame.commands, "images": frame.images, "cursor": cursor,
                "alternateScreen": frame.alternate_screen,
                "mouse": frame.mouse, "staleFrom": frame.stale_from, "styles": styles, "links": links, "rows": rows,
            })
        }
        ToView::Rows { channel, request, epoch, block: rows_block } => {
            let (styles, links, rows) = block(rows_block);
            json!({ "kind": "rows", "channel": channel, "request": request, "epoch": epoch, "styles": styles, "links": links, "rows": rows })
        }
        ToView::Ended { channel } => json!({ "kind": "ended", "channel": channel }),
        ToView::Pasted { channel, request, outcome } => {
            json!({ "kind": "pasted", "channel": channel, "request": request, "outcome": named(&["pasted", "unsafe"], *outcome) })
        }
        ToView::Span { channel, request, epoch, span: found } => {
            json!({ "kind": "span", "channel": channel, "request": request, "epoch": epoch, "span": found.map(span) })
        }
        ToView::Text { channel, request, text } => {
            json!({ "kind": "text", "channel": channel, "request": request, "text": text })
        }
        ToView::Spans { channel, request, epoch, spans } => {
            let spans: Vec<Value> = spans.iter().copied().map(span).collect();
            json!({ "kind": "spans", "channel": channel, "request": request, "epoch": epoch, "spans": spans })
        }
        ToView::Failed { channel, request, message } => {
            json!({ "kind": "failed", "channel": channel, "request": request, "message": message })
        }
        ToView::Commands { channel, request, epoch, commands } => {
            let commands: Vec<Value> = commands
                .iter()
                .map(|one| {
                    json!({
                        "id": one.id, "command": one.command, "cwd": one.cwd, "startedMs": one.started_ms,
                        "finishedMs": one.finished_ms, "exitStatus": one.exit_status, "prompt": one.prompt,
                        "output": one.output, "end": one.end,
                    })
                })
                .collect();
            json!({ "kind": "commands", "channel": channel, "request": request, "epoch": epoch, "commands": commands })
        }
        ToView::Images { channel, request, epoch, cell_width, cell_height, placements } => {
            let placements: Vec<Value> = placements
                .iter()
                .map(|one| {
                    json!({
                        "image": one.image, "generation": one.generation, "z": one.z, "line": one.line,
                        "column": one.column, "columns": one.columns, "rows": one.rows, "offsetX": one.offset_x,
                        "offsetY": one.offset_y, "width": one.width, "height": one.height, "sourceX": one.source_x, "sourceY": one.source_y,
                        "sourceWidth": one.source_width, "sourceHeight": one.source_height,
                    })
                })
                .collect();
            json!({
                "kind": "images", "channel": channel, "request": request, "epoch": epoch, "cellWidth": cell_width,
                "cellHeight": cell_height, "placements": placements,
            })
        }
        ToView::Image { channel, request, image, generation, width, height, channels, total, offset, bytes } => {
            json!({
                "kind": "image", "channel": channel, "request": request, "image": image, "generation": generation,
                "width": width, "height": height, "channels": channels, "total": total, "offset": offset,
                "bytes": bytes,
            })
        }
    }
}

fn to_broker(message: &ToBroker) -> Value {
    match message {
        ToBroker::Attach { channel, session } => json!({ "kind": "attach", "channel": channel, "session": session }),
        ToBroker::Detach { channel } => json!({ "kind": "detach", "channel": channel }),
        ToBroker::Acknowledge { channel, sequence } => {
            json!({ "kind": "acknowledge", "channel": channel, "sequence": sequence })
        }
        ToBroker::Fetch { channel, request, epoch, first, count } => {
            json!({ "kind": "fetch", "channel": channel, "request": request, "epoch": epoch, "first": first, "count": count })
        }
        ToBroker::Key { channel, action, modifiers, code, text, unshifted } => json!({
            "kind": "key", "channel": channel, "action": named(&["press", "repeat", "release"], *action),
            "modifiers": modifiers, "code": code, "text": text, "unshifted": unshifted,
        }),
        ToBroker::Text { channel, text } => json!({ "kind": "text", "channel": channel, "text": text }),
        ToBroker::Pointer { channel, action, button, modifiers, x, y, cell_width, cell_height } => json!({
            "kind": "pointer", "channel": channel,
            "action": named(&["press", "release", "move", "wheelUp", "wheelDown"], *action),
            "button": if *button == NO_BUTTON { -1 } else { i32::from(*button) },
            "modifiers": modifiers, "x": x, "y": y, "cellWidth": cell_width, "cellHeight": cell_height,
        }),
        ToBroker::Paste { channel, request, allow_unsafe, text } => {
            json!({ "kind": "paste", "channel": channel, "request": request, "allowUnsafe": allow_unsafe, "text": text })
        }
        ToBroker::Focus { channel, gained } => json!({ "kind": "focus", "channel": channel, "gained": gained }),
        ToBroker::WordAt { channel, request, epoch, at } => {
            json!({ "kind": "wordAt", "channel": channel, "request": request, "epoch": epoch, "at": position(*at) })
        }
        ToBroker::LineAt { channel, request, epoch, at } => {
            json!({ "kind": "lineAt", "channel": channel, "request": request, "epoch": epoch, "at": position(*at) })
        }
        ToBroker::TextOf { channel, request, epoch, span: asked } => {
            json!({ "kind": "textOf", "channel": channel, "request": request, "epoch": epoch, "span": span(*asked) })
        }
        ToBroker::Search { channel, request, needle } => {
            json!({ "kind": "search", "channel": channel, "request": request, "needle": needle })
        }
        ToBroker::Commands { channel, request } => {
            json!({ "kind": "commands", "channel": channel, "request": request })
        }
        ToBroker::Images { channel, request } => {
            json!({ "kind": "images", "channel": channel, "request": request })
        }
        ToBroker::Image { channel, request, image, offset } => {
            json!({ "kind": "image", "channel": channel, "request": request, "image": image, "offset": offset })
        }
    }
}

/// A screen with one of everything a row can hold.
fn sample_block() -> Block {
    let glyph = |column: u16, width: u8, text: &str, style: u16, link: u16| Glyph {
        column,
        width,
        text: text.to_owned(),
        style,
        link,
    };
    Block {
        styles: vec![
            Style::default(),
            Style { fg: Color::Palette(1), bg: Color::Default, underline: Color::Default, flags: 1 },
            Style {
                fg: Color::Rgb(0x12, 0x34, 0x56),
                bg: Color::Palette(250),
                underline: Color::Rgb(255, 0, 128),
                flags: 0x302,
            },
        ],
        links: vec![String::new(), "https://example.com/a?b=c".to_owned()],
        rows: vec![
            // Plain text, a gap, then styled and linked runs side by side
            Row {
                line: 0,
                wrapped: false,
                prompt: true,
                glyphs: vec![
                    glyph(0, 1, "$", 0, 0),
                    glyph(2, 1, "l", 1, 0),
                    glyph(3, 1, "s", 1, 0),
                    glyph(5, 1, "x", 2, 1),
                    glyph(6, 1, "y", 2, 1),
                ],
            },
            // A wide character, one of several code points, and a letter of two bytes
            Row {
                line: 1,
                wrapped: true,
                prompt: false,
                glyphs: vec![
                    glyph(0, 2, "\u{4e16}", 0, 0),
                    glyph(2, 1, "e\u{301}", 0, 0),
                    glyph(3, 2, "\u{1F468}\u{200D}\u{1F469}", 0, 0),
                    glyph(5, 1, "\u{e9}", 0, 0),
                ],
            },
            // An empty row, and one far enough up that its number takes more than a byte
            Row { line: 2, wrapped: false, prompt: false, glyphs: Vec::new() },
            Row { line: 123_456_789, wrapped: false, prompt: false, glyphs: vec![glyph(79, 1, "z", 0, 0)] },
        ],
    }
}

fn vectors() -> Value {
    let frame = |cursor: Option<Cursor>, stale_from: Option<u64>, block: Block| {
        ToView::Frame(Frame {
            channel: 3,
            sequence: 41,
            epoch: 2,
            first_active: 1_000,
            columns: 80,
            screen_rows: 24,
            scrollback: 1_000,
            commands: 7,
            images: 4,
            cursor,
            alternate_screen: stale_from.is_some(),
            mouse: cursor.is_none(),
            stale_from,
            block,
        })
    };
    let at = Position { line: 70_000, column: 12 };
    let across = Span { start: Position { line: 9, column: 0 }, end: Position { line: 11, column: 79 } };
    let for_a_view: Vec<(&str, ToView)> = vec![
        (
            "a frame with one of everything a row can hold",
            frame(Some(Cursor { column: 4, row: 23, shape: 1, blinking: true }), None, sample_block()),
        ),
        ("a frame with no cursor and nothing changed", frame(None, None, Block::default())),
        (
            "a frame naming stale history, with a steady underline cursor",
            frame(Some(Cursor { column: 0, row: 0, shape: 2, blinking: false }), Some(990), Block::default()),
        ),
        ("fetched rows", ToView::Rows { channel: 3, request: 7, epoch: 2, block: sample_block() }),
        ("a session ending", ToView::Ended { channel: 3 }),
        ("a paste that went", ToView::Pasted { channel: 3, request: 8, outcome: 0 }),
        ("a paste refused", ToView::Pasted { channel: 3, request: 9, outcome: 1 }),
        ("a word", ToView::Span { channel: 3, request: 10, epoch: 2, span: Some(across) }),
        ("no word", ToView::Span { channel: 3, request: 11, epoch: 2, span: None }),
        ("text", ToView::Text { channel: 3, request: 12, text: "one\ntwo \u{4e16}".to_owned() }),
        (
            "matches",
            ToView::Spans { channel: 3, request: 13, epoch: 2, spans: vec![across, Span { start: at, end: at }] },
        ),
        ("no matches", ToView::Spans { channel: 3, request: 14, epoch: 2, spans: Vec::new() }),
        ("a refusal", ToView::Failed { channel: 3, request: 15, message: "no session s-9".to_owned() }),
        (
            "commands: one finished with everything known, one that failed with a negative status, one waiting",
            ToView::Commands {
                channel: 3,
                request: 17,
                epoch: 2,
                commands: vec![
                    CommandRow {
                        id: 1,
                        command: Some("cargo test".to_owned()),
                        cwd: Some("/src/app".to_owned()),
                        started_ms: Some(1_794_000_000_000),
                        finished_ms: Some(1_794_000_012_345),
                        exit_status: Some(0),
                        prompt: Some(40),
                        output: Some(41),
                        end: Some(90),
                    },
                    CommandRow {
                        id: 2,
                        command: None,
                        cwd: None,
                        started_ms: Some(1_794_000_020_000),
                        finished_ms: Some(1_794_000_020_500),
                        exit_status: Some(-1),
                        prompt: None,
                        output: None,
                        end: Some(95),
                    },
                    CommandRow { id: 300, prompt: Some(95), ..CommandRow::default() },
                ],
            },
        ),
        (
            "images: one under the text and cut from a larger picture, one over it and placed whole",
            ToView::Images {
                channel: 3,
                request: 18,
                epoch: 2,
                cell_width: 9,
                cell_height: 18,
                placements: vec![
                    Placement {
                        image: 7,
                        generation: 5_000_000_000,
                        z: -1,
                        line: 990,
                        column: 4,
                        columns: 20,
                        rows: 6,
                        offset_x: 3,
                        offset_y: 5,
                        width: 177,
                        height: 103,
                        source_x: 10,
                        source_y: 20,
                        source_width: 180,
                        source_height: 108,
                    },
                    Placement {
                        image: 300,
                        generation: 1,
                        z: 2,
                        line: 1_010,
                        columns: 1,
                        rows: 1,
                        ..Placement::default()
                    },
                ],
            },
        ),
        (
            "no images",
            ToView::Images { channel: 3, request: 19, epoch: 2, cell_width: 0, cell_height: 0, placements: Vec::new() },
        ),
        (
            "an image's pixels: the second part of a two by two picture in red, green, blue and alpha",
            ToView::Image {
                channel: 3,
                request: 20,
                image: 7,
                generation: 5_000_000_000,
                width: 2,
                height: 2,
                channels: 4,
                total: 16,
                offset: 8,
                bytes: vec![0, 0, 255, 255, 255, 255, 255, 128],
            },
        ),
    ];
    let for_the_broker: Vec<(&str, ToBroker)> = vec![
        ("attaching", ToBroker::Attach { channel: 3, session: "s-12".to_owned() }),
        ("detaching", ToBroker::Detach { channel: 3 }),
        ("acknowledging", ToBroker::Acknowledge { channel: 3, sequence: 41 }),
        ("fetching", ToBroker::Fetch { channel: 3, request: 7, epoch: 2, first: 69_000, count: 200 }),
        (
            "a letter with shift",
            ToBroker::Key {
                channel: 3,
                action: 0,
                modifiers: 1,
                code: "KeyA".to_owned(),
                text: "A".to_owned(),
                unshifted: 97,
            },
        ),
        (
            "an arrow released with control and alt",
            ToBroker::Key {
                channel: 3,
                action: 2,
                modifiers: 6,
                code: "ArrowUp".to_owned(),
                text: String::new(),
                unshifted: 0,
            },
        ),
        (
            "a key held",
            ToBroker::Key {
                channel: 3,
                action: 1,
                modifiers: 0,
                code: "KeyJ".to_owned(),
                text: "j".to_owned(),
                unshifted: 106,
            },
        ),
        ("composed text", ToBroker::Text { channel: 3, text: "\u{65e5}\u{672c}\u{8a9e}".to_owned() }),
        (
            "a click",
            ToBroker::Pointer {
                channel: 3,
                action: 0,
                button: 0,
                modifiers: 0,
                x: 104.5,
                y: 37.25,
                cell_width: 8.5,
                cell_height: 17.0,
            },
        ),
        (
            "a move with nothing held",
            ToBroker::Pointer {
                channel: 3,
                action: 2,
                button: NO_BUTTON,
                modifiers: 0,
                x: 0.0,
                y: 0.0,
                cell_width: 8.0,
                cell_height: 16.0,
            },
        ),
        (
            "the wheel with shift",
            ToBroker::Pointer {
                channel: 3,
                action: 4,
                button: NO_BUTTON,
                modifiers: 1,
                x: 12.0,
                y: 300.0,
                cell_width: 8.0,
                cell_height: 16.0,
            },
        ),
        ("a paste", ToBroker::Paste { channel: 3, request: 8, allow_unsafe: false, text: "ls -la\n".to_owned() }),
        (
            "a paste the person allowed",
            ToBroker::Paste { channel: 3, request: 9, allow_unsafe: true, text: "x".to_owned() },
        ),
        ("focus gained", ToBroker::Focus { channel: 3, gained: true }),
        ("focus lost", ToBroker::Focus { channel: 3, gained: false }),
        ("asking for a word", ToBroker::WordAt { channel: 3, request: 10, epoch: 2, at }),
        ("asking for a line", ToBroker::LineAt { channel: 3, request: 16, epoch: 2, at }),
        ("asking for text", ToBroker::TextOf { channel: 3, request: 12, epoch: 2, span: across }),
        ("searching", ToBroker::Search { channel: 3, request: 13, needle: "error:".to_owned() }),
        ("asking for the commands", ToBroker::Commands { channel: 3, request: 17 }),
        ("asking where the images are", ToBroker::Images { channel: 3, request: 18 }),
        ("asking for an image's pixels", ToBroker::Image { channel: 3, request: 20, image: 7, offset: 8 }),
    ];

    let mut all: Vec<Value> = Vec::new();
    for (name, message) in &for_a_view {
        let encoded = message.encode();
        assert_eq!(ToView::decode(body(&encoded)).as_ref(), Ok(message), "{name} does not read back");
        all.push(json!({ "name": name, "to": "view", "hex": hex(&encoded), "message": to_view(message) }));
    }
    for (name, message) in &for_the_broker {
        let encoded = message.encode();
        assert_eq!(ToBroker::decode(body(&encoded)).as_ref(), Ok(message), "{name} does not read back");
        all.push(json!({ "name": name, "to": "broker", "hex": hex(&encoded), "message": to_broker(message) }));
    }
    json!(all)
}

#[test]
fn the_vectors_a_view_reads_are_what_this_encoder_writes() {
    let mut written = serde_json::to_string_pretty(&vectors()).expect("vectors serialize");
    written.push('\n');
    if std::env::var_os("APACK_UPDATE_VECTORS").is_some() {
        std::fs::write(VECTORS, &written).expect("the vectors are written");
    }
    let committed = std::fs::read_to_string(VECTORS).unwrap_or_default();
    assert!(
        committed == written,
        "node/view-vectors.json is not what the encoder writes. A view in another repository reads these \
         bytes: re-record with APACK_UPDATE_VECTORS=1, read the diff, and take the new file to it"
    );
}

/// Every kind of message has a vector, so a kind added to the protocol cannot be left out of what a view
/// is tested against.
#[test]
fn every_kind_of_message_has_a_vector() {
    let kinds: std::collections::BTreeSet<String> = vectors()
        .as_array()
        .expect("the vectors are a list")
        .iter()
        .map(|vector| {
            format!("{}:{}", vector["to"].as_str().unwrap_or(""), vector["message"]["kind"].as_str().unwrap_or(""))
        })
        .collect();
    let expected = [
        "broker:acknowledge",
        "broker:attach",
        "broker:commands",
        "broker:detach",
        "broker:fetch",
        "broker:focus",
        "broker:image",
        "broker:images",
        "broker:key",
        "broker:lineAt",
        "broker:paste",
        "broker:pointer",
        "broker:search",
        "broker:text",
        "broker:textOf",
        "broker:wordAt",
        "view:commands",
        "view:ended",
        "view:failed",
        "view:frame",
        "view:image",
        "view:images",
        "view:pasted",
        "view:rows",
        "view:span",
        "view:spans",
        "view:text",
    ];
    assert_eq!(kinds.into_iter().collect::<Vec<_>>(), expected);
}
