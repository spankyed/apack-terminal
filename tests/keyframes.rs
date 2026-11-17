//! A recording's keyframe: that a terminal put back from one goes on exactly as the terminal it was taken
//! from, whatever a program had done to it; that it is bounded; that what is not one is refused; and that
//! a terminal put back is under the settings of the session it is put back in.
//!
//! **What "the same terminal" means here is what a program and a view can tell.** Two terminals are the
//! same when every row they hold reads alike, the cursor is drawn alike, the modes a program can set are
//! alike — and when they *stay* alike under whatever is written next, which is what finds the state no
//! read shows: a saved cursor, a character set, a tab stop, a margin.

use std::sync::mpsc::channel;

use apack_terminal::protocol::{CursorBlink, CursorStyle};
use apack_terminal::recording::{Entry, Timed};
use apack_terminal::screen::{Colors, KEYFRAME_BYTES, KEYFRAME_HISTORY_LINES, Screen};
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::wire::{ColorChanges, Cursor, Frame, ToView, body};
use apack_terminal::vt::Terminal;
use apack_terminal::vt::rows::{CursorRead, Rendering, RowRead, Space};
use proptest::prelude::*;

use crate::screens::{keyframe, keyframe_of};

const COLUMNS: u16 = 24;
const ROWS: u16 = 6;

/// A terminal as a session's is set up, less where its reports go (`Screen::build`).
fn terminal(columns: u16, rows: u16) -> Terminal {
    let mut terminal = Terminal::new(columns, rows).expect("a terminal");
    terminal.set_cursor_defaults(CursorStyle::Bar, CursorBlink::Program).expect("the cursor's defaults are set");
    terminal.reset_cursor();
    terminal.set_scrollback_lines(100_000).expect("the history is bounded in lines");
    terminal.set_scrollback_bytes(256 << 20).expect("the history is bounded in bytes");
    terminal.set_mode(2027, true).expect("clustering is on");
    terminal.refuse_images().expect("images are refused");
    terminal
}

/// The DEC private modes a program sets that say something of how a terminal behaves or is typed at.
const MODES: [u16; 22] =
    [1, 3, 5, 6, 7, 8, 9, 12, 25, 45, 66, 69, 1000, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 1049, 2004];

/// Everything of a terminal that can be read.
#[derive(Debug, PartialEq)]
struct Seen {
    rows: Vec<RowRead>,
    cursor: Option<CursorRead>,
    pending_wrap: bool,
    alternate: bool,
    mouse: bool,
    modes: Vec<Option<bool>>,
    colors: ColorChanges,
}

fn seen(terminal: &Terminal) -> Seen {
    let held = terminal.scrollback_rows().expect("the history is counted") + u32::from(terminal.rows().expect("rows"));
    let mut rendering = Rendering::new().expect("a render state");
    rendering.look(terminal, &mut Vec::new()).expect("the terminal is looked at");
    Seen {
        rows: (0..held)
            .map(|row| terminal.read_row(Space::Screen, row).expect("a row reads").unwrap_or_default())
            .collect(),
        cursor: rendering.cursor().expect("the cursor reads"),
        pending_wrap: terminal.cursor_pending_wrap().expect("the wrap reads"),
        alternate: terminal.alt_screen().expect("the screen reads"),
        mouse: terminal.mouse_tracking().expect("the mouse reads"),
        modes: MODES.iter().map(|mode| terminal.mode(*mode).ok()).collect(),
        colors: terminal.color_changes().expect("the colours read"),
    }
}

/// What a program writes, a whole sequence or a run of text at a time: each thing that leaves a terminal
/// in a state, and each thing that shows what state it was left in.
const WRITTEN: &[&str] = &[
    // Text: plain, wrapping, wide, clustered, a line's end, a tab
    "word ",
    "a line of text that is longer than the screen is wide and so wraps",
    "\u{4e2d}\u{6587}",
    "e\u{301}",
    "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
    "\r\n",
    "\n",
    "\r",
    "\t",
    "\x08",
    // The cursor: where it is, the one saved, how it is drawn, whether it shows
    "\x1b[H",
    "\x1b[3;7H",
    "\x1b[A",
    "\x1b[2B",
    "\x1b[5C",
    "\x1b[2D",
    "\x1b7",
    "\x1b8",
    "\x1b[s",
    "\x1b[u",
    "\x1b[0 q",
    "\x1b[1 q",
    "\x1b[2 q",
    "\x1b[3 q",
    "\x1b[4 q",
    "\x1b[5 q",
    "\x1b[6 q",
    "\x1b[?12h",
    "\x1b[?12l",
    "\x1b[?25l",
    "\x1b[?25h",
    // How text is drawn: its attributes, a link, protection, a character set
    "\x1b[1;31m",
    "\x1b[4;38;5;196;48;2;1;2;3m",
    "\x1b[7m",
    "\x1b[0m",
    "\x1b]8;;https://example.test/a\x1b\\",
    "\x1b]8;;\x1b\\",
    "\x1b[1\"q",
    "\x1b[0\"q",
    "\x1b(0",
    "\x1b(B",
    "\x1b)0",
    "\x0e",
    "\x0f",
    "qqxx",
    // Where text goes: margins, the origin, insertion, wrapping, tab stops
    "\x1b[2;4r",
    "\x1b[r",
    "\x1b[?6h",
    "\x1b[?6l",
    "\x1b[4h",
    "\x1b[4l",
    "\x1b[?7l",
    "\x1b[?7h",
    "\x1bH",
    "\x1b[g",
    "\x1b[3g",
    // What moves it: scrolling, inserting and deleting, erasing
    "\x1bD",
    "\x1bM",
    "\x1bE",
    "\x1b[2S",
    "\x1b[T",
    "\x1b[L",
    "\x1b[M",
    "\x1b[2@",
    "\x1b[P",
    "\x1b[3X",
    "\x1b[K",
    "\x1b[1K",
    "\x1b[J",
    "\x1b[2J",
    "\x1b[3J",
    "\x1b[b",
    // The other screen, each way in and out
    "\x1b[?1049h",
    "\x1b[?1049l",
    "\x1b[?1047h",
    "\x1b[?1047l",
    "\x1b[?47h",
    "\x1b[?47l",
    "\x1b[?1048h",
    "\x1b[?1048l",
    // What a program is typed at with, and told of
    "\x1b[?1h",
    "\x1b[?1l",
    "\x1b=",
    "\x1b>",
    "\x1b[?1000h",
    "\x1b[?1002h",
    "\x1b[?1003h",
    "\x1b[?1006h",
    "\x1b[?1000l",
    "\x1b[?1004h",
    "\x1b[?2004h",
    "\x1b[?2004l",
    "\x1b[>5u",
    "\x1b[<u",
    "\x1b[>4;2m",
    // Modes kept and put back
    "\x1b[?7;25;1000s",
    "\x1b[?7;25;1000r",
    // Its colours
    "\x1b[?5h",
    "\x1b[?5l",
    "\x1b]10;#ff0000\x07",
    "\x1b]11;#00ff00\x07",
    "\x1b]12;#0000ff\x07",
    "\x1b]110\x07",
    "\x1b]4;1;#123456\x07",
    "\x1b]104\x07",
    // What the shell says of itself, and a title
    "\x1b]133;A\x07$ ",
    "\x1b]133;B\x07",
    "\x1b]133;C\x07",
    "\x1b]133;D;0\x07",
    "\x1b]0;a title\x07",
    // A hold on the screen, and the two resets
    "\x1b[?2026h",
    "\x1b[?2026l",
    "\x1b[!p",
    "\x1bc",
];

/// Left and right margins: the mode that allows them, a pair of them, and the mode off again.
const MARGINS: &[&str] = &["\x1b[?69h", "\x1b[3;12s", "\x1b[?69l"];

/// Up to `most` of the things a program writes, one after another.
fn written(most: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(prop::sample::select(WRITTEN), 0..most).prop_map(|pieces| pieces.concat().into_bytes())
}

/// What saves the cursor: by itself, with the screen switched, and `CSI s`, which is the cursor saved
/// while margins are not allowed and a pair of margins while they are.
const SAVES_THE_CURSOR: &[&str] = &["\x1b7", "\x1b[s", "\x1b[?1048h", "\x1b[?1049h"];

/// The same, with left and right margins among them, and nothing that saves the cursor.
fn written_in_margins(most: usize) -> impl Strategy<Value = Vec<u8>> {
    let unsaved: Vec<&str> = WRITTEN.iter().copied().filter(|piece| !SAVES_THE_CURSOR.contains(piece)).collect();
    let pieces = prop_oneof![9 => prop::sample::select(unsaved), 1 => prop::sample::select(MARGINS)];
    prop::collection::vec(pieces, 0..most).prop_map(|pieces| pieces.concat().into_bytes())
}

/// A terminal written out after `before`, put back in place of one that held `replaced`, and both fed
/// `after`: what each reads as when put back, and after what was written next.
fn put_back(before: &[u8], after: &[u8], replaced: &[u8]) -> Option<[(Seen, Seen); 2]> {
    let mut live = terminal(COLUMNS, ROWS);
    live.feed(before);
    // Where a session takes one: not in the one state the core does not write out
    if !live.writes_out_whole() {
        return None;
    }
    let taken =
        live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("a terminal between sequences is written out");
    // Put back in place of a terminal that had a state of its own, none of which is left
    let mut back = terminal(COLUMNS, ROWS);
    back.feed(replaced);
    back.restore(&taken).expect("a keyframe is put back");
    let at_once = (seen(&back), seen(&live));
    live.feed(after);
    back.feed(after);
    Some([at_once, (seen(&back), seen(&live))])
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// **A terminal put back from its keyframe is the terminal the keyframe was taken from, and goes on as
    /// it.** Whatever a program wrote before the keyframe, the terminal put back reads as the one it was
    /// taken from, and after whatever is written next it still does — which is what a replay from a
    /// keyframe rests on, and what the sequences that redraw a screen do not give it.
    #[test]
    fn a_terminal_put_back_from_its_keyframe_goes_on_as_the_one_it_was_taken_from(
        before in written(40),
        after in written(40),
        replaced in written(10),
    ) {
        let put_back = put_back(&before, &after, &replaced);
        prop_assume!(put_back.is_some());
        let [(back, live), (back_later, live_later)] = put_back.expect("assumed");
        prop_assert_eq!(back, live, "as it was put back");
        prop_assert_eq!(back_later, live_later, "after what was written next");
    }

    /// **And inside left and right margins, less two things the core does not hold there.**
    ///
    /// *Which rows carry the shell's prompt mark.* With margins set, a scroll moves the cells between
    /// them and leaves the rows where they are, and whether a row keeps the mark the shell put on it
    /// then differs between a terminal and the one put back from a keyframe taken earlier — with nothing
    /// else of the two differing before or after, and no difference where the keyframe is taken just
    /// before the scroll. It is recorded here rather than explained, and the marks are left out of what
    /// is compared.
    ///
    /// *A cursor saved with a wrap pending at a margin.* The core writes out no wrap pending short of
    /// the screen's edge (`a_wrap_pending_at_a_right_margin_…`), and a saved cursor carries one as the
    /// cursor does. A session takes no keyframe while its own cursor is in that state; a saved cursor
    /// cannot be asked about, so nothing here saves one.
    ///
    /// Every row's text and style, the cursor and the modes are held to the same as above.
    #[test]
    fn a_terminal_put_back_goes_on_as_the_one_it_was_taken_from_inside_margins(
        before in written_in_margins(40),
        after in written_in_margins(40),
    ) {
        let put_back = put_back(&before, &after, &[]);
        prop_assume!(put_back.is_some());
        for (what, (mut back, mut live)) in ["as it was put back", "after what was written next"].into_iter().zip(put_back.expect("assumed")) {
            for row in back.rows.iter_mut().chain(live.rows.iter_mut()) {
                row.prompt = false;
            }
            prop_assert_eq!(back, live, "{}", what);
        }
    }

    /// **A moment is the same screen from whichever keyframe it is replayed.** A recording holds a
    /// keyframe every so often and a replay starts at any of them, so each has to lead where the session
    /// went: here a session's output with a keyframe taken between every few things it wrote, replayed
    /// from each in turn.
    #[test]
    fn a_moment_is_the_same_screen_from_whichever_keyframe_it_is_replayed(
        stretches in prop::collection::vec(written(12), 1..6),
    ) {
        let mut live = Screen::new(COLUMNS, ROWS);
        let mut entries = Vec::new();
        let mut keyframes = Vec::new();
        for (index, stretch) in stretches.iter().enumerate() {
            // Where a session takes one: not in the one state the core does not write out
            if live.writes_out_whole() {
                keyframes.push(entries.len());
                entries.push(Timed { at: index as u64, entry: keyframe_of(&live, COLUMNS, ROWS) });
            }
            live.feed(stretch);
            entries.push(Timed { at: index as u64, entry: Entry::Output(stretch.clone()) });
        }
        let ended = showing(&mut live);
        for from in keyframes {
            let mut replayed = Screen::new(COLUMNS, ROWS);
            prop_assert!(replayed.replay_all(&entries[from..]));
            prop_assert_eq!(showing(&mut replayed), ended.clone(), "replayed from the keyframe at entry {}", from);
        }
    }
}

/// What a view of `screen` is shown: every row of the screen, and the cursor as it is drawn.
fn showing(screen: &mut Screen) -> (Vec<RowRead>, Option<Cursor>, bool) {
    let rows = (0..u32::from(screen.rows()))
        .map(|row| screen.row(Space::Active, row).expect("a row reads").unwrap_or_default())
        .collect();
    (rows, first_frame(screen).cursor, screen.snapshot(0).alt_screen)
}

/// The frame a view that attaches to `screen` now is sent first.
fn first_frame(screen: &mut Screen) -> Frame {
    let (sink, incoming) = channel();
    let id = ViewId { connection: 9, channel: 9 };
    screen.attach_view(id, sink).expect("a view attaches");
    let sent = incoming.try_recv().expect("a view is sent a frame as it attaches");
    screen.detach_view(id);
    match ToView::decode(body(&sent)).expect("a message decodes") {
        ToView::Frame(frame) => frame,
        other => panic!("expected a frame, found {other:?}"),
    }
}

/// **Three things the sequences that redraw a screen do not hold, each by name.** The cursor a program
/// saved, without which restoring it after a replay goes to the wrong cell and what is printed next
/// overwrites the wrong text; the shape a program asked the cursor to be; and the shell's screen under a
/// full-screen program, without which leaving the program shows an empty one.
#[test]
fn a_keyframe_holds_a_saved_cursor_a_cursors_shape_and_the_screen_under_a_program() {
    let cases: [(&str, &str, &str); 3] = [
        ("a saved cursor", "ab\x1b7\r\n\r\ncd", "\x1b8X"),
        ("a cursor's shape", "text\x1b[4 q", ""),
        ("the screen under a program", "the shell's screen\x1b[?1049h\x1b[2Ja program", "\x1b[?1049l!"),
    ];
    for (what, before, after) in cases {
        let mut live = terminal(COLUMNS, ROWS);
        live.feed(before.as_bytes());
        let taken = live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
        let mut back = terminal(COLUMNS, ROWS);
        back.restore(&taken).expect("a keyframe is put back");
        live.feed(after.as_bytes());
        back.feed(after.as_bytes());
        assert_eq!(seen(&back), seen(&live), "{what}");
    }
    // And they are what they are said to be, so that the cases above are not two terminals wrong alike
    let mut saved = terminal(COLUMNS, ROWS);
    saved.feed(b"ab\x1b7\r\n\r\ncd\x1b8X");
    assert_eq!(saved.text_between((0, 0), (COLUMNS - 1, 0)).expect("the row reads").trim_end(), "abX");
    let mut shaped = terminal(COLUMNS, ROWS);
    shaped.feed(b"\x1b[4 q");
    assert_eq!(seen(&shaped).cursor.map(|cursor| (cursor.shape, cursor.blinking)), Some((2, false)));
}

/// **What the core's snapshot does not hold, and that a session takes no keyframe where it can tell.** A
/// line about to wrap at a right margin short of the screen's edge is written out as a cursor at the
/// margin with no wrap pending, so the next character overwrites the last where it would have begun a
/// line. **When the first assertion of this fails the core holds it**, and `Terminal::writes_out_whole`
/// and what asks it can go. Until then a recording takes its keyframe once the next thing written has
/// ended, and a replay from it is what the session showed.
#[test]
fn a_wrap_pending_at_a_right_margin_is_not_written_out_and_no_keyframe_is_taken_there() {
    // Margins at columns 3 and 12, and a line that ends exactly at the right one
    let at_the_margin = b"\x1b[?69h\x1b[3;12s\x1b[1;3Hten chars!";
    let mut live = terminal(COLUMNS, ROWS);
    live.feed(at_the_margin);
    assert!(live.cursor_pending_wrap().expect("the wrap reads"), "the line did not end at the margin");
    assert!(!live.writes_out_whole());
    let taken = live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    let mut back = terminal(COLUMNS, ROWS);
    back.restore(&taken).expect("a keyframe is put back");
    assert!(
        !back.cursor_pending_wrap().expect("the wrap reads"),
        "the core now writes out a wrap pending at a right margin: `writes_out_whole` can go"
    );
    // **A cursor saved in that state is put back without the wrap too, and a session cannot know**: nothing
    // asks the core about a saved cursor, so a keyframe taken while one is held is the terminal less
    // that. When this fails the core holds it
    let mut saved = terminal(COLUMNS, ROWS);
    saved.feed(at_the_margin);
    saved.feed(b"\x1b7\r\n");
    assert!(saved.writes_out_whole());
    let taken = saved.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    let mut back = terminal(COLUMNS, ROWS);
    back.restore(&taken).expect("a keyframe is put back");
    for terminal in [&mut saved, &mut back] {
        terminal.feed(b"\x1b8");
    }
    assert!(saved.cursor_pending_wrap().expect("the wrap reads"), "a saved cursor does not carry a pending wrap");
    assert!(
        !back.cursor_pending_wrap().expect("the wrap reads"),
        "the core now writes out a saved cursor's wrap pending at a right margin"
    );

    // At the screen's own edge the wrap is held, and that is written out whole
    let mut edge = terminal(COLUMNS, ROWS);
    edge.feed("x".repeat(usize::from(COLUMNS)).as_bytes());
    assert!(edge.cursor_pending_wrap().expect("the wrap reads") && edge.writes_out_whole());

    // A recording due a keyframe after every byte takes none while the wrap is pending
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let recorder = apack_terminal::recording::Recorder::start_with(&path, u64::MAX, 1).expect("the recording starts");
    let mut session = Screen::new(COLUMNS, ROWS);
    session.start_recording_with(recorder).expect("the first keyframe is written");
    let keyframes = |session: &mut Screen| {
        session.flush_recording();
        apack_terminal::recording::read(&path)
            .iter()
            .filter(|timed| matches!(timed.entry, Entry::Keyframe { .. }))
            .count()
    };
    session.feed(b"\x1b[?69h\x1b[3;12s\x1b[1;3H");
    let before = keyframes(&mut session);
    session.feed(b"ten chars!");
    assert_eq!(keyframes(&mut session), before, "a keyframe was taken with a wrap pending at the margin");
    session.feed(b"next");
    assert_eq!(keyframes(&mut session), before + 1, "no keyframe once the line had wrapped");

    session.flush_recording();
    let entries = apack_terminal::recording::read(&path);
    let last = entries.iter().rposition(|timed| matches!(timed.entry, Entry::Keyframe { .. })).expect("a keyframe");
    let mut replayed = Screen::new(COLUMNS, ROWS);
    assert!(replayed.replay_all(&entries[last..]));
    session.feed(b" and on");
    replayed.feed(b" and on");
    assert_eq!(showing(&mut replayed), showing(&mut session));
}

/// **A cursor a program asked for is kept, whichever default the terminal put back is under.** Telling
/// the core a cursor's default is the one setting that touches a terminal's state, in two ways, and each
/// was a cursor drawn wrong after a replay. Told the default, the core draws the default cursor where it
/// holds the cursor to be the default one — which a program that asked for a cursor on one screen and the
/// default on the other left it holding, on both — so a terminal put back under the default it was
/// written out under is not told it again. And told whether the default blinks, the core sets whether the
/// cursor blinks now, so under another default that is read first and put back.
#[test]
fn a_cursor_a_program_asked_for_is_kept_whichever_default_it_is_put_back_under() {
    // A blinking block on the shell's screen, the default asked for on the other, and back
    let mut live = terminal(COLUMNS, ROWS);
    live.feed(b"\x1b[1 q\x1b[?1047h\x1b[0 q\x1b[?1049l");
    assert_eq!(seen(&live).cursor.map(|cursor| cursor.shape), Some(0));
    let taken = live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    let mut back = terminal(COLUMNS, ROWS);
    back.restore(&taken).expect("a keyframe is put back");
    assert_eq!(seen(&back), seen(&live), "under the default it was written out under");

    // A steady cursor asked for by mode 12, put back under a default of another shape that blinks
    let mut live = terminal(COLUMNS, ROWS);
    live.feed(b"\x1b[?12l");
    let taken = live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    let mut back = terminal(COLUMNS, ROWS);
    back.set_cursor_defaults(CursorStyle::Underline, CursorBlink::Program).expect("the cursor's defaults are set");
    back.restore(&taken).expect("a keyframe is put back");
    assert_eq!(back.mode(12).ok(), Some(false), "a cursor a program made steady blinks under another default");
    // And the default it is under is the one a reset returns to
    back.feed(b"\x1b[0 q");
    assert_eq!(seen(&back).cursor.map(|cursor| (cursor.shape, cursor.blinking)), Some((2, true)));
}

/// A terminal with `lines` of history above its screen, each line its own number.
fn with_history(columns: u16, rows: u16, lines: usize, line: impl Fn(usize) -> String) -> Terminal {
    let mut terminal = terminal(columns, rows);
    let mut printed = String::new();
    for number in 0..lines {
        printed.push_str(&line(number));
        printed.push_str("\r\n");
    }
    terminal.feed(printed.as_bytes());
    terminal
}

/// The history rows and screen rows of `terminal`, newest last.
fn rows_of(terminal: &Terminal) -> Vec<RowRead> {
    seen(terminal).rows
}

/// **A keyframe of a long history is the terminal's state and its newest history, and no more.** Twenty
/// thousand lines are megabytes written out whole; a keyframe of them holds the screen and the thousand
/// lines above it — or as many more as end where the core's own pages do — and is a fraction of the size.
/// What it holds is the newest, and the state is all there.
#[test]
fn a_keyframe_of_a_long_history_holds_the_newest_of_it_and_all_of_the_terminals_state() {
    let mut live = with_history(80, 24, 20_000, |number| format!("line {number} of the history"));
    live.feed(b"\x1b[2;20r\x1b[5;9H\x1b7\x1b[3 q\x1b[10;1Hon screen");
    let taken = live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    assert!(taken.len() < KEYFRAME_BYTES / 8, "{} bytes for a screen and a thousand lines", taken.len());

    let mut back = terminal(80, 24);
    back.restore(&taken).expect("a keyframe is put back");
    let (kept, all) = (rows_of(&back), rows_of(&live));
    let history = kept.len() - 24;
    assert!(history >= KEYFRAME_HISTORY_LINES as usize, "{history} lines of history");
    assert!(history < 3 * KEYFRAME_HISTORY_LINES as usize, "{history} lines of history, of {}", all.len() - 24);
    assert_eq!(kept[..], all[all.len() - kept.len()..], "the newest lines, as they were");

    // The state no row shows, by what is written next
    for terminal in [&mut live, &mut back] {
        terminal.feed(b"\x1b8saved\x1b[20;1H\r\nscrolled in the margins");
    }
    let (kept, all) = (seen(&back), seen(&live));
    assert_eq!(kept.cursor, all.cursor);
    assert_eq!(kept.rows[kept.rows.len() - 24..], all.rows[all.rows.len() - 24..]);

    // Asked for no history, it is the screen and what the core writes out with it
    let screen_only = live.keyframe(0, KEYFRAME_BYTES).expect("the terminal is written out");
    assert!(screen_only.len() < taken.len());
    let mut back = terminal(80, 24);
    back.restore(&screen_only).expect("a keyframe is put back");
    assert_eq!(rows_of(&back)[rows_of(&back).len() - 24..], all.rows[all.rows.len() - 24..]);
}

/// **A keyframe that a smaller cut does not hold is cut larger.** The cuts tried are a sixty-fourth and an
/// eighth of the most a keyframe may be, neither of which a wide screen's thousand lines fit: the keyframe
/// is then taken with the whole allowance, and holds them. One of short lines is held by the smallest, and
/// is a thousand lines all the same. One that does not fit the allowance at all is not a keyframe.
#[test]
fn a_keyframe_is_cut_larger_where_the_smaller_cut_does_not_hold_it() {
    let wide = |number: usize| format!("{number:06} {}", "wide ".repeat(70));
    let live = with_history(360, 40, 4_000, wide);
    let most = 2 << 20;
    let taken = live.keyframe(KEYFRAME_HISTORY_LINES, most).expect("the terminal is written out");
    assert!(taken.len() > most / 8, "{} bytes fitted the smaller cut, so the larger was not needed", taken.len());
    let mut back = terminal(360, 40);
    back.restore(&taken).expect("a keyframe is put back");
    let history = rows_of(&back).len() - 40;
    assert!(history >= KEYFRAME_HISTORY_LINES as usize, "{history} lines of history");
    assert!(history < 4_000, "the whole history was written out");

    // With room for less than those thousand lines, it holds the newest that fit, and all of the screen
    let room = taken.len() * 2 / 3;
    let short = live.keyframe(KEYFRAME_HISTORY_LINES, room).expect("the terminal is written out");
    assert!(short.len() <= room + 6, "{} bytes in room for {room}", short.len());
    let mut back = terminal(360, 40);
    back.restore(&short).expect("a keyframe is put back");
    let (kept, all) = (rows_of(&back), rows_of(&live));
    assert!(kept.len() - 40 < KEYFRAME_HISTORY_LINES as usize, "{} lines of history", kept.len() - 40);
    assert!(kept.len() > 40, "no history at all in two thirds of the room a thousand lines take");
    assert_eq!(kept[..], all[all.len() - kept.len()..]);

    // And with no room for the screen itself, there is no keyframe
    assert!(live.keyframe(KEYFRAME_HISTORY_LINES, 4_096).is_err());

    // Short lines: the smallest cut holds far more than a thousand of them, and a thousand are kept
    let short_lines = with_history(80, 24, 30_000, |number| format!("{number}"));
    let taken = short_lines.keyframe(KEYFRAME_HISTORY_LINES, most).expect("the terminal is written out");
    assert!(taken.len() < most / 64, "{} bytes of short lines", taken.len());
    let mut back = terminal(80, 24);
    back.restore(&taken).expect("a keyframe is put back");
    let (kept, all) = (rows_of(&back), rows_of(&short_lines));
    let history = kept.len() - 24;
    assert!(
        (KEYFRAME_HISTORY_LINES as usize..3 * KEYFRAME_HISTORY_LINES as usize).contains(&history),
        "{history} lines"
    );
    assert_eq!(kept[..], all[all.len() - kept.len()..]);
}

/// **A terminal part way through a sequence is not written out.** The core writes out a parser that is
/// between sequences; a session takes its keyframe where a sequence ends, and is refused one anywhere
/// else rather than handed half of one.
#[test]
fn a_terminal_part_way_through_a_sequence_is_not_written_out() {
    let unfinished: [&[u8]; 5] = [b"\x1b", b"\x1b[3", b"\x1b]0;a title", b"\x1bP1$r", &"\u{4e2d}".as_bytes()[..1]];
    for unfinished in unfinished {
        let mut live = terminal(COLUMNS, ROWS);
        live.feed(b"text");
        live.feed(unfinished);
        assert!(live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).is_err(), "written out after {unfinished:?}");
    }
    let mut live = terminal(COLUMNS, ROWS);
    live.feed(b"text\x1b[3");
    live.feed(b"1m");
    assert!(live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).is_ok(), "refused once the sequence had ended");
}

/// **What is not a keyframe is refused, and the terminal is as it was.** Nothing, a recording's bytes from
/// some other format, a keyframe cut short, one with a byte changed, and one that says it holds more
/// history than it does: each leaves the terminal it was to replace exactly what it was.
#[test]
fn what_is_not_a_keyframe_is_refused_and_the_terminal_is_as_it_was() {
    let live = with_history(80, 24, 3_000, |number| format!("line {number}"));
    let taken = live.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    let records = u32::from_le_bytes(taken[..4].try_into().expect("four bytes"));
    assert!(records > 0, "a keyframe of three thousand lines holds no history beyond its screen's");

    let mut changed = taken.clone();
    let middle = changed.len() / 2;
    changed[middle] ^= 0x40;
    let mut says_more = taken.clone();
    says_more[..4].copy_from_slice(&(records + 1).to_le_bytes());
    let not_keyframes: [(&str, Vec<u8>); 7] = [
        ("nothing", Vec::new()),
        ("three bytes", vec![1, 0, 0]),
        ("the sequences that redraw a screen", b"\x00\x00\x00\x00\x1b[H\x1b[2J$ ".to_vec()),
        ("a keyframe cut short of its screen", taken[..400].to_vec()),
        ("a keyframe cut short of its history", taken[..taken.len() - 100].to_vec()),
        ("a keyframe with a byte changed", changed),
        ("a keyframe that says it holds more than it does", says_more),
    ];
    for (what, bytes) in not_keyframes {
        let mut held = terminal(COLUMNS, ROWS);
        held.feed(b"what it held\x1b[3 q");
        let before = seen(&held);
        assert!(held.restore(&bytes).is_err(), "{what} was put back");
        assert_eq!(seen(&held), before, "{what}");
        // And it is still a terminal
        held.feed(b" and more");
        assert_eq!(
            held.text_between((0, 0), (COLUMNS - 1, 0)).expect("the row reads").trim_end(),
            "what it held and more"
        );
    }
    // The whole of it is one
    let mut back = terminal(80, 24);
    back.restore(&taken).expect("a keyframe is put back");
}

/// What `screen` answered a program that asked `question`, since the last time it was asked.
fn answered(screen: &mut Screen, question: &str) -> Vec<String> {
    screen.feed(question.as_bytes());
    screen.take_replies()
}

/// A keyframe of a terminal nobody told anything, which is the core's own defaults under `drawn`.
fn untold(columns: u16, rows: u16, drawn: &[u8]) -> Entry {
    let mut terminal = Terminal::new(columns, rows).expect("a terminal");
    terminal.feed(drawn);
    let state = terminal.keyframe(KEYFRAME_HISTORY_LINES, KEYFRAME_BYTES).expect("the terminal is written out");
    Entry::Keyframe { cols: columns, rows, state }
}

/// **A terminal put back is under the settings of the session it is put back in.** A keyframe is another
/// terminal's state, here one that was told nothing at all; the session replaying it answers a program as
/// itself — it answers at all, in its own colours and by its own name — keeps the history it keeps,
/// refuses the images it refuses, and returns to its own cursor and its own modes on a reset.
#[test]
fn a_terminal_put_back_is_told_what_the_one_it_replaces_was_told() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let entries = [Timed { at: 1, entry: untold(40, 6, b"recorded") }];
    let mut screen = Screen::with_history(40, 6, 2_000);
    screen.set_cursor_defaults(CursorStyle::Underline, CursorBlink::Off).expect("the defaults are set");
    screen.set_colors(&Colors { foreground: [1, 2, 3], background: [4, 5, 6], cursor: None, ansi: Vec::new() });
    assert!(screen.replay(&entries, u64::MAX));
    assert_eq!(screen.snapshot(0).lines[0], "recorded");

    // Where its reports go: a question is answered, which no terminal that was told nothing does
    assert_eq!(answered(&mut screen, "\x1b[6n"), ["\x1b[1;9R"]);
    // Its colours and its name
    assert_eq!(answered(&mut screen, "\x1b]10;?\x07"), ["\x1b]10;rgb:0101/0202/0303\x07"]);
    let named = answered(&mut screen, "\x1bP+q544e\x1b\\");
    let name: String = apack_terminal::screen::TERMINFO_NAME.bytes().map(|byte| format!("{byte:02x}")).collect();
    assert!(named.iter().any(|reply| reply.to_lowercase().contains(&name)), "{named:?}");
    // The images it refuses: a program asking whether it may send one is not told yes
    let asked = answered(&mut screen, "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    assert!(!asked.iter().any(|reply| reply.contains("OK")), "{asked:?}");
    // The cursor a reset returns to, which is this session's and not the recording's
    screen.feed(b"\x1b[0 q");
    let cursor = first_frame(&mut screen).cursor.expect("a cursor");
    assert_eq!((cursor.shape, cursor.blinking), (2, false));
    // The mode a reset returns to: clustering, which the core leaves off and a session turns on
    screen.feed(format!("\x1bc{family}").as_bytes());
    assert_eq!(screen.snapshot(0).cursor_column, 2, "a family is one character after a reset");
    // The history it keeps
    let lines: String = (0..6_000).map(|number| format!("{number}\r\n")).collect();
    screen.feed(lines.as_bytes());
    assert!(screen.scrollback_rows() < 4_000, "{} lines kept by a session that keeps 2,000", screen.scrollback_rows());
}

/// **And what a program did to the terminal is not undone by telling it.** A mode a session turns on by
/// default and a program turned off is off in the terminal put back, where telling the core a mode's
/// default would turn it on again: a setting is what a reset returns to, and the state is the keyframe's.
#[test]
fn a_mode_a_program_changed_is_as_the_keyframe_holds_it_whatever_its_default() {
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let entries = [Timed { at: 1, entry: keyframe(40, 6, b"\x1b[?2027l") }];
    let mut screen = Screen::new(40, 6);
    assert!(screen.replay(&entries, u64::MAX));
    screen.feed(family.as_bytes());
    assert_eq!(
        screen.snapshot(0).cursor_column,
        6,
        "a family is three characters where a program turned clustering off"
    );
    screen.feed(format!("\x1bc{family}").as_bytes());
    assert_eq!(screen.snapshot(0).cursor_column, 2, "and one again after a reset");
}

/// **A view of a screen that is rebuilt is sent the screen it became**, each time: a rewound session is
/// rebuilt at every moment looked at, under a view that stays attached, and the terminal a replay puts in
/// place is another terminal than the one the view's lines were counted on.
#[test]
fn a_view_of_a_screen_that_is_rebuilt_is_sent_the_screen_it_became() {
    let entries = [
        Timed { at: 100, entry: keyframe(40, 6, b"") },
        Timed { at: 200, entry: Entry::Output(b"early\r\n".to_vec()) },
        Timed { at: 300, entry: Entry::Output(b"later\r\n".to_vec()) },
    ];
    let mut screen = Screen::new(40, 6);
    let (sink, incoming) = channel();
    let id = ViewId { connection: 1, channel: 1 };
    screen.attach_view(id, sink).expect("a view attaches");
    let drawn = |screen: &mut Screen| {
        let frames: Vec<Frame> = incoming
            .try_iter()
            .filter_map(|sent| match ToView::decode(body(&sent)).expect("a message decodes") {
                ToView::Frame(frame) => Some(frame),
                _ => None,
            })
            .collect();
        let last = frames.last().expect("the view is sent a frame").clone();
        screen.acknowledge(id, last.sequence).expect("an acknowledgement is taken");
        (
            last.epoch,
            last.block
                .rows
                .iter()
                .map(|row| row.glyphs.iter().map(|glyph| glyph.text.as_str()).collect())
                .collect::<Vec<String>>(),
        )
    };
    let (attached, _) = drawn(&mut screen);

    assert!(screen.replay(&entries, 300));
    screen.resize(40, 6, 0, 0);
    let (latest, rows) = drawn(&mut screen);
    assert_ne!(latest, attached, "the lines of another terminal were numbered on from this one's");
    assert_eq!(rows[..2], ["early", "later"]);

    assert!(screen.replay(&entries, 200));
    screen.resize(40, 6, 0, 0);
    let (earlier, rows) = drawn(&mut screen);
    assert_ne!(earlier, latest);
    assert_eq!(rows[..2], ["early", ""]);
}

/// **A recording begun with keyframes this core does not read is picked up from the first it does**, and
/// one that holds none is not picked up, with the screen as it was. A moment looked back at is replayed
/// from the keyframe before it and no other, so one this core does not read is not shown.
#[test]
fn a_recording_is_picked_up_from_the_first_keyframe_that_can_be_read() {
    let unread =
        || Entry::Keyframe { cols: 40, rows: 6, state: b"\x00\x00\x00\x00\x1b[H\x1b[2Jfrom another core".to_vec() };
    let entries = [
        Timed { at: 100, entry: unread() },
        Timed { at: 200, entry: Entry::Output(b"not shown\r\n".to_vec()) },
        Timed { at: 300, entry: keyframe(40, 6, b"read\r\n") },
        Timed { at: 400, entry: Entry::Output(b"and after".to_vec()) },
    ];
    let mut screen = Screen::new(40, 6);
    screen.feed(b"as it was");
    assert!(screen.replay_all(&entries));
    assert_eq!(screen.snapshot(0).lines[..2], ["read", "and after"]);

    for none in [&entries[..2], &[]] {
        let mut screen = Screen::new(40, 6);
        screen.feed(b"as it was");
        assert!(!screen.replay_all(none));
        assert_eq!(screen.snapshot(0).lines[0], "as it was");
    }
    let mut screen = Screen::new(40, 6);
    screen.feed(b"as it was");
    assert!(!screen.replay(&entries, 250), "a moment whose keyframe cannot be read was shown");
    assert_eq!(screen.snapshot(0).lines[0], "as it was");
    assert!(screen.replay(&entries, 400));
    assert_eq!(screen.snapshot(0).lines[..2], ["read", "and after"]);
}

/// **A replay that comes to a keyframe it cannot read, part way through a sequence, ends there.** The
/// recording stopped taking that sequence and took a keyframe where it ended; without the keyframe what
/// follows it would be read as the sequence's, so what the recording can show is what came before.
#[test]
fn a_replay_ends_at_a_keyframe_it_cannot_read_part_way_through_a_sequence() {
    let entries = [
        Timed { at: 100, entry: keyframe(40, 6, b"first") },
        Timed { at: 200, entry: Entry::Output(b"\r\nshown\x1b]0;a title that does not end".to_vec()) },
        Timed { at: 300, entry: Entry::Keyframe { cols: 40, rows: 6, state: b"not a keyframe".to_vec() } },
        Timed { at: 400, entry: Entry::Output(b"\x07\r\nnot shown".to_vec()) },
    ];
    let mut screen = Screen::new(40, 6);
    assert!(screen.replay_all(&entries));
    assert_eq!(screen.snapshot(0).lines[..3], ["first", "shown", ""]);
    // What is written to it afterwards is a new sequence's, not the rest of the one the replay ended in
    screen.feed(b"\x18\r\nnext");
    assert_eq!(screen.snapshot(0).lines[..3], ["first", "shown", "next"]);
}

/// **A session picked up from a recording starts with its own cursor.** A recording ends with whatever
/// cursor its last program asked for, which is the keyframe's and the replay's; the shell that starts next
/// asked for none, so it is given the session's.
#[test]
fn a_session_picked_up_from_a_recording_starts_with_its_own_cursor() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut first = Screen::new(40, 6);
    first.start_recording(&path).expect("the recording starts");
    first.feed(b"$ vim\r\n\x1b[2 q");
    first.stop_recording();
    drop(first);
    assert!(apack_terminal::recording::settled(&path));

    let mut next = Screen::new(40, 6);
    next.set_cursor_defaults(CursorStyle::Underline, CursorBlink::Program).expect("the defaults are set");
    assert!(next.resume_recording(&path).expect("the recording is picked up"));
    let cursor = first_frame(&mut next).cursor.expect("a cursor");
    assert_eq!((cursor.shape, cursor.blinking), (2, true), "the cursor the last program left");
    assert_eq!(next.snapshot(0).lines[0], "$ vim");
}
