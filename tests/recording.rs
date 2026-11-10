//! A session's recording: that the terminal written out whole redraws as itself, that a replay holds what
//! the live screen held, that a recording stays inside its budget, and that nothing a replay provokes is
//! told a second time.

use apack_terminal::recording::{self, Entry, Recorder, Timed};
use apack_terminal::screen::Screen;
use apack_terminal::vt::rows::{RowRead, Space};

/// Every row a screen holds, history first, with where its cursor is.
fn held(screen: &Screen, rows: u16) -> (Vec<RowRead>, (u16, u16), bool) {
    let total = screen.scrollback_rows() + u32::from(rows);
    let all = (0..total).map(|row| screen.row(Space::Screen, row).expect("a row reads").unwrap_or_default()).collect();
    let snapshot = screen.snapshot(0);
    (all, (snapshot.cursor_row, snapshot.cursor_column), snapshot.alt_screen)
}

/// Output with what a keyframe has to carry: colour, a link, a line that wraps, wide characters, history
/// that has scrolled off, and a cursor left somewhere in particular.
fn busy(lines: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in 0..lines {
        bytes.extend_from_slice(
            format!(
                "\x1b[1;3{}mline {line}\x1b[0m \x1b]8;;https://example.test/{line}\x1b\\link\x1b]8;;\x1b\\ 日本語 {}\r\n",
                line % 8,
                "w".repeat(line % 70)
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(b"\x1b[48;2;10;20;30mprompt$ \x1b[0mtyped");
    bytes
}

#[test]
fn a_terminal_written_out_whole_redraws_as_itself() {
    let mut original = Screen::new(40, 10);
    original.feed(&busy(60));
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    original.start_recording(&path).expect("the recording starts");

    let entries = recording::read(&path);
    assert!(matches!(entries.as_slice(), [Timed { entry: Entry::Keyframe { cols: 40, rows: 10, .. }, .. }]));
    let mut redrawn = Screen::new(40, 10);
    assert!(redrawn.replay(&entries, u64::MAX));
    assert_eq!(held(&redrawn, 10), held(&original, 10));
}

/// A replay to the end holds what the live screen holds, through output, a resize and an application on
/// the alternate screen — the last of which a keyframe is not taken during.
#[test]
fn a_replay_holds_what_the_live_screen_held() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut live = Screen::new(40, 10);
    // A keyframe every two kilobytes, so the replay crosses several
    live.start_recording_with(Recorder::start_with(&path, u64::MAX, 2_000).expect("the recording starts"))
        .expect("the first keyframe is written");
    for round in 0..8 {
        live.feed(&busy(20 + round));
        live.feed(b"\r\n");
    }
    live.resize(52, 14, 0, 0);
    live.feed(&busy(12));
    live.feed(b"\x1b[?1049h\x1b[2J\x1b[Han application\r\nwith its own screen");
    for _ in 0..40 {
        live.feed(format!("\x1b[3;1H{}", "x".repeat(50)).as_bytes());
    }

    let entries = recording::read(&path);
    let keyframes = entries.iter().filter(|timed| matches!(timed.entry, Entry::Keyframe { .. })).count();
    assert!(keyframes > 3, "{keyframes} keyframes");
    // None while the application was showing: the last keyframe is of the shell's screen
    let last = entries.iter().rposition(|timed| matches!(timed.entry, Entry::Keyframe { .. })).expect("a keyframe");
    let after: usize =
        entries[last..].iter().map(|timed| if let Entry::Output(bytes) = &timed.entry { bytes.len() } else { 0 }).sum();
    assert!(after > 2_000, "only {after} bytes after the last keyframe, so one was taken over the application");

    let mut replayed = Screen::new(80, 24);
    assert!(replayed.replay(&entries, u64::MAX));
    assert_eq!(held(&replayed, 14), held(&live, 14));
    assert!(held(&replayed, 14).2, "the application is showing");

    // And once the application has gone, the shell's history is still under it
    live.feed(b"\x1b[?1049l");
    let mut after_leaving = Screen::new(80, 24);
    assert!(after_leaving.replay(&recording::read(&path), u64::MAX));
    assert_eq!(held(&after_leaving, 14), held(&live, 14));
}

/// A replay to a moment holds what had been written by then and nothing after.
#[test]
fn a_replay_stops_at_the_moment_it_is_asked_for() {
    let mut first = Screen::new(30, 6);
    first.feed(b"one\r\n");
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    first.start_recording(&path).expect("the recording starts");
    let keyframe = recording::read(&path).remove(0).entry;

    let entries = vec![
        Timed { at: 100, entry: keyframe },
        Timed { at: 200, entry: Entry::Output(b"two\r\n".to_vec()) },
        Timed { at: 300, entry: Entry::Output(b"three\r\n".to_vec()) },
    ];
    let mut screen = Screen::new(30, 6);
    assert!(screen.replay(&entries, 250));
    assert_eq!(screen.snapshot(0).lines[..3], ["one", "two", ""]);
    assert!(screen.replay(&entries, 300));
    assert_eq!(screen.snapshot(0).lines[..3], ["one", "two", "three"]);
    // Back again: a replay starts from the keyframe, not from what the screen last held
    assert!(screen.replay(&entries, 100));
    assert_eq!(screen.snapshot(0).lines[..2], ["one", ""]);
    // Before anything was recorded there is nothing to show, and the screen is left as it was
    assert!(!screen.replay(&entries, 99));
    assert_eq!(screen.snapshot(0).lines[..2], ["one", ""]);
}

#[test]
fn a_recording_stays_inside_its_budget_and_still_starts_with_a_whole_screen() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let budget = 1 << 20;
    let mut live = Screen::new(40, 10);
    live.start_recording_with(Recorder::start_with(&path, budget, 16_000).expect("the recording starts"))
        .expect("the first keyframe is written");
    let mut largest = 0;
    for round in 0..3_000 {
        live.feed(&busy(10 + round % 7));
        live.feed(b"\r\n");
        largest = largest.max(std::fs::metadata(&path).expect("the recording is there").len());
    }
    // Past it by at most what is written between two keyframes, and the keyframe itself
    assert!(largest < budget + budget / 2, "the recording reached {largest} bytes on a budget of {budget}");
    let entries = recording::read(&path);
    assert!(matches!(entries.first(), Some(Timed { entry: Entry::Keyframe { .. }, .. })));
    let mut replayed = Screen::new(40, 10);
    assert!(replayed.replay(&entries, u64::MAX));
    // The screen and the history a keyframe carries; what scrolled off before that is the part a replay
    // from the last keyframe does not bring back
    let (replayed_rows, cursor, _) = held(&replayed, 10);
    let (live_rows, live_cursor, _) = held(&live, 10);
    assert_eq!(cursor, live_cursor);
    let carried = 10 + apack_terminal::screen::KEYFRAME_HISTORY_LINES as usize;
    assert!(replayed_rows.len() >= carried, "{} rows", replayed_rows.len());
    assert_eq!(replayed_rows[replayed_rows.len() - carried..], live_rows[live_rows.len() - carried..]);

    // And everything the recording covers, replayed from its first keyframe, is more history than that
    let mut whole = Screen::new(40, 10);
    assert!(whole.replay_all(&entries));
    let (whole_rows, whole_cursor, _) = held(&whole, 10);
    assert_eq!(whole_cursor, live_cursor);
    assert!(whole_rows.len() > replayed_rows.len(), "{} rows against {}", whole_rows.len(), replayed_rows.len());
    assert_eq!(whole_rows[..], live_rows[live_rows.len() - whole_rows.len()..]);
}

/// A recording whose writer died mid-entry reads as everything before that entry.
#[test]
fn a_recording_cut_off_mid_entry_reads_up_to_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut live = Screen::new(30, 6);
    live.start_recording(&path).expect("the recording starts");
    live.feed(b"first\r\n");
    live.feed(b"second\r\n");
    let whole = std::fs::read(&path).expect("the recording reads");
    let entries = recording::decode(&whole);
    assert_eq!(entries.len(), 3);
    for cut in 1..(b"second\r\n".len() + 13) {
        let torn = recording::decode(&whole[..whole.len() - cut]);
        assert_eq!(torn, entries[..2], "cut {cut} bytes from the end");
    }
}

/// What a recorded program asked of the terminal and of the world is not asked again by a replay, and a
/// screen made ready for a new shell is out of whatever the last one's programs left it in.
#[test]
fn a_replay_tells_nobody_anything_and_a_restored_screen_is_ready_for_a_shell() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut live = Screen::new(40, 10);
    live.start_recording(&path).expect("the recording starts");
    // A cursor-position request, a clipboard write, a title, a bell, a command the shell marked as
    // running, then an editor with the mouse reported
    live.feed(b"\x1b[6n\x1b]52;c;c2VjcmV0\x07\x1b]0;a title\x07\x07");
    live.feed(b"\x1b]633;E;sleep 100\x07\x1b]633;C\x07");
    live.feed(b"shell line\r\n\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004heditor");
    assert!(!live.take_replies().is_empty());
    assert!(!live.take_effects().is_empty());
    assert!(live.facts().running);

    let mut restored = Screen::new(40, 10);
    assert!(restored.replay(&recording::read(&path), u64::MAX));
    assert_eq!(restored.take_replies(), Vec::<String>::new());
    assert!(restored.take_effects().is_empty());
    assert!(!restored.facts().running);

    restored.settle_restored();
    let snapshot = restored.snapshot(0);
    assert!(!snapshot.alt_screen);
    assert_eq!(snapshot.cursor_column, 0);
    assert!(snapshot.lines.iter().any(|line| line == "shell line"));
    assert!(!restored.mouse_reported().expect("the modes read"));
    assert!(restored.take_effects().is_empty());
}

/// A shell that was not inside an application is left where it was: its cursor under what it printed,
/// however long ago some program saved a cursor position of its own.
#[test]
fn a_restored_shell_screen_keeps_its_cursor_under_what_was_printed() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut live = Screen::new(40, 10);
    live.start_recording(&path).expect("the recording starts");
    // A cursor saved on the first row, as a prompt theme does, and then four lines printed under it
    live.feed(b"first\x1b7\r\nsecond\r\nthird\r\nfourth\r\n$ ");

    let mut restored = Screen::new(40, 10);
    assert!(restored.resume_recording(&path).expect("the recording is picked up"));
    let snapshot = restored.snapshot(0);
    assert_eq!(snapshot.lines[..5], ["first", "second", "third", "fourth", "$"]);
    assert_eq!((snapshot.cursor_row, snapshot.cursor_column), (5, 0), "the new shell would draw over what is there");
}

/// A session ended on purpose stops writing its recording at once, so whatever its program prints on the
/// way out cannot land in a file the next session has already picked up.
#[test]
fn a_recording_that_was_stopped_is_written_no_more() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut ending = Screen::new(40, 10);
    ending.start_recording(&path).expect("the recording starts");
    ending.feed(b"kept\r\n");
    ending.stop_recording();
    let before = std::fs::read(&path).expect("the recording reads");
    ending.feed(b"printed on the way out\r\n");
    ending.resize(50, 12, 0, 0);
    assert!(std::fs::read(&path).expect("the recording reads") == before);
    assert!(!ending.recorded());
}

/// What a recording on `budget` holds once it has been written a keyframe, some output and a second keyframe.
fn two_keyframes(path: &std::path::Path, budget: u64) -> Vec<Timed> {
    let mut recorder = Recorder::start_with(path, budget, u64::MAX).expect("the recording starts");
    recorder.keyframe(20, 4, vec![b'k'; 300]).expect("a keyframe is written");
    recorder.output(&[b'o'; 500]).expect("output is written");
    recorder.keyframe(20, 4, vec![b'K'; 300]).expect("a keyframe is written");
    recording::read(path)
}

/// **The budget is the size a recording may be, and the byte past it is what lets its oldest part go.** A
/// recording exactly at its budget is whole; one byte over starts at its second keyframe.
#[test]
fn a_recording_at_its_budget_is_kept_whole_and_one_byte_past_it_loses_its_oldest_part() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let whole = two_keyframes(&path, u64::MAX);
    assert_eq!(whole.len(), 3);
    let length = std::fs::metadata(&path).expect("the recording is there").len();

    let at_the_budget = two_keyframes(&path, length);
    assert_eq!(at_the_budget.len(), 3, "a recording at its budget dropped something");
    assert!(matches!(&at_the_budget[0].entry, Entry::Keyframe { redraw, .. } if redraw[0] == b'k'));

    let past_it = two_keyframes(&path, length - 1);
    assert_eq!(past_it.len(), 1, "a recording past its budget kept its oldest part");
    assert!(matches!(&past_it[0].entry, Entry::Keyframe { redraw, .. } if redraw[0] == b'K'));
}

/// A keyframe is put off while an application has the screen. Past its budget a recording stops putting it
/// off — but only once something has been written since the last, or every write would take another.
#[test]
fn a_recording_past_its_budget_can_no_longer_put_a_keyframe_off() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut recorder = Recorder::start_with(&path, 1_000, u64::MAX).expect("the recording starts");
    recorder.keyframe(20, 4, vec![b'k'; 100]).expect("a keyframe is written");
    recorder.output(&[b'o'; 400]).expect("output is written");
    assert!(!recorder.keyframe_overdue(), "inside its budget");
    // The same on a budget of exactly what that wrote: at it, and not past it
    let length = std::fs::metadata(&path).expect("the recording is there").len();
    let exact = directory.path().join("exact.recording");
    let mut at_its_budget = Recorder::start_with(&exact, length, u64::MAX).expect("the recording starts");
    at_its_budget.keyframe(20, 4, vec![b'k'; 100]).expect("a keyframe is written");
    at_its_budget.output(&[b'o'; 400]).expect("output is written");
    assert!(!at_its_budget.keyframe_overdue(), "at its budget");
    recorder.output(&[b'o'; 600]).expect("output is written");
    assert!(recorder.keyframe_overdue(), "past its budget, with output since the keyframe");

    // A keyframe larger than the budget leaves the recording past it with nothing written since
    recorder.keyframe(20, 4, vec![b'K'; 2_000]).expect("a keyframe is written");
    assert!(!recorder.keyframe_overdue(), "a keyframe was just taken");
    recorder.output(b"more").expect("output is written");
    assert!(recorder.keyframe_overdue());
}

/// The same through a screen: an application that never leaves the alternate screen does not grow its
/// recording without bound, which waiting for the shell's screen alone would let it.
#[test]
fn an_application_that_keeps_the_screen_does_not_grow_the_recording_without_bound() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let budget = 64_000;
    let mut live = Screen::new(40, 10);
    live.start_recording_with(Recorder::start_with(&path, budget, 4_000).expect("the recording starts"))
        .expect("the first keyframe is written");
    live.feed(b"\x1b[?1049h\x1b[2J");
    let mut largest = 0;
    for round in 0..6_000 {
        live.feed(format!("\x1b[{};1H{}", 1 + round % 10, "x".repeat(39)).as_bytes());
        largest = largest.max(std::fs::metadata(&path).expect("the recording is there").len());
    }
    assert!(largest < budget * 2, "the recording reached {largest} bytes on a budget of {budget}");
}

/// A recording carried on knows how much was written since its last keyframe, so the next one comes when
/// it would have and not a whole distance later.
#[test]
fn a_recording_carried_on_counts_what_was_written_since_its_last_keyframe() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    {
        let mut recorder = Recorder::start_with(&path, u64::MAX, u64::MAX).expect("the recording starts");
        recorder.keyframe(20, 4, vec![b'k'; 100]).expect("a keyframe is written");
        recorder.output(&vec![b'o'; (1 << 20) - 1]).expect("output is written");
    }
    let (mut resumed, entries) = Recorder::resume(&path).expect("the recording is carried on");
    assert_eq!(entries.len(), 2);
    assert!(!resumed.keyframe_due(), "a byte short of the distance between keyframes");
    assert!(resumed.output(b"o").expect("output is written"), "and the byte that reaches it");
}
