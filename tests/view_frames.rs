//! What a view is sent, case by case.
//!
//! `view_model.rs` establishes that frames add up to the screen. These are the things that property cannot
//! see, because a view ends up holding the right rows either way: *when* a frame is sent, how little is in
//! it, and what a view is told besides rows.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use apack_terminal::screen::Screen;
use apack_terminal::view::publish::{FETCH_LINES, HOLD_LIMIT, ViewId};
use apack_terminal::view::wire::{Color, Frame, Glyph, Row, ToView, body};

struct View {
    id: ViewId,
    incoming: Receiver<Vec<u8>>,
}

impl View {
    fn attached(screen: &mut Screen, connection: u64, channel_number: u32) -> Self {
        let (sink, incoming) = channel();
        let id = ViewId { connection, channel: channel_number };
        screen.attach_view(id, sink).expect("a view attaches");
        Self { id, incoming }
    }

    fn messages(&self) -> Vec<ToView> {
        self.incoming.try_iter().map(|encoded| ToView::decode(body(&encoded)).expect("a message decodes")).collect()
    }

    /// The one frame waiting, and nothing else.
    fn frame(&self) -> Frame {
        match self.messages().as_slice() {
            [ToView::Frame(frame)] => frame.clone(),
            other => panic!("expected one frame, found {other:?}"),
        }
    }

    fn nothing(&self) {
        let waiting = self.messages();
        assert!(waiting.is_empty(), "expected nothing, found {waiting:?}");
    }
}

fn text(row: &Row) -> String {
    let mut text = String::new();
    for glyph in &row.glyphs {
        while text.chars().count() < usize::from(glyph.column) {
            text.push(' ');
        }
        text.push_str(&glyph.text);
    }
    text
}

fn lines(frame: &Frame) -> Vec<(u64, String)> {
    frame.block.rows.iter().map(|row| (row.line, text(row))).collect()
}

/// Feeds, and lets the publisher send what is due, as the session's reader does after every read.
fn write(screen: &mut Screen, bytes: &str) {
    screen.feed(bytes.as_bytes());
    screen.publish();
}

#[test]
fn a_view_is_sent_the_whole_screen_when_it_attaches() {
    let mut screen = Screen::new(20, 4);
    screen.feed(b"one\r\ntwo");
    let view = View::attached(&mut screen, 1, 7);
    let frame = view.frame();

    assert_eq!(frame.channel, 7);
    assert_eq!((frame.columns, frame.screen_rows, frame.scrollback, frame.first_active), (20, 4, 0, 0));
    assert_eq!(
        lines(&frame),
        vec![(0, "one".to_owned()), (1, "two".to_owned()), (2, String::new()), (3, String::new())]
    );
    let cursor = frame.cursor.expect("the cursor is showing");
    assert_eq!((cursor.column, cursor.row), (3, 1));
    assert!(!frame.alternate_screen && !frame.mouse && frame.stale_from.is_none());
}

/// **One frame in flight.** Whatever a program writes while a view is drawing costs that view nothing, and
/// what it is sent next is everything since as one change.
#[test]
fn nothing_more_is_sent_until_a_view_says_it_drew_the_last() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();

    write(&mut screen, "a");
    write(&mut screen, "b");
    write(&mut screen, "c");
    view.nothing();

    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    let second = view.frame();
    assert_eq!(lines(&second), vec![(0, "abc".to_owned())], "only the row that changed, as it ended up");
    assert!(second.sequence > first.sequence);
}

#[test]
fn an_acknowledgement_with_nothing_new_sends_nothing() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    view.nothing();
    // And a write once it is idle goes at once
    write(&mut screen, "x");
    assert_eq!(lines(&view.frame()), vec![(0, "x".to_owned())]);
}

/// A late acknowledgement is for a frame since superseded: the one in flight has still to be drawn.
#[test]
fn an_acknowledgement_of_another_frame_releases_nothing() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    write(&mut screen, "x");
    screen.acknowledge(view.id, first.sequence + 40).expect("an acknowledgement is taken");
    view.nothing();
    screen.acknowledge(ViewId { connection: 9, channel: 1 }, first.sequence).expect("an acknowledgement is taken");
    view.nothing();
}

/// **A line is usually written once more before it scrolls** — the one being typed on always is — so the
/// lines that left the screen come with the frame, as they ended up.
#[test]
fn a_few_lines_that_scrolled_off_come_as_they_ended_up() {
    let mut screen = Screen::new(20, 3);
    write(&mut screen, "one\r\ntwo\r\nthr");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();

    write(&mut screen, "ee\r\nfour\r\nfive");
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    let second = view.frame();

    assert_eq!((second.first_active, second.scrollback), (2, 2));
    assert_eq!(second.stale_from, None);
    assert_eq!(
        lines(&second),
        vec![
            (0, "one".to_owned()),
            (1, "two".to_owned()),
            (2, "three".to_owned()),
            (3, "four".to_owned()),
            (4, "five".to_owned())
        ]
    );
}

/// A view showing the screen has no use for the thousand lines that passed between two of its frames, so
/// it is told which of its lines to let go rather than sent them.
#[test]
fn a_flood_between_two_frames_is_named_rather_than_sent() {
    let mut screen = Screen::new(20, 3);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();

    let flood: String = (0..500).map(|line| format!("line {line}\r\n")).collect();
    write(&mut screen, &flood);
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    let second = view.frame();

    assert_eq!(second.stale_from, Some(0));
    assert_eq!(second.first_active, 498);
    assert_eq!(
        lines(&second),
        vec![(498, "line 498".to_owned()), (499, "line 499".to_owned()), (500, String::new())],
        "the screen, and none of what scrolled past"
    );
}

/// **Synchronized output.** A program redrawing asks for the screen to be held so nobody sees it half
/// done, and a view is sent nothing until it lets go.
#[test]
fn a_program_holding_the_screen_is_not_shown_half_drawn() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026hhalf");
    view.nothing();
    write(&mut screen, " done\x1b[?2026l");
    assert_eq!(lines(&view.frame()), vec![(0, "half done".to_owned())]);
}

/// A program that asks and never lets go is drawn anyway, once it has had the time the limit allows.
#[test]
fn a_hold_nobody_ends_is_drawn_after_the_limit() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026hstuck");
    view.nothing();
    std::thread::sleep(HOLD_LIMIT + Duration::from_millis(50));
    write(&mut screen, "!");
    assert_eq!(lines(&view.frame()), vec![(0, "stuck!".to_owned())]);
}

/// Whatever makes every line number meaningless starts a new epoch, and the frame that says so carries
/// the whole screen: a view forgets its rows on seeing it.
#[test]
fn a_resize_and_a_switch_of_screens_each_start_a_new_epoch() {
    let mut screen = Screen::new(20, 4);
    write(&mut screen, "kept");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();

    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    screen.resize(10, 3, 0, 0);
    screen.publish();
    let resized = view.frame();
    assert_ne!(resized.epoch, first.epoch);
    assert_eq!((resized.columns, resized.screen_rows), (10, 3));
    assert_eq!(resized.block.rows.len(), 3, "the whole screen");

    screen.acknowledge(view.id, resized.sequence).expect("an acknowledgement is taken");
    write(&mut screen, "\x1b[?1049h");
    let alternate = view.frame();
    assert_ne!(alternate.epoch, resized.epoch);
    assert!(alternate.alternate_screen);
    assert_eq!(alternate.block.rows.len(), 3);

    screen.acknowledge(view.id, alternate.sequence).expect("an acknowledgement is taken");
    write(&mut screen, "\x1b[?1049l");
    let back = view.frame();
    assert_ne!(back.epoch, alternate.epoch);
    assert!(!back.alternate_screen);
    assert_eq!(text(&back.block.rows[0]), "kept");
}

#[test]
fn a_frame_says_how_the_cursor_is_drawn_and_whether_the_program_took_the_mouse() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let mut last = view.frame();
    assert_eq!(
        last.cursor.map(|cursor| (cursor.shape, cursor.blinking)),
        Some((1, true)),
        "a blinking bar until a program says otherwise"
    );

    for (written, check) in [
        (
            "\x1b[2 q",
            Box::new(|frame: &Frame| frame.cursor.is_some_and(|cursor| cursor.shape == 0 && !cursor.blinking))
                as Box<dyn Fn(&Frame) -> bool>,
        ),
        // Asking for the default again is asking for the bar
        ("\x1b[0 q", Box::new(|frame: &Frame| frame.cursor.is_some_and(|cursor| cursor.shape == 1 && cursor.blinking))),
        (
            "\x1b[4 q",
            Box::new(|frame: &Frame| frame.cursor.is_some_and(|cursor| cursor.shape == 2 && !cursor.blinking)),
        ),
        ("\x1b[?25l", Box::new(|frame: &Frame| frame.cursor.is_none())),
        ("\x1b[?25h\x1b[?1000h", Box::new(|frame: &Frame| frame.cursor.is_some() && frame.mouse)),
        ("\x1b[?1000l", Box::new(|frame: &Frame| !frame.mouse)),
    ] {
        screen.acknowledge(view.id, last.sequence).expect("an acknowledgement is taken");
        write(&mut screen, written);
        last = view.frame();
        assert!(check(&last), "after {written:?}: {last:?}");
        assert!(last.block.rows.is_empty(), "no row changed, so none is sent");
    }
}

#[test]
fn a_row_carries_each_characters_style_and_link() {
    let mut screen = Screen::new(30, 3);
    screen.feed(
        b"plain \x1b[1;31mred\x1b[0m \x1b[38;2;1;2;3;4mrgb\x1b[0m \x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
    );
    screen.feed("\r\n\u{4e16}e\u{301}\r\n\x1b[44m\x1b[K".as_bytes());
    let view = View::attached(&mut screen, 1, 1);
    let frame = view.frame();
    let block = &frame.block;
    let styled = |glyph: &Glyph| block.styles[usize::from(glyph.style)];
    let glyph_at = |row: usize, column: u16| {
        block.rows[row].glyphs.iter().find(|glyph| glyph.column == column).expect("a glyph at that column")
    };

    assert_eq!(text(&block.rows[0]), "plain red rgb link");
    assert_eq!(glyph_at(0, 0).style, 0, "an unstyled character uses the plain style");
    let red = styled(glyph_at(0, 6));
    assert_eq!((red.fg, red.flags & 1), (Color::Palette(1), 1), "bold, and the palette's red by index");
    let rgb = styled(glyph_at(0, 10));
    assert_eq!(rgb.fg, Color::Rgb(1, 2, 3));
    assert_eq!((rgb.flags >> 8) & 7, 1, "a single underline");
    assert_eq!(block.links[usize::from(glyph_at(0, 14).link)], "https://example.com");
    assert_eq!(glyph_at(0, 0).link, 0);

    let wide = glyph_at(1, 0);
    assert_eq!((wide.text.as_str(), wide.width), ("\u{4e16}", 2));
    let accented = glyph_at(1, 2);
    assert_eq!(
        (accented.text.as_str(), accented.width),
        ("e\u{301}", 1),
        "a character of two code points, after the wide one"
    );

    // A row erased with a colour set is cells of background and nothing else
    let bar = &block.rows[2];
    assert_eq!(bar.glyphs.len(), 30);
    assert!(bar.glyphs.iter().all(|glyph| glyph.text == " " && styled(glyph).bg == Color::Palette(4)));
}

#[test]
fn a_row_says_when_it_runs_on_and_when_the_shell_drew_its_prompt_there() {
    let mut screen = Screen::new(10, 4);
    screen.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07abcdefghijkl\r\n\x1b]133;C\x07out");
    let view = View::attached(&mut screen, 1, 1);
    let rows = view.frame().block.rows;
    assert_eq!((rows[0].wrapped, rows[0].prompt), (true, true));
    assert_eq!((rows[1].wrapped, rows[2].prompt), (false, false));
}

#[test]
fn a_view_that_detaches_is_sent_nothing_more() {
    let mut screen = Screen::new(20, 4);
    let leaving = View::attached(&mut screen, 1, 1);
    let staying = View::attached(&mut screen, 2, 1);
    let (first, other) = (leaving.frame(), staying.frame());
    screen.acknowledge(leaving.id, first.sequence).expect("an acknowledgement is taken");
    screen.acknowledge(staying.id, other.sequence).expect("an acknowledgement is taken");

    screen.detach_view(leaving.id);
    write(&mut screen, "x");
    leaving.nothing();
    let next = staying.frame();

    // And a connection going takes every view it had
    screen.acknowledge(staying.id, next.sequence).expect("an acknowledgement is taken");
    screen.detach_connection(2);
    write(&mut screen, "y");
    staying.nothing();
}

/// The session ending is not paced: a view is sent the screen as it ended whether or not it had drawn its
/// last frame, then told there is no more.
#[test]
fn a_session_ending_sends_the_last_screen_and_says_so() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 3);
    let _unacknowledged = view.frame();
    screen.feed(b"last words");
    screen.end_views();

    match view.messages().as_slice() {
        [ToView::Frame(frame), ToView::Ended { channel: 3 }] => {
            assert_eq!(lines(frame), vec![(0, "last words".to_owned())]);
        }
        other => panic!("expected the last frame and the end, found {other:?}"),
    }
    write(&mut screen, "after");
    view.nothing();
}

#[test]
fn a_view_whose_connection_is_gone_stops_being_sent_to() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    drop(view);
    write(&mut screen, "x");
    // Nothing to assert on a dropped channel but that writing on is fine, twice: the first finds it gone
    write(&mut screen, "y");
}

#[test]
fn lines_are_fetched_by_number_and_a_line_no_longer_held_is_absent() {
    let mut screen = Screen::new(20, 3);
    let written: String = (0..10).map(|line| format!("line {line}\r\n")).collect();
    screen.feed(written.as_bytes());

    let (epoch, block) = screen.fetch(2, 3).expect("a fetch reads");
    assert_eq!(epoch, screen.view_position().expect("a position reads").0);
    assert_eq!(
        block.rows.iter().map(|row| (row.line, text(row))).collect::<Vec<_>>(),
        vec![(2, "line 2".to_owned()), (3, "line 3".to_owned()), (4, "line 4".to_owned())]
    );

    // Erasing the history leaves only the screen, which is lines 8 to 10
    screen.feed(b"\x1b[3J");
    let (_, block) = screen.fetch(0, 20).expect("a fetch reads");
    assert_eq!(block.rows.iter().map(|row| row.line).collect::<Vec<_>>(), vec![8, 9, 10]);
}

#[test]
fn one_fetch_answers_with_no_more_than_its_limit() {
    let mut screen = Screen::new(8, 2);
    let written: String = (0..FETCH_LINES + 300).map(|line| format!("{line}\r\n")).collect();
    screen.feed(written.as_bytes());
    let first = screen.view_position().expect("a position reads").1 - u64::from(screen.scrollback_rows());
    let (_, block) = screen.fetch(first, u32::MAX).expect("a fetch reads");
    assert_eq!(block.rows.len(), FETCH_LINES as usize);
}

/// A shell's mark moves nothing on the screen, and is still something a view is told of: the frame's
/// command count is how it knows to ask which commands there are now.
#[test]
fn a_mark_that_draws_nothing_is_still_a_frame() {
    let mut screen = Screen::new(20, 4);
    write(&mut screen, "$ make\r\n");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    view.nothing();

    write(&mut screen, "\x1b]133;C\x07");
    let marked = view.frame();
    assert_ne!(marked.commands, first.commands);
    assert!(marked.block.rows.is_empty(), "no row changed, so none is sent");
}

/// Lines that scroll off a history already full leave it the same length, and when they are all alike
/// they leave the screen looking the same too. The line the screen starts on has still moved, which is
/// what a view numbers everything by.
#[test]
fn lines_alike_scrolling_through_a_full_history_still_move_the_first_line() {
    let mut screen = Screen::with_history(8, 3, 5);
    for _ in 0..400 {
        screen.feed(b"y\r\n");
    }
    let view = View::attached(&mut screen, 1, 1);
    let mut last = view.frame();
    for _ in 0..40 {
        screen.acknowledge(view.id, last.sequence).expect("an acknowledgement is taken");
        write(&mut screen, "y\r\n");
        let next = view.frame();
        assert_eq!(next.first_active, last.first_active + 1);
        last = next;
    }
}

/// One answer holds as many lines as were asked for when each brings a style of its own: the styles a
/// block can index run to tens of thousands, and a few hundred long rows are nowhere near that.
#[test]
fn a_fetch_of_rows_each_in_its_own_colour_is_answered_whole() {
    let mut screen = Screen::with_history(120, 2, 2_000);
    let written: String =
        (0..900u32).map(|line| format!("\x1b[38;2;{};{};7m{}\r\n", line / 256, line % 256, "x".repeat(110))).collect();
    screen.feed(written.as_bytes());
    let first = screen.view_position().expect("a position reads").1 - u64::from(screen.scrollback_rows());
    let (_, block) = screen.fetch(first, 900).expect("a fetch reads");
    assert_eq!(block.rows.len(), 900);
    assert!(block.styles.len() >= 900, "{} styles", block.styles.len());
}

/// A link is read whole however long it is, up to the length nothing real reaches.
#[test]
fn a_link_longer_than_the_first_read_allows_for_is_read_whole() {
    let address = format!("https://example.com/{}", "a".repeat(700));
    let mut screen = Screen::new(30, 3);
    screen.feed(format!("\x1b]8;;{address}\x1b\\here\x1b]8;;\x1b\\").as_bytes());
    let view = View::attached(&mut screen, 1, 1);
    let frame = view.frame();
    assert!(frame.block.links.contains(&address), "{:?}", frame.block.links);
}
