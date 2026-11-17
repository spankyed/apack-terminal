//! What a view is sent, case by case.
//!
//! `view_model.rs` establishes that frames add up to the screen. These are the things that property cannot
//! see, because a view ends up holding the right rows either way: *when* a frame is sent, how little is in
//! it, and what a view is told besides rows.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use apack_terminal::holds::{HOLD_LIMIT, Holds};
use apack_terminal::protocol::{CursorBlink, CursorStyle};
use apack_terminal::screen::{POSTPONED_BYTES, POSTPONED_READS, Screen, TEXT_BYTES, Unanswered};
use apack_terminal::view::publish::{FETCH_LINES, Publication, ViewId};
use apack_terminal::view::wire::{
    Color, ColorChanges, Frame, Glyph, MAX_MESSAGE_BYTES, Position, Row, Span, ToView, body, rows as rows_message,
};
use apack_terminal::vt::Terminal;
use apack_terminal::vt::input::Pointer;
use apack_terminal::vt::rows::ROW_LINK_BYTES;

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

/// The numbers a message says.
fn numbers_in(said: &str) -> Vec<u64> {
    said.split(|character: char| !character.is_ascii_digit()).filter_map(|digits| digits.parse().ok()).collect()
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

/// **A line that only moved is the row a view already has.** A screen that scrolls has every row somewhere
/// new, and each is still the line it was: the frame carries the line that left as it ended up and the
/// line that arrived, and none of the lines between.
#[test]
fn lines_that_only_moved_up_the_screen_are_not_sent_again() {
    let mut screen = Screen::new(20, 4);
    write(&mut screen, "one\r\ntwo\r\nthree\r\nfour");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\r\nfive");
    let second = view.frame();
    assert_eq!(second.first_active, 1);
    assert_eq!(lines(&second), vec![(0, "one".to_owned()), (4, "five".to_owned())]);
}

/// A program that draws a row again as it already stood has changed nothing, and a view is sent nothing:
/// a row is the bytes it is sent as, and those are the ones the view has.
#[test]
fn a_row_written_over_with_what_it_already_held_is_no_change() {
    let mut screen = Screen::new(20, 4);
    write(&mut screen, "status: ok");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    // Back to the start of the row and the same text over it, which leaves the cursor where it was too
    write(&mut screen, "\rstatus: ok");
    view.nothing();
    // And one character of it different is the row again
    write(&mut screen, "\rstatus: no");
    assert_eq!(lines(&view.frame()), vec![(0, "status: no".to_owned())]);
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

/// **A snapshot read during a hold is the screen from before it.** A view that asks for lines by number
/// while a program is redrawing gets what the program last let go of, not the half-drawn rows it is
/// writing: the same rule the frames follow.
#[test]
fn lines_fetched_while_the_program_is_holding_the_screen_are_those_from_before_the_hold() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"old text");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let epoch = first.epoch;
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026h\x1b[1;1Hnew text");
    let fetched = screen.fetch(epoch, 0, 1).expect("a fetch reads");
    let rows = fetched.rows.block().expect("the rows read back").rows.iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows, vec!["old text".to_owned()], "a fetch during a hold shows the screen before it");

    write(&mut screen, "\x1b[?2026l");
    let fetched = screen.fetch(epoch, 0, 1).expect("a fetch reads");
    let rows = fetched.rows.block().expect("the rows read back").rows.iter().map(text).collect::<Vec<_>>();
    assert_eq!(rows, vec!["new text".to_owned()], "and what the program let go of once it has");
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

    // Nothing more is written: what draws it is the limit passing, which the screen is told of by the
    // thread every session shares
    let screen = Arc::new(Mutex::new(screen));
    let holds = Holds::start();
    screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
    let began = Instant::now();
    screen.lock().expect("the screen").feed(b"\x1b[?2026l\x1b[?2026h and still");
    // Letting go in the middle of that drew what there was, and the hold taken again is a new one.
    // **Each frame is waited for, however long this thread takes to get to it**: what is asserted is the
    // order they come in and that the second came no sooner than the limit, neither of which a slow
    // machine can change
    let next = |what: &str| match ToView::decode(body(&view.incoming.recv_timeout(HOLD_LIMIT * 30).expect(what))) {
        Ok(ToView::Frame(frame)) => frame,
        other => panic!("expected a frame, found {other:?}"),
    };
    let let_go = next("the screen as it was let go");
    assert_eq!(lines(&let_go), vec![(0, "stuck".to_owned())]);
    screen.lock().expect("the screen").acknowledge(view.id, let_go.sequence).expect("an acknowledgement is taken");

    let drawn = next("the hold is ended for the program");
    assert_eq!(lines(&drawn), vec![(0, "stuck and still".to_owned())]);
    assert!(began.elapsed() >= HOLD_LIMIT, "drawn {:?} into a hold it had a second of", began.elapsed());
}

/// A view attached with its own end of the channel kept, so a test can send on it what a session would:
/// the answer to a question, in the order it falls among the frames.
fn attached_with_its_sink(screen: &mut Screen) -> (View, std::sync::mpsc::Sender<Vec<u8>>) {
    let (sink, incoming) = channel();
    let id = ViewId { connection: 1, channel: 1 };
    screen.attach_view(id, sink.clone()).expect("a view attaches");
    (View { id, incoming }, sink)
}

fn cell(line: u64, column: u16) -> Position {
    Position { line, column }
}

fn cells(start: (u64, u16), end: (u64, u16)) -> Span {
    Span { start: cell(start.0, start.1), end: cell(end.0, end.1) }
}

/// **What a view asks of the text waits out a hold.** A copy made while a program is part way through
/// redrawing is answered when the program lets go, with the text of the frame it let go of, and after
/// that frame: a view is never told the text of a screen it was not shown.
#[test]
fn the_text_a_view_asks_for_during_a_hold_is_answered_after_the_frame_the_program_lets_go_of() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"old text");
    let (view, sink) = attached_with_its_sink(&mut screen);
    let first = view.frame();
    let epoch = first.epoch;
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026h\x1b[1;1H\x1b[2Knew");
    screen.once_whole(view.id, move |screen| {
        let text = screen.text_of(epoch, cells((0, 0), (0, 9))).expect("the text reads");
        let told = ToView::Text { channel: 1, request: 7, epoch, text }.encode().expect("an answer encodes");
        sink.send(told).expect("the view is there");
    });
    view.nothing();

    write(&mut screen, "er text\x1b[?2026l");
    match view.messages().as_slice() {
        [ToView::Frame(frame), ToView::Text { request: 7, text, .. }] => {
            assert_eq!(lines(frame), vec![(0, "newer text".to_owned())]);
            assert_eq!(text, "newer text", "the text of the frame the view was just sent");
        }
        other => panic!("expected the frame and then the answer, found {other:?}"),
    }
}

/// **Every question about the text waits the same way**: the word at a cell, the line it is on and the
/// places a needle appears are each answered about the frame the hold ends with, none about the half of
/// it that was there when they were asked.
#[test]
fn a_word_a_line_and_a_search_asked_during_a_hold_are_answered_about_the_frame_it_ends_with() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"old text");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let epoch = first.epoch;
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026h\x1b[1;1H\x1b[2Kne");
    let (told, answers) = channel();
    let (word, line, found) = (told.clone(), told.clone(), told);
    screen.once_whole(view.id, move |screen| {
        word.send(format!("word {:?}", screen.word_at(epoch, cell(0, 0)).expect("a word reads"))).expect("heard");
    });
    screen.once_whole(view.id, move |screen| {
        line.send(format!("line {:?}", screen.line_at(epoch, cell(0, 0)).expect("a line reads"))).expect("heard");
    });
    screen.once_whole(view.id, move |screen| {
        found.send(format!("found {:?}", screen.find("text", false).expect("a search runs").spans)).expect("heard");
    });
    assert!(answers.try_recv().is_err(), "nothing is answered while the program holds the screen");

    write(&mut screen, "wer text\x1b[?2026l");
    assert_eq!(
        answers.try_iter().collect::<Vec<_>>(),
        vec![
            format!("word {:?}", Some(cells((0, 0), (0, 4)))),
            format!("line {:?}", Some(cells((0, 0), (0, 9)))),
            format!("found {:?}", vec![cells((0, 6), (0, 9))]),
        ],
        "each in the order it was asked, about `newer text`"
    );
}

/// A screen nobody is holding is whole already, so what is asked of it is answered there and then.
#[test]
fn what_is_asked_of_a_screen_nobody_is_holding_is_answered_at_once() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"plain text");
    let view = View::attached(&mut screen, 1, 1);
    let epoch = view.frame().epoch;

    let (told, answers) = channel();
    screen.once_whole(view.id, move |screen| told.send(screen.text_of(epoch, cells((0, 0), (0, 4)))).expect("heard"));
    assert_eq!(answers.try_recv().expect("answered at once").expect("the text reads"), "plain");
}

/// **A hold that runs out answers what waited for it**, as it draws what was withheld: a program that
/// takes the screen and never lets go keeps a view's questions no longer than it keeps its frames.
#[test]
fn a_hold_that_runs_out_answers_what_waited_for_it() {
    let mut screen = Screen::new(20, 3);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let epoch = first.epoch;
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    let screen = Arc::new(Mutex::new(screen));
    let holds = Holds::start();
    screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
    let (told, answers) = channel();
    {
        let mut screen = screen.lock().expect("the screen");
        screen.feed(b"\x1b[?2026hstuck");
        screen
            .once_whole(view.id, move |screen| told.send(screen.text_of(epoch, cells((0, 0), (0, 4)))).expect("heard"));
    }
    assert!(answers.try_recv().is_err(), "nothing is answered while the hold has time left");

    let answered = answers.recv_timeout(HOLD_LIMIT * 30).expect("the hold is ended for the program");
    assert_eq!(answered.expect("the text reads"), "stuck");
}

/// **A hold keeps a bounded number of questions waiting.** The one asked past that is answered from the
/// screen as it stands, and the ones that waited are still answered when the program lets go.
#[test]
fn a_question_past_what_a_hold_keeps_waiting_is_answered_from_the_screen_as_it_stands() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"old text");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let epoch = first.epoch;
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026h\x1b[1;1H\x1b[2Khalf");
    let (told, answers) = channel();
    for asked in 0..=POSTPONED_READS {
        let told = told.clone();
        screen.once_whole(view.id, move |screen| {
            told.send((asked, screen.text_of(epoch, cells((0, 0), (0, 9))).expect("the text reads"))).expect("heard");
        });
    }
    assert_eq!(
        answers.try_iter().collect::<Vec<_>>(),
        vec![(POSTPONED_READS, "half".to_owned())],
        "only the one past the limit is answered during the hold"
    );

    write(&mut screen, " whole\x1b[?2026l");
    let waited: Vec<_> = answers.try_iter().collect();
    assert_eq!(waited.len(), POSTPONED_READS);
    assert!(waited.iter().enumerate().all(|(nth, (asked, text))| *asked == nth && text == "half whole"), "{waited:?}");
}

/// **And a bounded number of bytes.** A question keeps what it was asked with while it waits, a search its
/// needle, and one that would take what waits past the limit is answered from the screen as it stands.
#[test]
fn a_question_that_would_keep_more_than_a_hold_keeps_waiting_is_answered_from_the_screen_as_it_stands() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"old text");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let epoch = first.epoch;
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026h\x1b[1;1H\x1b[2Khalf");
    let (told, answers) = channel();
    let ask = |screen: &mut Screen, asked: &'static str, bytes: usize| {
        let told = told.clone();
        screen.once_whole_keeping(view.id, bytes, move |screen| {
            told.send((asked, screen.text_of(epoch, cells((0, 0), (0, 9))).expect("the text reads"))).expect("heard");
        });
    };
    ask(&mut screen, "all a hold keeps", POSTPONED_BYTES);
    ask(&mut screen, "one byte more", 1);
    ask(&mut screen, "nothing more", 0);
    assert_eq!(
        answers.try_iter().collect::<Vec<_>>(),
        vec![("one byte more", "half".to_owned())],
        "only the one that would take the wait past the limit is answered during the hold"
    );

    write(&mut screen, " whole\x1b[?2026l");
    assert_eq!(
        answers.try_iter().collect::<Vec<_>>(),
        vec![("all a hold keeps", "half whole".to_owned()), ("nothing more", "half whole".to_owned())]
    );
}

/// **One session's hold does not wait on another session's work.** A screen that is busy when its hold
/// runs out — a long search has it — is come back to; a hold on another screen that runs out meanwhile is
/// ended on time, and the busy one's as soon as it is free.
#[test]
fn a_hold_is_ended_on_time_though_another_sessions_screen_is_busy() {
    let holds = Holds::start();
    let held = |written: &str| {
        let mut screen = Screen::new(20, 4);
        let view = View::attached(&mut screen, 1, 1);
        let first = view.frame();
        screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
        let screen = Arc::new(Mutex::new(screen));
        screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
        screen.lock().expect("the screen").feed(format!("\x1b[?2026h{written}").as_bytes());
        (screen, view)
    };
    let (busy, busy_view) = held("busy");
    let (free, free_view) = held("free");

    // The first screen is kept locked, as a long read of it would, until the second has been drawn
    let working = busy.lock().expect("the screen");
    let drawn = free_view.incoming.recv_timeout(HOLD_LIMIT * 30).expect("the other session's hold is ended");
    assert!(
        matches!(ToView::decode(body(&drawn)), Ok(ToView::Frame(frame)) if lines(&frame) == vec![(0, "free".to_owned())])
    );
    busy_view.nothing();
    drop(working);
    let drawn =
        busy_view.incoming.recv_timeout(HOLD_LIMIT * 30).expect("the busy session's hold is ended once it is free");
    assert!(
        matches!(ToView::decode(body(&drawn)), Ok(ToView::Frame(frame)) if lines(&frame) == vec![(0, "busy".to_owned())])
    );
    drop(free);
}

/// **A program taking and letting go of the screen as fast as it can write costs one deadline, and
/// nobody else's hold.** Each hold it takes is a new one with a second of its own; only the newest can
/// be in force, so only that one is kept time for — however many there have been — and a hold on another
/// session that runs out meanwhile is ended while the first is still at it.
#[test]
fn a_session_taking_the_screen_over_and_over_neither_piles_up_deadlines_nor_delays_anothers() {
    let holds = Holds::start();
    let held = |written: &str| {
        let mut screen = Screen::new(20, 4);
        let view = View::attached(&mut screen, 1, 1);
        let first = view.frame();
        screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
        let screen = Arc::new(Mutex::new(screen));
        screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
        screen.lock().expect("the screen").feed(format!("\x1b[?2026h{written}").as_bytes());
        (screen, view)
    };
    let (restless, _restless_view) = held("restless");
    let stop = Arc::new(AtomicBool::new(false));
    let toggling = std::thread::spawn({
        let (restless, holds, stop) = (Arc::clone(&restless), holds.clone(), Arc::clone(&stop));
        move || {
            // Lets go and takes it again, without pause, until told the other session has been drawn
            let (mut most, mut holds_taken) = (0, 0u64);
            while !stop.load(Ordering::SeqCst) {
                restless.lock().expect("the screen").feed(b"\x1b[?2026l\x1b[?2026h");
                holds_taken += 1;
                most = most.max(holds.outstanding());
            }
            (most, holds_taken)
        }
    });

    let (stuck, stuck_view) = held("stuck");
    let drawn = stuck_view.incoming.recv_timeout(HOLD_LIMIT * 30).expect("the other session's hold is ended");
    assert!(
        matches!(ToView::decode(body(&drawn)), Ok(ToView::Frame(frame)) if lines(&frame) == vec![(0, "stuck".to_owned())])
    );
    stop.store(true, Ordering::SeqCst);
    let (most, holds_taken) = toggling.join().expect("the restless session's writer ends");
    assert!(holds_taken > 100, "only {holds_taken} holds were taken: this was not a session at it without pause");
    // One for each of the two screens there are
    assert!(most <= 2, "{most} deadlines were outstanding for two screens, after {holds_taken} holds");
    drop(stuck);
}

/// **The frame is whole when the program lets go, so that is when it is sent** — by the write that let
/// go, not by whatever is written next: a program that redraws and then waits for a key is drawn.
#[test]
fn letting_go_of_the_screen_sends_it_without_anything_more_being_written() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    // Fed and nothing else: no reader publishing after the read, as there is none after the last read
    screen.feed(b"\x1b[?2026hwhole\x1b[?2026l");
    assert_eq!(lines(&view.frame()), vec![(0, "whole".to_owned())]);
}

/// A program that lets go and takes hold again in one read has finished one frame and begun the next:
/// the first is sent as it stood when it was let go, and none of the second with it.
#[test]
fn a_frame_let_go_and_another_begun_in_one_read_sends_the_first_alone() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");

    write(&mut screen, "\x1b[?2026hone\x1b[?2026l\x1b[?2026h two");
    let second = view.frame();
    assert_eq!(lines(&second), vec![(0, "one".to_owned())]);
    screen.acknowledge(view.id, second.sequence).expect("an acknowledgement is taken");
    view.nothing();
    write(&mut screen, "\x1b[?2026l");
    assert_eq!(lines(&view.frame()), vec![(0, "one two".to_owned())]);
}

/// A view that attaches while a program is holding the screen is showing nothing at all, so it is sent
/// the screen as it stands rather than made to wait for the program.
#[test]
fn a_view_attaching_while_the_screen_is_held_is_sent_it_as_it_stands() {
    let mut screen = Screen::new(20, 4);
    screen.feed(b"before\r\n\x1b[?2026hpart");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    assert_eq!(lines(&first)[..2], [(0, "before".to_owned()), (1, "part".to_owned())]);

    // And from there it waits like any other
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    write(&mut screen, " way");
    view.nothing();
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
    // Nothing is written after it: a resize is itself what a view is told of
    screen.resize(10, 3, 0, 0);
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
        // Hidden, and still where it is: an input method composes beside it whether or not it is drawn
        (
            "\x1b[?25l\x1b[3;8H",
            Box::new(|frame: &Frame| {
                frame.cursor.is_some_and(|cursor| cursor.hidden && (cursor.column, cursor.row) == (7, 2))
            }),
        ),
        // And followed while it is: a program that draws its own cursor moves this one with it
        (
            "\x1b[2;3H",
            Box::new(|frame: &Frame| {
                frame.cursor.is_some_and(|cursor| cursor.hidden && (cursor.column, cursor.row) == (2, 1))
            }),
        ),
        (
            "\x1b[?25h\x1b[?1000h",
            Box::new(|frame: &Frame| {
                frame.mouse
                    && frame.cursor.is_some_and(|cursor| !cursor.hidden && (cursor.column, cursor.row) == (2, 1))
            }),
        ),
        ("\x1b[?1000l", Box::new(|frame: &Frame| !frame.mouse)),
    ] {
        screen.acknowledge(view.id, last.sequence).expect("an acknowledgement is taken");
        write(&mut screen, written);
        last = view.frame();
        assert!(check(&last), "after {written:?}: {last:?}");
        assert!(last.block.rows.is_empty(), "no row changed, so none is sent");
    }
}

/// What a frame says the cursor is drawn as: its shape, and whether it blinks.
fn drawn_as(frame: &Frame) -> Option<(u8, bool)> {
    frame.cursor.map(|cursor| (cursor.shape, cursor.blinking))
}

/// Applies `change` to the screen, and returns the frame a view is sent for it.
fn after(screen: &mut Screen, view: &View, last: &Frame, change: impl FnOnce(&mut Screen)) -> Frame {
    screen.acknowledge(view.id, last.sequence).expect("an acknowledgement is taken");
    change(screen);
    view.frame()
}

#[test]
fn a_default_cursor_set_by_the_app_is_the_one_a_frame_shows() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    assert_eq!(drawn_as(&first), Some((1, true)), "the blinking bar a session starts with");

    let block = after(&mut screen, &view, &first, |screen| {
        screen.set_cursor_defaults(CursorStyle::Block, CursorBlink::Off).expect("the defaults are set")
    });
    assert_eq!(drawn_as(&block), Some((0, false)));

    let underline = after(&mut screen, &view, &block, |screen| {
        screen.set_cursor_defaults(CursorStyle::Underline, CursorBlink::On).expect("the defaults are set")
    });
    assert_eq!(drawn_as(&underline), Some((2, true)));
}

#[test]
fn a_program_choice_wins_over_the_default_until_the_program_resets_it() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let set = after(&mut screen, &view, &first, |screen| {
        screen.set_cursor_defaults(CursorStyle::Block, CursorBlink::Off).expect("the defaults are set")
    });

    let chosen = after(&mut screen, &view, &set, |screen| write(screen, "\x1b[5 q"));
    assert_eq!(drawn_as(&chosen), Some((1, true)), "DECSCUSR wins over the default");

    let reset = after(&mut screen, &view, &chosen, |screen| write(screen, "\x1b[0 q"));
    assert_eq!(drawn_as(&reset), Some((0, false)), "a reset returns to the default");
}

#[test]
fn a_reset_returns_to_a_blinking_cursor_for_on_and_program_and_a_steady_one_for_off() {
    for (blink, blinking) in [(CursorBlink::On, true), (CursorBlink::Program, true), (CursorBlink::Off, false)] {
        let mut screen = Screen::new(20, 4);
        let view = View::attached(&mut screen, 1, 1);
        let first = view.frame();
        let set = after(&mut screen, &view, &first, |screen| {
            screen.set_cursor_defaults(CursorStyle::Block, blink).expect("the defaults are set")
        });
        let chosen = after(&mut screen, &view, &set, |screen| write(screen, "\x1b[6 q"));
        let reset = after(&mut screen, &view, &chosen, |screen| write(screen, "\x1b[0 q"));
        assert_eq!(drawn_as(&reset), Some((0, blinking)), "after a reset with {blink:?}");
    }
}

/// **A default set part way through a program's sequence takes the cursor, and leaves the sequence whole**:
/// taking the cursor is a write to the parser, which waits for the sequence to end — made in the middle of
/// it, the write would end it early and the rest would be printed. It is taken where the sequence ends, so a
/// cursor the program chooses after that is the one drawn. A program that had chosen a cursor of its own is
/// the one the write is for, so each case begins by choosing one; and each is run in a session that takes
/// images by name as well, whose output is fed another way.
#[test]
fn a_default_set_part_way_through_a_program_sequence_takes_the_cursor_once_it_ends() {
    // What a steady underline, chosen by a program, is drawn as
    let chosen = {
        let mut screen = Screen::new(20, 4);
        screen.feed(b"\x1b[4 q");
        drawn_as(&View::attached(&mut screen, 1, 1).frame())
    };
    for images in [false, true] {
        for (begun, rest) in [("\x1b[", "1mbold"), ("\x1b]0;a ti", "tle\x07bold"), ("\x1b_Gq=2", "\x1b\\bold")] {
            for (then, drawn) in [("", Some((0, false))), ("\x1b[4 q", chosen)] {
                let mut screen = Screen::new(20, 4);
                if images {
                    screen.allow_images(true).expect("images are allowed");
                }
                screen.feed(b"\x1b[5 q");
                let view = View::attached(&mut screen, 1, 1);
                let first = view.frame();
                screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
                write(&mut screen, begun);
                screen.set_cursor_defaults(CursorStyle::Block, CursorBlink::Off).expect("the defaults are set");
                // Nothing the program sees has changed while its sequence is unfinished, nor once more of it
                // has come that does not finish it
                view.nothing();
                write(&mut screen, &rest[..1]);
                view.nothing();
                write(&mut screen, &format!("{}{then}", &rest[1..]));
                let ended = view.frame();
                let case = format!("{begun:?} then {then:?}, images {images}");
                assert_eq!(drawn_as(&ended), drawn, "{case}: the cursor was not taken where the sequence ended");
                assert_eq!(screen.snapshot(0).lines[0], "bold", "{case}: the sequence was cut short");
            }
        }
    }
}

/// DECSCUSR's blink and DEC mode 12 are one setting in the core: each sets what the other reads. That is
/// what a session shows while nobody has said whether its cursor blinks, which is how one starts.
#[test]
fn decscusr_and_dec_mode_12_set_one_blink_between_them() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    let mut last = view.frame();
    for (written, expected) in
        [("\x1b[2 q", (0, false)), ("\x1b[?12h", (0, true)), ("\x1b[5 q", (1, true)), ("\x1b[?12l", (1, false))]
    {
        last = after(&mut screen, &view, &last, |screen| write(screen, written));
        assert_eq!(drawn_as(&last), Some(expected), "after {written:?}");
    }
}

/// A screen whose person has said whether the cursor blinks, with a view of it and the frame that says so.
/// Run once as a session is by default and once as one that takes images by name, whose output is fed
/// another way.
fn with_a_blink_chosen(blink: CursorBlink, images: bool) -> (Screen, View, Frame) {
    let mut screen = Screen::new(20, 4);
    if images {
        screen.allow_images(true).expect("images are allowed");
    }
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    let set = after(&mut screen, &view, &first, |screen| {
        screen.set_cursor_defaults(CursorStyle::Block, blink).expect("the defaults are set")
    });
    (screen, view, set)
}

/// **A blink a person chose is not a program's mode to change.** DEC mode 12 is what `cnorm` and `cvvis`
/// carry, so a program that only shows the cursor again on its way out would turn the blink off with it,
/// and one that makes it very visible would turn it on. Once a person has said, mode 12 leaves the blink
/// as it was — set, reset or restored, alone or among other modes, which are still set.
#[test]
fn dec_mode_12_leaves_the_blink_a_person_chose() {
    for images in [false, true] {
        for (blink, blinking, against) in [(CursorBlink::On, true, 'l'), (CursorBlink::Off, false, 'h')] {
            let (mut screen, view, set) = with_a_blink_chosen(blink, images);
            assert_eq!(drawn_as(&set), Some((0, blinking)));
            let case = format!("{blink:?}, images {images}");

            // Each is followed by a letter, so there is a frame to read the cursor from
            let alone = after(&mut screen, &view, &set, |screen| write(screen, &format!("\x1b[?12{against}a")));
            assert_eq!(drawn_as(&alone), Some((0, blinking)), "{case}: mode 12 alone");

            // The mouse is taken by the same sequence, and that much of it is done
            let among = after(&mut screen, &view, &alone, |screen| write(screen, "\x1b[?12;1000hb\x1b[?1000;12lc"));
            assert_eq!(drawn_as(&among), Some((0, blinking)), "{case}: mode 12 among others");
            let mouse = after(&mut screen, &view, &among, |screen| write(screen, "\x1b[?12;1000hd"));
            assert!(mouse.mouse, "{case}: the other mode of the sequence was not set");
            assert_eq!(drawn_as(&mouse), Some((0, blinking)), "{case}: mode 12 among others");

            // Saved as it is and restored after a program changed it, which changes nothing either
            let restored = after(&mut screen, &view, &mouse, |screen| write(screen, "\x1b[?12s\x1b[?12re"));
            assert_eq!(drawn_as(&restored), Some((0, blinking)), "{case}: mode 12 saved and restored");
        }
    }
}

/// **A program asking for a cursor is still given the one it asks for**, blink and all, whatever a person
/// chose: that is a program saying what its cursor is, as an editor does for each of its modes, where mode
/// 12 is a side effect of showing one. A reset returns to what the person chose.
#[test]
fn a_program_asking_for_a_cursor_is_given_its_blink_whatever_a_person_chose() {
    for images in [false, true] {
        for (blink, blinking, asked, given) in
            [(CursorBlink::On, true, "\x1b[2 q", (0, false)), (CursorBlink::Off, false, "\x1b[5 q", (1, true))]
        {
            let (mut screen, view, set) = with_a_blink_chosen(blink, images);
            let case = format!("{blink:?}, images {images}");
            let chosen = after(&mut screen, &view, &set, |screen| write(screen, asked));
            assert_eq!(drawn_as(&chosen), Some(given), "{case}: the cursor the program asked for");

            // And mode 12 does not undo what the program asked for, any more than what the person chose
            let kept = after(&mut screen, &view, &chosen, |screen| {
                write(screen, if given.1 { "\x1b[?12la" } else { "\x1b[?12ha" });
            });
            assert_eq!(drawn_as(&kept), Some(given), "{case}: mode 12 after the program's cursor");

            let reset = after(&mut screen, &view, &kept, |screen| write(screen, "\x1b[0 q"));
            assert_eq!(drawn_as(&reset), Some((0, blinking)), "{case}: a reset returns to the person's");
        }
    }
}

/// **A sequence is what the parser says it is, however it arrives.** One split across reads at every byte
/// is the same sequence; one cut short by an escape never happened, and the cursor asked for by what cut
/// it short is given; and the same characters with no escape before them are text.
#[test]
fn a_blink_is_kept_or_given_whatever_shape_the_bytes_come_in() {
    for images in [false, true] {
        let case = format!("images {images}");
        let (mut screen, view, set) = with_a_blink_chosen(CursorBlink::Off, images);

        let split = after(&mut screen, &view, &set, |screen| {
            for piece in ["\x1b", "[", "?", "1", "2", "h", "a"] {
                write(screen, piece);
            }
        });
        assert_eq!(drawn_as(&split), Some((0, false)), "{case}: mode 12 a byte at a time");

        let asked = after(&mut screen, &view, &split, |screen| {
            for piece in ["\x1b", "[", "5", " ", "q"] {
                write(screen, piece);
            }
        });
        assert_eq!(drawn_as(&asked), Some((1, true)), "{case}: a cursor asked for a byte at a time");

        let reset = after(&mut screen, &view, &asked, |screen| write(screen, "\x1b[0 q"));
        assert_eq!(drawn_as(&reset), Some((0, false)));
        let cut_short = after(&mut screen, &view, &reset, |screen| write(screen, "\x1b[?12\x1b[5 q"));
        assert_eq!(drawn_as(&cut_short), Some((1, true)), "{case}: the cursor asked for by what cut a sequence short");

        let reset = after(&mut screen, &view, &cut_short, |screen| write(screen, "\x1b[0 q\r\x1b[2K"));
        let text = after(&mut screen, &view, &reset, |screen| write(screen, "[?12h"));
        assert_eq!(drawn_as(&text), Some((0, false)), "{case}");
        assert_eq!(lines(&text), vec![(0, "[?12h".to_owned())], "{case}: with no escape before them they are text");
    }
}

/// **What a watched sequence stopped at is what its caller is told, and no more.** A session asks the parse
/// to stop where it is between sequences, to take the cursor to a new default there; a watched sequence ends
/// at exactly such a place, so the stop that is the watch's own must be told apart from the one asked for.
/// Here a default is set part way through a sequence that names mode 12: the cursor is taken where it ends,
/// in the frame that shows what was written after it, and that text is whole. In a session that takes images
/// by name the parse also stops where a string begins, and one said to have begun when none did would have
/// the text read as its body.
#[test]
fn a_default_set_part_way_through_a_watched_sequence_takes_the_cursor_where_it_ends() {
    for images in [false, true] {
        let case = format!("images {images}");
        let (mut screen, view, set) = with_a_blink_chosen(CursorBlink::Off, images);
        // A program that has chosen a cursor of its own is the one a new default waits for
        let chosen = after(&mut screen, &view, &set, |screen| write(screen, "\x1b[5 q"));
        assert_eq!(drawn_as(&chosen), Some((1, true)));
        screen.acknowledge(view.id, chosen.sequence).expect("an acknowledgement is taken");

        write(&mut screen, "\x1b[?1");
        screen.set_cursor_defaults(CursorStyle::Underline, CursorBlink::Off).expect("the defaults are set");
        view.nothing();
        write(&mut screen, "2hbold");
        let ended = view.frame();
        assert_eq!(drawn_as(&ended), Some((2, false)), "{case}: the cursor was not taken where the sequence ended");
        assert_eq!(lines(&ended), vec![(0, "bold".to_owned())], "{case}: what followed the sequence");
    }
}

/// **A string that begins is not the parser between sequences.** In a session that takes images by name the
/// parse stops where a string begins, and one waiting to take the cursor also stops where the parser is
/// between sequences: the two are told apart, or the cursor is taken inside the string, where the write
/// that takes it is read as part of the string and lost. Here the default is set part way through a
/// sequence, which an image command then cuts short, with a blink chosen so the parse goes by the watch.
#[test]
fn a_default_waiting_for_the_end_of_a_sequence_is_not_taken_where_a_string_begins() {
    for images in [false, true] {
        let case = format!("images {images}");
        let (mut screen, view, set) = with_a_blink_chosen(CursorBlink::Off, images);
        let chosen = after(&mut screen, &view, &set, |screen| write(screen, "\x1b[5 q"));
        assert_eq!(drawn_as(&chosen), Some((1, true)));
        screen.acknowledge(view.id, chosen.sequence).expect("an acknowledgement is taken");

        write(&mut screen, "\x1b[");
        screen.set_cursor_defaults(CursorStyle::Underline, CursorBlink::Off).expect("the defaults are set");
        view.nothing();
        write(&mut screen, "\x1b_Gq=2\x1b\\bold");
        let ended = view.frame();
        assert_eq!(drawn_as(&ended), Some((2, false)), "{case}: the cursor was not taken once the string had ended");
        assert_eq!(lines(&ended), vec![(0, "bold".to_owned())], "{case}: what followed the string");
    }
}

/// **A string that ends is said to have ended, and nothing is said to have begun.** With a blink chosen the
/// parse goes by way of the watch for every byte, watched or not, and what it stopped at is passed on as it
/// was: a title's end is the end of a string, after which a session that takes images by name reads text
/// as text.
#[test]
fn text_after_a_title_is_text_in_a_session_with_a_blink_chosen() {
    for images in [false, true] {
        let (mut screen, view, set) = with_a_blink_chosen(CursorBlink::On, images);
        let titled =
            after(&mut screen, &view, &set, |screen| write(screen, "\x1b]0;a title\x07text\x1b]0;another\x1b\\ more"));
        assert_eq!(lines(&titled), vec![(0, "text more".to_owned())], "images {images}");
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
    let glyph_at = |row: usize, column: u16| {
        block.rows[row].glyphs.iter().find(|glyph| glyph.column == column).expect("a glyph at that column")
    };
    let styled = |row: usize, glyph: &Glyph| block.rows[row].style_of(glyph);

    assert_eq!(text(&block.rows[0]), "plain red rgb link");
    assert_eq!(glyph_at(0, 0).style, 0, "an unstyled character uses the plain style");
    let red = styled(0, glyph_at(0, 6));
    assert_eq!((red.fg, red.flags & 1), (Color::Palette(1), 1), "bold, and the palette's red by index");
    let rgb = styled(0, glyph_at(0, 10));
    assert_eq!(rgb.fg, Color::Rgb(1, 2, 3));
    assert_eq!((rgb.flags >> 8) & 7, 1, "a single underline");
    assert_eq!(block.rows[0].link_of(glyph_at(0, 14)), Some("https://example.com"));
    assert_eq!(block.rows[0].link_of(glyph_at(0, 0)), None);
    // A row's tables are its own: the two styles and the one link on this row, and nothing on the next
    assert_eq!((block.rows[0].styles.len(), block.rows[0].links.len()), (2, 1));
    assert_eq!((block.rows[1].styles.len(), block.rows[1].links.len()), (0, 0));

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
    assert!(bar.glyphs.iter().all(|glyph| glyph.text == " " && styled(2, glyph).bg == Color::Palette(4)));
}

/// **A hyperlink covers the cells written inside its sequence and no others.** The cell after the close is
/// unlinked. An id parameter does not change the URI reported: the core's only hyperlink accessor answers
/// the URI, and no id, so grouping by id is not something a row can carry.
#[test]
fn a_hyperlink_covers_its_cells_until_it_is_closed_and_an_id_does_not_change_the_uri() {
    let mut screen = Screen::new(30, 2);
    screen.feed(b"\x1b]8;id=one;https://example.com\x1b\\in\x1b]8;;\x1b\\out");
    let view = View::attached(&mut screen, 1, 1);
    let frame = view.frame();
    let row = &frame.block.rows[0];
    let glyph_at =
        |column: u16| row.glyphs.iter().find(|glyph| glyph.column == column).expect("a glyph at that column");

    assert_eq!(row.link_of(glyph_at(0)), Some("https://example.com"), "a cell inside the sequence, with an id on it");
    assert_eq!(row.link_of(glyph_at(1)), Some("https://example.com"));
    assert_eq!(row.link_of(glyph_at(2)), None, "the cell after the close");
}

/// **A hyperlink that wraps reports its URI on each row it covers**, the same string both times, so a view
/// that groups cells by URI can see that they are one link across the wrap.
#[test]
fn a_hyperlink_that_wraps_reports_its_uri_on_each_row_it_covers() {
    let mut screen = Screen::new(10, 3);
    screen.feed(b"\x1b]8;;https://example.com/long\x1b\\abcdefghijkl\x1b]8;;\x1b\\");
    let view = View::attached(&mut screen, 1, 1);
    let frame = view.frame();
    let rows = &frame.block.rows;
    let last_of_first = rows[0].glyphs.iter().find(|glyph| glyph.column == 9).expect("the last cell of the first row");
    let first_of_second =
        rows[1].glyphs.iter().find(|glyph| glyph.column == 0).expect("the first cell of the second row");

    assert!(rows[0].wrapped, "the first row wraps onto the second");
    assert_eq!(rows[0].link_of(last_of_first), Some("https://example.com/long"));
    assert_eq!(rows[1].link_of(first_of_second), Some("https://example.com/long"));
}

/// A colour named exactly rather than by the palette is a cell of background all the same, and a wide
/// character drawn in a colour is one character: the cell beside it that it also covers is not a second.
#[test]
fn a_background_named_exactly_and_a_wide_character_in_colour_are_each_what_they_are() {
    let mut screen = Screen::new(12, 3);
    screen.feed("\x1b[48;2;9;8;7m\x1b[K\x1b[0m\r\n\x1b[41m\u{4e16}\u{754c}\x1b[0m".as_bytes());
    let view = View::attached(&mut screen, 1, 1);
    let rows = view.frame().block.rows;

    assert_eq!(rows[0].glyphs.len(), 12);
    assert!(rows[0].glyphs.iter().all(|glyph| glyph.text == " " && rows[0].style_of(glyph).bg == Color::Rgb(9, 8, 7)));

    let wide: Vec<(u16, u8, &str)> =
        rows[1].glyphs.iter().map(|glyph| (glyph.column, glyph.width, glyph.text.as_str())).collect();
    assert_eq!(wide, vec![(0, 2, "\u{4e16}"), (2, 2, "\u{754c}")]);
    assert!(rows[1].glyphs.iter().all(|glyph| rows[1].style_of(glyph).bg == Color::Palette(1)));
}

/// **A history that is full is trimmed a stretch at a time**, so as lines arrive the count of lines held
/// climbs, drops, and climbs again. Exactly one such stretch of lines all alike, between two frames,
/// leaves a view nothing to tell the two apart by — the same rows on the screen, the same cursor, the
/// same count of history — and the line the screen starts on has moved by all of them. That is still a
/// change, and the view is told.
#[test]
fn a_stretch_of_like_lines_that_leaves_the_history_as_long_as_it_was_still_moves_the_first_line() {
    // Wide, so a stretch is a few hundred lines and a small part of the history: the line the screen
    // started on is still held when a whole stretch has gone by
    let mut screen = Screen::with_history(200, 3, 5);
    // Until the history has been trimmed twice, which is how long one stretch is
    let mut trimmed: Vec<u64> = Vec::new();
    let mut fed: u64 = 0;
    while trimmed.len() < 2 {
        let before = screen.scrollback_rows();
        screen.feed(b"y\r\n");
        fed += 1;
        if screen.scrollback_rows() < before {
            trimmed.push(fed);
        }
        assert!(fed < 200_000, "the history was never trimmed, so this case shows nothing");
    }
    let stretch = trimmed[1] - trimmed[0];

    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    for _ in 0..stretch {
        screen.feed(b"y\r\n");
    }
    assert_eq!(screen.scrollback_rows(), first.scrollback, "not a whole stretch, so this case shows nothing");
    screen.publish();
    let next = view.frame();
    assert_eq!(next.epoch, first.epoch);
    assert_eq!(next.first_active, first.first_active + stretch);
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

/// **The buttons a view holds down go when it does, and no other view's do.** A pointer that has left
/// the grid is reported — at the edge — only for a view holding a button down, so that is what shows who
/// is holding one: a view that lost focus or went holds none when its pointer next moves, and a view on
/// another connection is still dragging after the first connection has gone.
#[test]
fn the_buttons_a_view_held_go_when_it_does_and_no_other_views_do() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?1002h\x1b[?1006h");
    let (leaving, staying) = (ViewId { connection: 1, channel: 1 }, ViewId { connection: 2, channel: 1 });
    // The left button over cells ten pixels by twenty: `0` presses and `2` moves
    let told = |screen: &mut Screen, view: ViewId, action: u8, x: f32, row: u8| {
        let y = f32::from(row) * 20.0 + 10.0;
        let pointer = Pointer { action, button: 0, modifiers: 0, x, y, cell_width: 10.0, cell_height: 20.0 };
        String::from_utf8(screen.pointer(view, &pointer).expect("a pointer event encodes")).expect("a report is text")
    };
    // In the second column, and past the last: eighty columns of ten pixels end at 800
    let (inside, outside) = (15.0, 900.0);

    // **Every move here is on a row whose last cell has not been reported**, since a move within the cell
    // the program was last told of is reported to nobody, held or not
    assert_eq!(told(&mut screen, leaving, 0, inside, 1), "\x1b[<0;2;2M");
    assert_eq!(told(&mut screen, leaving, 2, outside, 1), "\x1b[<32;80;2M", "a drag, while it holds the button");
    // It loses focus with the button down: the release goes somewhere else
    screen.release_pointer(leaving);
    assert_eq!(told(&mut screen, leaving, 2, outside, 2), "", "a view that lost focus holds nothing");

    // It presses again and detaches without releasing, then comes back
    assert_eq!(told(&mut screen, leaving, 0, inside, 3), "\x1b[<0;2;4M");
    screen.detach_view(leaving);
    assert_eq!(told(&mut screen, leaving, 2, outside, 3), "", "a view that went holds nothing");

    // Both press, and the first one's connection goes
    assert_eq!(told(&mut screen, leaving, 0, inside, 5), "\x1b[<0;2;6M");
    assert_eq!(told(&mut screen, staying, 0, inside, 7), "\x1b[<0;2;8M");
    screen.detach_connection(1);
    assert_eq!(told(&mut screen, staying, 2, outside, 7), "\x1b[<32;80;8M", "the other connection's view still drags");
    assert_eq!(told(&mut screen, leaving, 2, outside, 9), "", "a view whose connection went holds nothing");
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
    let _view = View::attached(&mut screen, 1, 1);
    let epoch = screen.view_position().expect("a position reads").0;

    let fetched = screen.fetch(epoch, 2, 3).expect("a fetch reads");
    assert_eq!((fetched.first, fetched.count), (2, 3));
    assert_eq!(
        fetched
            .rows
            .block()
            .expect("the rows read back")
            .rows
            .iter()
            .map(|row| (row.line, text(row)))
            .collect::<Vec<_>>(),
        vec![(2, "line 2".to_owned()), (3, "line 3".to_owned()), (4, "line 4".to_owned())]
    );

    // Erasing the history leaves only the screen, which is lines 8 to 10. **The answer says it covers all
    // twenty lines asked for**, so the seventeen it has no row for are known to be lines not held — eight
    // that are gone and nine not written yet — rather than lines still to be asked for
    screen.feed(b"\x1b[3J");
    let fetched = screen.fetch(epoch, 0, 20).expect("a fetch reads");
    assert_eq!((fetched.first, fetched.count), (0, 20));
    let block = fetched.rows.block().expect("the rows read back");
    assert_eq!(block.rows.iter().map(|row| row.line).collect::<Vec<_>>(), vec![8, 9, 10]);
}

/// **A line has a place in the terminal only while the terminal holds it**: from the oldest line of the
/// history to the last row of the screen. A line that has left the history has none, and nor has the line
/// under the screen, which nothing has written yet.
#[test]
fn a_line_has_a_place_in_the_terminal_from_the_oldest_held_to_the_last_row_of_the_screen() {
    let mut terminal = Terminal::new(20, 3).expect("a terminal");
    terminal.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
    let mut numbering = Publication::new(&terminal, 0).expect("a session's lines are numbered");
    numbering.advance(&terminal).expect("the numbering is brought up to the terminal");
    // Two lines of history over a screen of three, so lines 0 to 4
    assert_eq!(numbering.first_active(), 2);
    assert_eq!((numbering.screen_row(0), numbering.screen_row(4)), (Some(0), Some(4)));
    assert_eq!(numbering.screen_row(5), None, "the line under the screen is not written yet");

    // The history erased: the screen's lines keep their numbers, and are now the terminal's first rows
    terminal.feed(b"\x1b[3J");
    numbering.advance(&terminal).expect("the numbering is brought up to the terminal");
    assert_eq!(numbering.screen_row(1), None, "a line that is gone");
    assert_eq!((numbering.screen_row(2), numbering.screen_row(4)), (Some(0), Some(2)));
    assert_eq!(numbering.screen_row(5), None, "and the line under the screen is still not written");
}

/// Line numbers belong to what a view is told, so a question about lines put to a session nothing views
/// is refused: nothing is set up to answer it, and nothing is left behind by its having been asked.
#[test]
fn a_question_about_lines_is_refused_while_nothing_views_the_session() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"one\r\ntwo");
    let at = Position { line: 0, column: 0 };
    assert_eq!(screen.fetch(0, 0, 3).err(), Some(Unanswered::Unviewed));
    assert_eq!(screen.word_at(0, at).err(), Some(Unanswered::Unviewed));
    assert_eq!(screen.view_position().err(), Some(Unanswered::Unviewed));

    // And once the last view has gone it is so again, with the next view's lines in a numbering of
    // their own: a view that comes back must not take rows it kept to still be good
    let view = View::attached(&mut screen, 1, 1);
    let before = view.frame().epoch;
    screen.detach_view(view.id);
    assert_eq!(screen.view_position().err(), Some(Unanswered::Unviewed));
    let again = View::attached(&mut screen, 1, 1);
    assert_ne!(again.frame().epoch, before);
}

/// **A question names lines in the numbering it was asked in.** Once the lines have been numbered again
/// — a resize reflowed them — the same numbers are other lines, so the question is refused rather than
/// answered about whatever they now are.
#[test]
fn a_question_asked_in_a_numbering_that_is_over_is_refused() {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"alpha beta\r\ngamma");
    let view = View::attached(&mut screen, 1, 1);
    let asked = view.frame().epoch;
    let at = Position { line: 0, column: 1 };
    let all = Span { start: at, end: Position { line: 1, column: 4 } };
    assert_eq!(screen.text_of(asked, all).as_deref(), Ok("lpha beta\ngamma"));

    screen.resize(8, 3, 0, 0);
    let now = screen.view_position().expect("a position reads").0;
    assert_ne!(now, asked);
    let renumbered = Some(Unanswered::Renumbered { asked, now });
    assert_eq!(screen.text_of(asked, all).err(), renumbered);
    assert_eq!(screen.word_at(asked, at).err(), renumbered);
    assert_eq!(screen.line_at(asked, at).err(), renumbered);
    assert_eq!(screen.fetch(asked, 0, 3).err(), renumbered);
    // The refusal is read by whoever asked, and says both numberings: the one to stop asking in, and
    // the one to ask in
    let said = screen.fetch(asked, 0, 3).expect_err("a refusal").to_string();
    assert!(numbers_in(&said).contains(&u64::from(asked)) && numbers_in(&said).contains(&u64::from(now)), "{said:?}");
    // Asked again in the numbering in use, it is answered
    assert!(screen.word_at(now, at).is_ok());
}

#[test]
fn one_fetch_answers_with_no_more_than_its_limit() {
    let mut screen = Screen::new(8, 2);
    let written: String = (0..FETCH_LINES + 300).map(|line| format!("{line}\r\n")).collect();
    screen.feed(written.as_bytes());
    let _view = View::attached(&mut screen, 1, 1);
    let (epoch, first_active) = screen.view_position().expect("a position reads");
    let first = first_active - u64::from(screen.scrollback_rows());
    let fetched = screen.fetch(epoch, first, u32::MAX).expect("a fetch reads");
    assert_eq!(fetched.rows.count(), FETCH_LINES);
    assert_eq!(fetched.count, FETCH_LINES, "and it says that is as far as it answers for");
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

/// One answer holds as many lines as were asked for when each brings a style of its own: a row carries
/// its own styles, so how many there are across an answer is no limit on it.
#[test]
fn a_fetch_of_rows_each_in_its_own_colour_is_answered_whole() {
    let mut screen = Screen::with_history(120, 2, 2_000);
    let written: String =
        (0..900u32).map(|line| format!("\x1b[38;2;{};{};7m{}\r\n", line / 256, line % 256, "x".repeat(110))).collect();
    screen.feed(written.as_bytes());
    let _view = View::attached(&mut screen, 1, 1);
    let (epoch, first_active) = screen.view_position().expect("a position reads");
    let first = first_active - u64::from(screen.scrollback_rows());
    let fetched = screen.fetch(epoch, first, 900).expect("a fetch reads");
    let block = fetched.rows.block().expect("the rows read back");
    assert_eq!((fetched.count, block.rows.len()), (900, 900));
    let colours: std::collections::HashSet<_> = block.rows.iter().map(|row| row.style_of(&row.glyphs[0]).fg).collect();
    assert_eq!(colours.len(), 900, "each row is drawn in the colour it was written in");
}

/// Rows that cost as much to send as a row can: every cell a character of sixteen code points, in three
/// colours no other cell on its row has.
fn costly_rows(rows: u32, columns: u32) -> String {
    let character = format!("a{}", "\u{301}".repeat(15));
    let mut written = String::new();
    for row in 0..rows {
        for column in 0..columns {
            let (r, g, b) = (row % 256, column % 256, column / 256);
            written.push_str(&format!("\x1b[38;2;{r};{g};{b};48;2;{g};{b};{r};58;2;{b};{r};{g}m{character}"));
        }
        written.push_str("\x1b[0m\r\n");
    }
    written
}

/// **No message is longer than a view reads.** A screen that would be is sent as far as one message
/// goes, in line order, and the rest in the frame after — which the view is owed whether or not anything
/// more is written.
#[test]
fn a_screen_longer_than_one_message_is_sent_in_as_many_frames_as_it_takes() {
    // Some fifty kilobytes a row, and two hundred of them: more than one message by a few megabytes
    let mut screen = Screen::new(1_000, 200);
    screen.feed(costly_rows(199, 1_000).as_bytes());
    let view = View::attached(&mut screen, 1, 1);

    let mut seen: Vec<u64> = Vec::new();
    let mut sequences: Vec<u64> = Vec::new();
    loop {
        let encoded: Vec<Vec<u8>> = view.incoming.try_iter().collect();
        let [encoded] = encoded.as_slice() else { break };
        assert!(encoded.len() <= MAX_MESSAGE_BYTES + 4, "a frame of {} bytes", encoded.len());
        let Ok(ToView::Frame(frame)) = ToView::decode(body(encoded)) else { panic!("expected a frame") };
        assert_eq!(frame.screen_rows, 200);
        seen.extend(frame.block.rows.iter().map(|row| row.line));
        sequences.push(frame.sequence);
        screen.acknowledge(view.id, frame.sequence).expect("an acknowledgement is taken");
    }
    assert!(sequences.len() > 1, "the screen fitted one frame, so this case shows nothing");
    assert!(sequences.is_sorted_by(|before, after| before < after), "each frame has a number of its own");
    assert_eq!(seen, (0..200).collect::<Vec<u64>>(), "every row once, in order");
}

/// **A session that ends is sent whole however long its screen is**: every frame it takes, one after
/// another with none of them waiting to be drawn, and the end said after the last.
#[test]
fn a_session_ending_with_a_screen_longer_than_one_message_is_sent_all_of_it_and_then_told() {
    let mut screen = Screen::new(1_000, 200);
    let view = View::attached(&mut screen, 1, 4);
    let _unacknowledged = view.frame();
    screen.feed(costly_rows(199, 1_000).as_bytes());
    screen.end_views();

    let messages = view.messages();
    let (ended, frames) = messages.split_last().expect("the view is told something");
    assert_eq!(ended, &ToView::Ended { channel: 4 });
    assert!(frames.len() > 1, "the screen fitted one frame, so this case shows nothing");
    let mut seen: Vec<u64> = Vec::new();
    for message in frames {
        let ToView::Frame(frame) = message else { panic!("expected a frame, found {message:?}") };
        seen.extend(frame.block.rows.iter().map(|row| row.line));
    }
    assert_eq!(seen, (0..199).collect::<Vec<u64>>(), "every row that was written, once, in order");
}

/// A fetch stops where its answer is full and says how far it got, so the lines past that are asked for
/// again rather than taken to be gone.
#[test]
fn a_fetch_longer_than_one_message_answers_as_far_as_it_fits_and_says_how_far() {
    let mut screen = Screen::with_history(1_000, 2, 2_000);
    screen.feed(costly_rows(200, 1_000).as_bytes());
    let _view = View::attached(&mut screen, 1, 1);
    let (epoch, first_active) = screen.view_position().expect("a position reads");
    let first = first_active - u64::from(screen.scrollback_rows());
    assert_eq!(first_active - first, 199, "the history was cut short, so this case shows less than it says");

    let fetched = screen.fetch(epoch, first, 900).expect("a fetch reads");
    assert!(fetched.count > 0 && fetched.count < 200, "it answered for {} lines", fetched.count);
    assert_eq!(fetched.rows.count(), fetched.count, "every line it answers for is held, and is there");
    let message = rows_message(1, 1, epoch, fetched.first, fetched.count, &fetched.rows).expect("it fits a message");
    assert!(message.len() <= MAX_MESSAGE_BYTES + 4, "an answer of {} bytes", message.len());

    // The next one carries on from where that one stopped
    let next = screen.fetch(epoch, first + u64::from(fetched.count), 900).expect("a fetch reads");
    let block = next.rows.block().expect("the rows read back");
    assert_eq!(block.rows.first().map(|row| row.line), Some(first + u64::from(fetched.count)));
}

/// A selection whose text is more than one answer carries is refused with its size, rather than sent as
/// a message a view would have to hang up on.
#[test]
fn text_longer_than_one_answer_is_refused() {
    let mut screen = Screen::with_history(1_000, 2, 12_000);
    let line = format!("{}\r\n", "y".repeat(999));
    for _ in 0..10_000 {
        screen.feed(line.as_bytes());
    }
    let _view = View::attached(&mut screen, 1, 1);
    let (epoch, first_active) = screen.view_position().expect("a position reads");
    let first = first_active - u64::from(screen.scrollback_rows());
    let all = Span { start: Position { line: first, column: 0 }, end: Position { line: first_active, column: 0 } };
    match screen.text_of(epoch, all) {
        Err(Unanswered::TooLong { bytes }) => {
            assert!(bytes > MAX_MESSAGE_BYTES, "{bytes} bytes");
            // Whoever asked is told how long it was and how long it may be
            let said = Unanswered::TooLong { bytes }.to_string();
            assert!(
                numbers_in(&said).contains(&(bytes as u64)) && numbers_in(&said).contains(&(TEXT_BYTES as u64)),
                "{said:?}"
            );
        }
        other => panic!("expected a refusal, found {:?}", other.map(|text| text.len())),
    }
    // A part of it that fits is answered: half a message of it, a thousand bytes a line
    let half = MAX_MESSAGE_BYTES / 2_000;
    let some = Span { start: all.start, end: Position { line: first + half as u64, column: 998 } };
    assert_eq!(screen.text_of(epoch, some).map(|text| text.len()), Ok((half + 1) * 1_000 - 1));

    // **As much as one answer carries, to the byte.** The longest text that is answered makes a message
    // a view reads, with everything else the answer says at its longest, and one byte more is refused
    let (lines, over) = (TEXT_BYTES / 1_000, TEXT_BYTES % 1_000);
    assert!(over > 0 && first_active - first > lines as u64, "no span here is exactly that long");
    let end = Position { line: first + lines as u64, column: over as u16 - 1 };
    let most = screen.text_of(epoch, Span { start: all.start, end }).expect("the longest text is answered");
    assert_eq!(most.len(), TEXT_BYTES);
    let answer = ToView::Text { channel: u32::MAX, request: u32::MAX, epoch: u32::MAX, text: most };
    assert!(answer.encode().is_ok(), "the longest text that is answered does not fit a message");
    let past = Span { start: all.start, end: Position { column: end.column + 1, ..end } };
    assert_eq!(screen.text_of(epoch, past).err(), Some(Unanswered::TooLong { bytes: TEXT_BYTES + 1 }));
}

/// A link is read whole however long it is, up to the length nothing real reaches.
#[test]
fn a_link_longer_than_the_first_read_allows_for_is_read_whole() {
    let address = format!("https://example.com/{}", "a".repeat(700));
    let mut screen = Screen::new(30, 3);
    screen.feed(format!("\x1b]8;;{address}\x1b\\here\x1b]8;;\x1b\\").as_bytes());
    let view = View::attached(&mut screen, 1, 1);
    let frame = view.frame();
    assert_eq!(frame.block.rows[0].links, vec![address]);
}

/// A link covers a run of cells, and a row says it once: each cell of the run names the one entry, and a
/// link that comes back later on the row is the entry it already was.
#[test]
fn a_link_is_held_once_a_row_however_many_cells_it_covers() {
    let link = |address: &str, text: &str| format!("\x1b]8;;{address}\x1b\\{text}\x1b]8;;\x1b\\");
    let mut screen = Screen::new(60, 3);
    let written = [
        link("https://a.test/", "aaaaaaaa"),
        " ".to_owned(),
        link("https://b.test/", "bbbb"),
        link("https://a.test/", "again"),
    ]
    .concat();
    screen.feed(written.as_bytes());
    let view = View::attached(&mut screen, 1, 1);
    let row = view.frame().block.rows.remove(0);
    assert_eq!(row.links, vec!["https://a.test/".to_owned(), "https://b.test/".to_owned()]);
    let linked: Vec<Option<&str>> = row.glyphs.iter().map(|glyph| row.link_of(glyph)).collect();
    let (a, b) = (Some("https://a.test/"), Some("https://b.test/"));
    assert_eq!(linked, [vec![a; 8], vec![None], vec![b; 4], vec![a; 5]].concat());
}

/// **A view that comes back is told to trust nothing it kept of the history.** While another view has
/// stayed, the lines are numbered as they were, and a line can have been written to again after the
/// returning view last saw it and before it scrolled off. So its first frame names every line of history
/// as stale — and the view that stayed is told nothing of the kind.
#[test]
fn a_view_that_comes_back_while_another_stayed_is_told_its_history_is_stale() {
    let mut screen = Screen::new(20, 3);
    let staying = View::attached(&mut screen, 1, 1);
    let returning = View::attached(&mut screen, 2, 1);
    let (stayed, first) = (staying.frame(), returning.frame());
    assert_eq!(first.stale_from, None, "with no history there is none to distrust");
    screen.acknowledge(staying.id, stayed.sequence).expect("an acknowledgement is taken");
    screen.acknowledge(returning.id, first.sequence).expect("an acknowledgement is taken");

    // It sees the line half written, and leaves
    write(&mut screen, "half");
    let seen = returning.frame();
    assert_eq!(lines(&seen), vec![(0, "half".to_owned())]);
    screen.detach_view(returning.id);

    // The line is finished and scrolls off, with the other view watching throughout
    write(&mut screen, " and whole\r\ntwo\r\nthree\r\nfour\r\nfive");
    let watched = staying.frame();
    assert_eq!(watched.stale_from, None);

    let returning = View::attached(&mut screen, 2, 1);
    let back = returning.frame();
    assert_eq!(back.epoch, first.epoch, "the view that stayed is in the numbering it was in");
    assert_eq!((back.first_active, back.scrollback), (2, 2));
    assert_eq!(back.stale_from, Some(0), "every line of history, from the oldest held");
    // What it then fetches is the line as it ended up
    let fetched = screen.fetch(back.epoch, 0, 1).expect("a fetch reads").rows.block().expect("the rows read back");
    assert_eq!(text(&fetched.rows[0]), "half and whole");
}

/// **A row is also what it is flagged as, and a flag can change with nothing written on the row.** The
/// character that wraps off a full row is written on the row below; the row it left now runs on, and is
/// sent again saying so. A shell's prompt mark prints nothing, and the row it marks is sent as a prompt's.
#[test]
fn a_row_whose_flags_changed_with_nothing_written_on_it_is_sent_again() {
    let mut screen = Screen::new(10, 4);
    let view = View::attached(&mut screen, 1, 1);
    let mut last = view.frame();
    let mut after = |screen: &mut Screen, written: &str| {
        screen.acknowledge(view.id, last.sequence).expect("an acknowledgement is taken");
        write(screen, written);
        last = view.frame();
        last.block.rows.clone()
    };

    // Exactly as wide as the grid: the row is full, and is not yet a row that runs on
    let rows = after(&mut screen, "0123456789");
    assert_eq!((rows[0].line, rows[0].wrapped), (0, false));
    // One more character, with no scroll: it lands on the second row, and the first now runs on
    let rows = after(&mut screen, "x");
    let first = rows.iter().find(|row| row.line == 0).expect("the row the character wrapped off is sent again");
    assert!(first.wrapped);
    assert_eq!(text(first), "0123456789");

    // A prompt mark on a row with nothing printed on it
    let rows = after(&mut screen, "\r\n\r\n");
    assert!(rows.iter().all(|row| !row.prompt));
    let rows = after(&mut screen, "\x1b]133;A\x07");
    let marked = rows.iter().find(|row| row.line == 3).expect("the row the shell marked is sent again");
    assert!(marked.prompt);
}

/// A session that is over has told its views so, and takes no more: a view that attached after would be
/// drawing a screen whose end nothing was left to tell it of.
#[test]
fn nothing_attaches_to_a_session_that_has_ended() {
    let mut screen = Screen::new(20, 4);
    let view = View::attached(&mut screen, 1, 1);
    view.frame();
    screen.end_views();
    let (sink, incoming) = channel();
    assert_eq!(screen.attach_view(ViewId { connection: 2, channel: 1 }, sink).err(), Some(Unanswered::Ended));
    assert!(incoming.try_recv().is_err(), "a view of a session that is over was sent something");
}

/// **A program's colours are told to a view, which draws in a theme of its own.** What a frame carries is
/// what the program changed from the colours the session was given — all of it, whenever any of it
/// changed and in a view's first frame — and whether it has the screen in reverse video.
#[test]
fn the_colours_a_program_changes_and_reverse_video_are_told() {
    let mut screen = Screen::new(20, 4);
    screen.set_colors(&apack_terminal::screen::Colors {
        foreground: [200, 200, 200],
        background: [10, 10, 10],
        cursor: Some([255, 255, 0]),
        ansi: vec![[1, 1, 1], [170, 0, 0]],
    });
    let view = View::attached(&mut screen, 1, 1);
    let mut last = view.frame();
    // The colours a session is given are the view's own theme: none of them is a change
    assert_eq!(last.colors, Some(ColorChanges::default()));
    assert!(!last.reverse_video);
    let mut after = |screen: &mut Screen, written: &str| {
        screen.acknowledge(view.id, last.sequence).expect("an acknowledgement is taken");
        write(screen, written);
        last = view.frame();
        last.clone()
    };

    // A background, as an editor sets one: a frame is due for it though no row changed
    let frame = after(&mut screen, "\x1b]11;rgb:00/2b/36\x07");
    let background = ColorChanges { background: Some([0x00, 0x2B, 0x36]), ..ColorChanges::default() };
    assert_eq!(frame.colors.as_ref(), Some(&background));
    assert!(frame.block.rows.is_empty());

    // Text, and nothing about colours: the view goes on with what it has
    assert_eq!(after(&mut screen, "plain").colors, None);

    // A foreground, a cursor and two palette entries, as a shell theme sets them: everything changed so
    // far is said again, so what a frame carries is the whole of it
    let frame = after(&mut screen, "\x1b]10;#eee8d5\x07\x1b]12;#dc322f\x07\x1b]4;1;#dc322f\x07\x1b]4;200;#010203\x07");
    let themed = ColorChanges {
        foreground: Some([0xEE, 0xE8, 0xD5]),
        background: Some([0x00, 0x2B, 0x36]),
        cursor: Some([0xDC, 0x32, 0x2F]),
        palette: vec![(1, [0xDC, 0x32, 0x2F]), (200, [0x01, 0x02, 0x03])],
    };
    assert_eq!(frame.colors.as_ref(), Some(&themed));

    // Reverse video is a mode, told beside the others, and is not a change of colour
    let frame = after(&mut screen, "\x1b[?5h");
    assert!(frame.reverse_video);
    assert_eq!(frame.colors, None);
    assert!(!after(&mut screen, "\x1b[?5l").reverse_video);

    // A view that arrives now is told all of it in its first frame
    let late = View::attached(&mut screen, 2, 1);
    assert_eq!(late.frame().colors.as_ref(), Some(&themed));

    // One colour put back, then the rest: each is a frame, and the last says nothing is changed
    let frame = after(&mut screen, "\x1b]111\x07");
    assert_eq!(frame.colors.as_ref(), Some(&ColorChanges { background: None, ..themed.clone() }));
    let frame = after(&mut screen, "\x1b]110\x07\x1b]112\x07\x1b]104\x07");
    assert_eq!(frame.colors, Some(ColorChanges::default()));
}

/// **A row's links have an end**, which is what keeps one row inside one message whatever a program
/// links: past a megabyte of addresses on a row, a cell is drawn with no link rather than the row being
/// more than can be sent.
#[test]
fn a_rows_links_come_to_no_more_than_a_row_may_carry() {
    let mut screen = Screen::new(1_000, 2);
    let mut written = String::new();
    for cell in 0..1_000 {
        written.push_str(&format!("\x1b]8;;https://example.test/{cell}/{}\x1b\\x", "a".repeat(1_990)));
    }
    screen.feed(written.as_bytes());
    let view = View::attached(&mut screen, 1, 1);
    let row = view.frame().block.rows.remove(0);
    let bytes: usize = row.links.iter().map(String::len).sum();
    assert!(bytes <= ROW_LINK_BYTES, "{bytes} bytes of links on one row");
    assert!(row.links.len() > 100, "{} links: too few for this case to have reached the limit", row.links.len());
    assert_eq!(row.glyphs.len(), 1_000, "a cell past the limit is still drawn");
    assert!(row.glyphs.iter().any(|glyph| glyph.link == 0), "every cell was linked, so the limit was not reached");
}

/// **A recording's keyframe says a cursor blinks by mode 12, and is believed.** A keyframe is the core's own
/// account of a screen, and a blinking cursor is written there as `CSI ? 12 h` whichever way its program
/// asked for it. So a session whose person chose a steady cursor still replays the blinking one a program
/// asked for, the program's own mode 12 in the output after the keyframe is still left alone, and a session
/// that leaves the blink to programs still has it changed by mode 12 once the replay is over.
#[test]
fn a_replay_draws_the_cursor_its_keyframe_says_whatever_blink_a_person_chose() {
    use apack_terminal::recording::{Entry, Timed};
    let entries = [
        Timed { at: 100, entry: Entry::Keyframe { cols: 20, rows: 4, redraw: b"hello\x1b[?12h".to_vec() } },
        Timed { at: 150, entry: Entry::Output(b"\x1b[?12l more".to_vec()) },
    ];
    let mut chosen = Screen::new(20, 4);
    chosen.set_cursor_defaults(CursorStyle::Block, CursorBlink::Off).expect("the defaults are set");
    assert!(chosen.replay(&entries, 200));
    let view = View::attached(&mut chosen, 1, 1);
    let replayed = view.frame();
    assert_eq!(drawn_as(&replayed), Some((0, true)), "the keyframe's blinking cursor");
    assert_eq!(lines(&replayed)[0], (0, "hello more".to_owned()));

    let mut left = Screen::new(20, 4);
    left.set_cursor_defaults(CursorStyle::Block, CursorBlink::Program).expect("the defaults are set");
    assert!(left.replay(&entries[..1], 200));
    let view = View::attached(&mut left, 1, 1);
    let replayed = view.frame();
    assert_eq!(drawn_as(&replayed), Some((0, true)));
    let steady = after(&mut left, &view, &replayed, |screen| write(screen, "\x1b[?12l"));
    assert_eq!(drawn_as(&steady), Some((0, false)), "mode 12 left alone in a session that leaves it to programs");
}

/// A screen held by its program, with a view of it that has drawn what came before, and the epoch its
/// lines are numbered in.
fn held_with_a_view(drawn: &str) -> (Screen, View, u32) {
    let mut screen = Screen::new(20, 3);
    screen.feed(b"old text");
    let view = View::attached(&mut screen, 1, 1);
    let first = view.frame();
    screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
    write(&mut screen, &format!("\x1b[?2026h\x1b[1;1H\x1b[2K{drawn}"));
    (screen, view, first.epoch)
}

/// **What a view asked during a hold is answered when the view goes, or its session ends**, from the
/// screen as it stands: a question is answered or refused and never dropped, whichever of them ends
/// first. A view of another connection that asked is left waiting for the hold.
#[test]
fn what_a_view_asked_during_a_hold_is_answered_when_it_goes_or_its_session_ends() {
    type Going = fn(&mut Screen, ViewId);
    let ways: [(&str, Going); 3] = [
        ("the view detaching", |screen, id| screen.detach_view(id)),
        ("its connection going", |screen, id| screen.detach_connection(id.connection)),
        ("the session ending", |screen, _| screen.end_views()),
    ];
    for (way, go) in ways {
        let (mut screen, view, epoch) = held_with_a_view("half");
        let other = View::attached(&mut screen, 2, 1);
        let (told, answers) = channel();
        for asking in [view.id, other.id] {
            let told = told.clone();
            screen.once_whole(asking, move |screen| {
                told.send((asking, screen.text_of(epoch, cells((0, 0), (0, 3))))).expect("heard");
            });
        }
        assert!(answers.try_recv().is_err(), "{way}: answered while the program held the screen");

        go(&mut screen, view.id);
        let answered: Vec<_> = answers.try_iter().collect();
        let ended = way == "the session ending";
        assert_eq!(answered.len(), if ended { 2 } else { 1 }, "{way}: {answered:?}");
        assert_eq!(answered[0], (view.id, Ok("half".to_owned())), "{way}");
        if !ended {
            write(&mut screen, " whole\x1b[?2026l");
            assert_eq!(answers.try_recv().expect("the other is answered as the hold ends").0, other.id, "{way}");
        }
    }
}

/// **One session's questions do not keep another's hold from ending.** Every session's hold is ended by
/// one thread, and what a view asked can be a search of a whole history: here a question that does not
/// finish until it is let, asked of one screen whose hold runs out, and a second screen whose hold must
/// still run out on time.
#[test]
fn a_question_that_waited_for_one_screens_hold_does_not_keep_anothers_from_ending() {
    let holds = Holds::start();
    let held = |text: &str| {
        let mut screen = Screen::new(20, 3);
        let view = View::attached(&mut screen, 1, 1);
        let first = view.frame();
        screen.acknowledge(view.id, first.sequence).expect("an acknowledgement is taken");
        let screen = Arc::new(Mutex::new(screen));
        screen.lock().expect("the screen").limit_holds(holds.for_screen(Arc::downgrade(&screen)));
        screen.lock().expect("the screen").feed(format!("\x1b[?2026h{text}").as_bytes());
        (screen, view)
    };
    let (slow, slow_view) = held("slow");
    let (begun, beginning) = channel();
    let (finish, finishing) = channel::<()>();
    slow.lock().expect("the screen").once_whole(slow_view.id, move |_| {
        begun.send(()).expect("heard");
        let _ = finishing.recv();
    });
    beginning.recv_timeout(HOLD_LIMIT * 30).expect("the question is asked once its hold runs out");

    // With that question still being answered, another screen's hold begins, and runs out
    let (other, other_view) = held("other");
    let drawn = other_view.incoming.recv_timeout(HOLD_LIMIT * 30).expect("the other screen's hold is ended");
    match ToView::decode(body(&drawn)).expect("a message decodes") {
        ToView::Frame(frame) => assert_eq!(lines(&frame)[0], (0, "other".to_owned())),
        other => panic!("expected the frame the hold kept back, found {other:?}"),
    }
    drop(finish);
    drop((slow, other));
}
