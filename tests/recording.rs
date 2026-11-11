//! A session's recording: that the terminal written out whole redraws as itself, that a replay holds what
//! the live screen held, that a recording stays inside its budget, and that nothing a replay provokes is
//! told a second time.

use std::path::Path;

use apack_terminal::recording::{self, Entry, Index, Recorder, Timed};
use apack_terminal::screen::Screen;
use apack_terminal::vt::rows::{RowRead, Space};
use proptest::prelude::*;

/// What a screen's recording holds now. The file is written by a thread of the recorder's own, so it is
/// waited for first: what is read is everything recorded, not everything that has reached the disk. A
/// recording that was stopped is waited for with `recording::settled`, which is what a recorder that
/// follows one on the same file does for itself.
fn recorded(screen: &mut Screen, path: &Path) -> Vec<Timed> {
    screen.flush_recording();
    recording::read(path)
}

/// How long a screen's recording is on disk now.
fn length(screen: &mut Screen, path: &Path) -> u64 {
    screen.flush_recording();
    std::fs::metadata(path).expect("the recording is there").len()
}

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

    let entries = recorded(&mut original, &path);
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

    let entries = recorded(&mut live, &path);
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
    assert!(after_leaving.replay(&recorded(&mut live, &path), u64::MAX));
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
    let keyframe = recorded(&mut first, &path).remove(0).entry;

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
        largest = largest.max(length(&mut live, &path));
    }
    // Past it by at most what is written between two keyframes, and the keyframe itself
    assert!(largest < budget + budget / 2, "the recording reached {largest} bytes on a budget of {budget}");
    let entries = recorded(&mut live, &path);
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
    live.flush_recording();
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
    assert!(live.mouse_reported().expect("the modes read"), "the editor asked for the mouse");

    let mut restored = Screen::new(40, 10);
    assert!(restored.replay(&recorded(&mut live, &path), u64::MAX));
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
    live.stop_recording();

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
    recording::settled(&path);
    let before = std::fs::read(&path).expect("the recording reads");
    ending.feed(b"printed on the way out\r\n");
    ending.resize(50, 12, 0, 0);
    assert!(std::fs::read(&path).expect("the recording reads") == before);
    assert!(!ending.recorded());
}

/// What a recording on `budget` holds once it has been written a keyframe, some output and a second keyframe.
fn two_keyframes(path: &Path, budget: u64) -> Vec<Timed> {
    let mut recorder = Recorder::start_with(path, budget, u64::MAX).expect("the recording starts");
    recorder.keyframe(20, 4, &[b'k'; 300]).expect("a keyframe is written");
    recorder.output(&[b'o'; 500]).expect("output is written");
    recorder.keyframe(20, 4, &[b'K'; 300]).expect("a keyframe is written");
    // Dropped, which is when the file is whole
    drop(recorder);
    recording::settled(path);
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

/// **What goes is the older half, and what stays starts at the first keyframe of the newer one.** Five
/// keyframes two hundred bytes apart, on a budget the fifth puts the recording past: the newer half begins
/// inside the third stretch, so the recording that remains begins at the fourth keyframe.
#[test]
fn a_recording_past_its_budget_keeps_from_the_first_keyframe_in_its_newer_half() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    // Each keyframe is 13 + 4 + 83 = 100 bytes and each stretch of output 13 + 87 = 100
    let mut recorder = Recorder::start_with(&path, 850, u64::MAX).expect("the recording starts");
    for mark in b"abcde" {
        recorder.keyframe(20, 4, &[*mark; 83]).expect("a keyframe is written");
        recorder.output(&[b'o'; 87]).expect("output is written");
    }
    drop(recorder);
    recording::settled(&path);
    let kept = recording::read(&path);
    // Nine hundred bytes at the fifth keyframe, on a budget of 850: the newer half starts 425 bytes from
    // the end, at byte 475, and the first keyframe at or after that is the fourth, at 600
    let marks: Vec<u8> = kept
        .iter()
        .filter_map(|timed| if let Entry::Keyframe { redraw, .. } = &timed.entry { Some(redraw[0]) } else { None })
        .collect();
    assert_eq!(marks, b"de");
    assert_eq!(std::fs::metadata(&path).expect("the recording is there").len(), 400);
}

/// A keyframe is put off while an application has the screen. Past its budget a recording stops putting
/// it off — and a keyframe that is itself larger than the budget, which leaves the recording past it
/// whatever is dropped, does not make every read after it take another.
#[test]
fn a_recording_past_its_budget_takes_a_keyframe_a_distance_and_not_a_keyframe_a_read() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut live = Screen::new(40, 10);
    live.feed(&busy(30));
    // A budget of a hundred bytes, which the first keyframe alone is many times over, and a keyframe due
    // every four thousand
    live.start_recording_with(Recorder::start_with(&path, 100, 4_000).expect("the recording starts"))
        .expect("the first keyframe is written");
    for _ in 0..100 {
        live.feed(b"0123456789");
    }
    let keyframes =
        recorded(&mut live, &path).iter().filter(|timed| matches!(timed.entry, Entry::Keyframe { .. })).count();
    assert_eq!(keyframes, 1, "a thousand bytes were written, which is a quarter of the way to the next keyframe");

    // And the distance does bring one, on the alternate screen too, since nothing else lets the oldest go
    live.feed(b"\x1b[?1049h");
    for _ in 0..400 {
        live.feed(b"0123456789");
    }
    let entries = recorded(&mut live, &path);
    assert!(matches!(entries.first(), Some(Timed { entry: Entry::Keyframe { .. }, .. })));
    assert!(entries.len() < 400, "{} entries: the oldest part was never dropped", entries.len());
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
        largest = largest.max(length(&mut live, &path));
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
        recorder.keyframe(20, 4, &[b'k'; 100]).expect("a keyframe is written");
        recorder.output(&vec![b'o'; (1 << 20) - 1]).expect("output is written");
    }
    let (mut resumed, entries) = Recorder::resume(&path).expect("the recording is carried on");
    assert_eq!(entries.len(), 2);
    assert!(!resumed.keyframe_due(), "a byte short of the distance between keyframes");
    resumed.output(b"o").expect("output is written");
    assert!(resumed.keyframe_due(), "and the byte that reaches it");
}

/// **A file that is there and is not a recording is neither picked up nor emptied.** Carrying a recording
/// on starts one where there is none; "none" is no file, or an empty one. Anything else that does not
/// read as a recording is somebody's file, and is refused with every byte of it where it was.
#[test]
fn a_file_that_is_not_a_recording_is_refused_and_left_as_it_was() {
    let directory = tempfile::tempdir().expect("a directory");
    let other = directory.path().join("notes.txt");
    std::fs::write(&other, b"not a recording, and not to be lost").expect("the file is written");
    let mut screen = Screen::new(20, 4);
    assert!(screen.resume_recording(&other).is_err());
    assert!(!screen.recorded());
    assert_eq!(std::fs::read(&other).expect("the file reads"), b"not a recording, and not to be lost");

    // No file, and an empty one, are each a recording not yet begun
    let absent = directory.path().join("absent.recording");
    assert_eq!(screen.resume_recording(&absent).ok(), Some(false), "nothing to rebuild from, and now recording");
    assert!(screen.recorded());
    screen.stop_recording();
    let empty = directory.path().join("empty.recording");
    std::fs::write(&empty, b"").expect("the file is written");
    assert_eq!(screen.resume_recording(&empty).ok(), Some(false));
    screen.stop_recording();
    recording::settled(&empty);
    assert!(matches!(recording::read(&empty).as_slice(), [Timed { entry: Entry::Keyframe { .. }, .. }]));
}

/// A recording is its owner's alone to read whether its file was made for it or was already there.
#[cfg(unix)]
#[test]
fn a_recording_picked_up_from_a_file_others_could_read_is_made_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mode = |path: &Path| std::fs::metadata(path).expect("the file is there").permissions().mode() & 0o777;
    let open = || std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("the mode is set");

    let mut first = Screen::new(20, 4);
    first.start_recording(&path).expect("the recording starts");
    first.feed(b"a secret, shown once\r\n");
    first.stop_recording();
    assert_eq!(mode(&path), 0o600);

    open();
    let mut carried_on = Screen::new(20, 4);
    assert!(carried_on.resume_recording(&path).expect("the recording is picked up"));
    assert_eq!(mode(&path), 0o600, "a recording carried on");
    carried_on.stop_recording();

    open();
    let mut replaced = Screen::new(20, 4);
    replaced.start_recording(&path).expect("the recording starts");
    assert_eq!(mode(&path), 0o600, "a recording started over a file that was there");
}

/// The file a recording is rewritten through is named after the whole of the recording's name, so two
/// recordings that differ only in what follows the last dot do not share one — and nothing else's file
/// that happens to have the recording's name with another ending is touched.
#[test]
fn a_recording_is_rewritten_through_a_file_of_its_own_name() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("session.1");
    // What a recording named `session.2` would be rewritten through, were the name's ending replaced
    // rather than added to — here as something a rewrite could not be written over
    let neighbour = directory.path().join("session.rewriting");
    std::fs::create_dir(&neighbour).expect("the directory is made");

    let mut live = Screen::new(40, 10);
    live.feed(b"where the recording began");
    live.start_recording_with(Recorder::start_with(&path, 20_000, 2_000).expect("the recording starts"))
        .expect("the first keyframe is written");
    let began_with = recorded(&mut live, &path).remove(0);
    for round in 0..200 {
        live.feed(&busy(5 + round % 3));
        live.feed(b"\r\n");
    }
    assert!(live.recorded(), "the recording stopped: it could not be rewritten");
    assert_ne!(recorded(&mut live, &path).first(), Some(&began_with), "the oldest part was never dropped");
    let mut replayed = Screen::new(40, 10);
    assert!(replayed.replay(&recorded(&mut live, &path), u64::MAX));
    assert_eq!(held(&replayed, 10).1, held(&live, 10).1);
}

/// **Times in a recording never go back**, whatever the clock does: what follows an entry is stamped no
/// earlier than it. A recording carried on begins from the last time in it, which here is an hour ahead
/// of the clock — as it would be after the clock was set back.
#[test]
fn what_is_recorded_after_an_entry_is_never_stamped_before_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut first = Screen::new(20, 4);
    first.start_recording(&path).expect("the recording starts");
    first.feed(b"before\r\n");
    first.stop_recording();
    recording::settled(&path);

    // The same recording, as if its last entry had been written an hour from now
    let ahead = recording::now() + 3_600_000;
    let mut bytes = std::fs::read(&path).expect("the recording reads");
    let last = recording::read(&path).last().map(|timed| timed.at).expect("an entry");
    let stamp = bytes.windows(8).rposition(|window| window == last.to_le_bytes()).expect("the last entry's time");
    bytes[stamp..stamp + 8].copy_from_slice(&ahead.to_le_bytes());
    std::fs::write(&path, &bytes).expect("the recording is written");

    let mut second = Screen::new(20, 4);
    assert!(second.resume_recording(&path).expect("the recording is picked up"));
    second.feed(b"after\r\n");
    let entries = recorded(&mut second, &path);
    assert!(entries.len() > 2);
    assert!(
        entries.is_sorted_by_key(|timed| timed.at),
        "{:?}",
        entries.iter().map(|timed| timed.at).collect::<Vec<_>>()
    );
    assert!(entries.last().is_some_and(|timed| timed.at >= ahead));
}

/// **A replay reports the recording's commands once, as of when they ran.** The commands a screen held
/// before are the same ones the replay is about to report, so they are forgotten first; each is stamped
/// with the time of the entry that reported it; and one the recording leaves running has ended, since
/// nothing is running it.
#[test]
fn a_replay_holds_the_recordings_commands_once_with_the_times_they_ran() {
    let mut source = Screen::new(40, 6);
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    source.start_recording(&path).expect("the recording starts");
    let keyframe = recorded(&mut source, &path).remove(0).entry;
    let output = |at: u64, bytes: &str| Timed { at, entry: Entry::Output(bytes.as_bytes().to_vec()) };
    let entries = vec![
        Timed { at: 1_000, entry: keyframe },
        output(2_000, "\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07"),
        output(3_000, "built\r\n\x1b]133;D;0\x07"),
        output(4_000, "\x1b]133;A\x07$ \x1b]133;B\x07sleep 100\r\n\x1b]133;C\x07zz"),
    ];

    let mut screen = Screen::new(40, 6);
    for _ in 0..3 {
        assert!(screen.replay(&entries, u64::MAX));
    }
    let (_, commands) = screen.commands().expect("the commands read");
    let times: Vec<_> = commands.iter().map(|one| (one.id, one.started_ms, one.finished_ms, one.exit_status)).collect();
    assert_eq!(
        times,
        vec![(1, Some(2_000), Some(3_000), Some(0)), (2, Some(4_000), Some(4_000), None)],
        "two commands, however many times the recording is replayed"
    );
    assert!(!screen.facts().running);
    assert_eq!(screen.command_output(1).expect("the output reads").as_deref(), Some("built"));
    assert_eq!(screen.command_output(2).expect("the output reads").as_deref(), Some("zz"));

    // Looked back at before the second command, it is not there at all
    assert!(screen.replay(&entries, 3_500));
    assert_eq!(screen.commands().expect("the commands read").1.len(), 1);
}

/// A recording opened to be looked through is read a moment at a time: where its keyframes are is kept,
/// and the entries for a moment are read from the file when the moment is asked for.
#[test]
fn a_recording_looked_through_reads_the_moment_asked_for() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut live = Screen::new(40, 10);
    live.start_recording_with(Recorder::start_with(&path, u64::MAX, 1_500).expect("the recording starts"))
        .expect("the first keyframe is written");
    for round in 0..12 {
        live.feed(&busy(8 + round));
        live.feed(b"\r\n");
    }
    let all = recorded(&mut live, &path);
    let mut index = Index::open(&path).expect("a recording with a keyframe");
    assert_eq!((index.first(), index.last()), (all[0].at, all[all.len() - 1].at));

    // The end, from the last keyframe: what a replay of everything read into memory does
    let last = all.iter().rposition(|timed| matches!(timed.entry, Entry::Keyframe { .. })).expect("a keyframe");
    assert!(last > 0, "one keyframe only, so this case shows nothing");
    assert_eq!(index.moment(u64::MAX).expect("the moment reads"), all[last..]);
    let (mut looked, mut replayed) = (Screen::new(40, 10), Screen::new(40, 10));
    assert!(looked.replay(&index.moment(index.last()).expect("the moment reads"), index.last()));
    assert!(replayed.replay(&all, u64::MAX));
    assert_eq!(held(&looked, 10), held(&replayed, 10));

    // Before the first keyframe there is nothing, and a file with no keyframe in it is not one to look through
    assert!(index.moment(index.first().saturating_sub(1)).expect("the moment reads").is_empty());
    let empty = directory.path().join("empty.recording");
    std::fs::write(&empty, b"").expect("the file is written");
    assert!(Index::open(&empty).is_none());
    assert!(Index::open(&directory.path().join("absent.recording")).is_none());
}

/// A stream a program might write, as pieces that each leave the parser between sequences: text, colour,
/// a link, a title, a character of several bytes, a cursor move. Lines are short, so none reaches the
/// right edge of the grid the cases below use.
fn stream() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        4 => "[a-z ]{1,10}",
        2 => (0u8..8, any::<bool>()).prop_map(|(color, bold)| format!("\x1b[{}3{color}m", if bold { "1;" } else { "" })),
        1 => Just("\x1b[0m".to_owned()),
        1 => (0u8..255, 0u8..255).prop_map(|(r, g)| format!("\x1b[38;2;{r};{g};9m\x1b[48;5;{g}m")),
        1 => "[a-z]{1,6}".prop_map(|word| format!("\x1b]8;;https://example.test/{word}\x1b\\{word}\x1b]8;;\x1b\\")),
        1 => "[a-z]{1,8}".prop_map(|title| format!("\x1b]0;{title}\x07")),
        2 => Just("\u{65e5}\u{672c}\u{8a9e} \u{e9}\u{1f680}".to_owned()),
        1 => (1u8..10, 1u8..60).prop_map(|(row, column)| format!("\x1b[{row};{column}H")),
        1 => Just("\x1b[K".to_owned()),
    ];
    let line = prop::collection::vec(piece, 1..6).prop_map(|pieces| format!("{}\r\n", pieces.concat()));
    prop::collection::vec(line, 1..40).prop_map(|lines| lines.concat().into_bytes())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// **A replay may begin at any keyframe**, and a read may end anywhere: in the middle of an escape
    /// sequence, of a control string, of a character. A keyframe is the screen and nothing of a sequence
    /// the parser is part way through, so one taken there would be followed in the recording by the rest
    /// of that sequence — which a replay beginning at the keyframe would be fed as text. So the stream is
    /// cut wherever the case says, with a keyframe due after almost every read, and replayed from each
    /// keyframe in turn: every one of them must end as the live screen did.
    #[test]
    fn a_replay_from_any_keyframe_ends_as_the_live_screen_did(
        written in stream(),
        cuts in prop::collection::vec(1usize..40, 1..200),
    ) {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("one.recording");
        let mut live = Screen::new(100, 12);
        live.start_recording_with(Recorder::start_with(&path, u64::MAX, 24).expect("the recording starts"))
            .expect("the first keyframe is written");
        let mut rest = written.as_slice();
        for cut in cuts.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (read, after) = rest.split_at((*cut).min(rest.len()));
            live.feed(read);
            rest = after;
        }

        let entries = recorded(&mut live, &path);
        let keyframes: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, timed)| matches!(timed.entry, Entry::Keyframe { .. }).then_some(index))
            .collect();
        prop_assert!(!keyframes.is_empty());
        for from in keyframes {
            let mut replayed = Screen::new(100, 12);
            prop_assert!(replayed.replay_all(&entries[from..]));
            prop_assert_eq!(held(&replayed, 12), held(&live, 12), "replayed from the keyframe at entry {}", from);
        }
    }
}

/// **One recorder a file, and one that follows another finds everything the other recorded.** A second
/// recorder is refused while the first is recording. When the first has stopped, the next waits for the
/// last of what it was handed to be in the file — the stopping itself waits for nothing — so megabytes
/// recorded an instant before are all there to be picked up.
#[test]
fn a_recorder_that_follows_another_on_a_file_finds_all_the_other_recorded() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut first = Recorder::start_with(&path, u64::MAX, u64::MAX).expect("the recording starts");
    first.keyframe(20, 4, b"k").expect("a keyframe is written");

    let other_spelling = directory.path().join(".").join("one.recording");
    assert!(Recorder::resume(&other_spelling).is_err(), "a second recorder was given a file being recorded to");
    assert!(Recorder::start(&path).is_err());

    let chunk = vec![b'o'; 64 * 1024];
    for _ in 0..128 {
        first.output(&chunk).expect("output is written");
    }
    drop(first);
    let (_second, entries) = Recorder::resume(&path).expect("the recording is carried on");
    assert_eq!(entries.len(), 129);
    assert!(entries[1..].iter().all(|timed| timed.entry == Entry::Output(chunk.clone())));
}

/// **A recording begun at a path takes the path, not the file that was there.** Something may be reading
/// that file — a recording being looked back through keeps it open — and what it is reading stays what
/// it was.
#[test]
fn a_recording_begun_where_one_is_being_looked_through_leaves_that_one_readable() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("one.recording");
    let mut earlier = Screen::new(40, 6);
    earlier.start_recording(&path).expect("the recording starts");
    earlier.feed(b"what was recorded before\r\n");
    earlier.stop_recording();
    recording::settled(&path);
    let mut looking = Index::open(&path).expect("a recording with a keyframe");
    let before = looking.moment(u64::MAX).expect("the moment reads");

    let mut later = Screen::new(40, 6);
    later.start_recording(&path).expect("the recording starts");
    later.feed(b"another session entirely\r\n");
    later.flush_recording();

    assert_eq!(looking.moment(u64::MAX).expect("the moment reads"), before);
    let mut shown = Screen::new(40, 6);
    assert!(shown.replay(&before, u64::MAX));
    assert_eq!(shown.snapshot(0).lines[0], "what was recorded before");
    // And the path is the new recording
    assert!(
        recording::read(&path)
            .iter()
            .all(|timed| timed.entry != Entry::Output(b"what was recorded before\r\n".to_vec()))
    );
}

/// **A process about to exit waits for what its recorders were handed.** A recorder that is stopped
/// leaves the last of its recording to its own thread, which ends with the process; `settle_all` is what
/// a broker calls on its way out, and after it every recording that was stopped holds all it was given
/// — here megabytes each, handed over far faster than a file takes them.
#[test]
fn every_stopped_recording_is_whole_once_they_have_all_been_waited_for() {
    let directory = tempfile::tempdir().expect("a directory");
    let paths: Vec<_> = (0..4).map(|number| directory.path().join(format!("{number}.recording"))).collect();
    // Under what a recorder will hold for a disk that is behind (`BACKLOG`), and most of it
    let (chunk, chunks) = (vec![b'o'; 256 << 10], 48);
    for path in &paths {
        let mut recorder = Recorder::start_with(path, u64::MAX, u64::MAX).expect("the recording starts");
        recorder.keyframe(20, 4, b"k").expect("a keyframe is written");
        for _ in 0..chunks {
            recorder.output(&chunk).expect("output is taken");
        }
        // Stopped with most of that still on its way to the file
        drop(recorder);
    }
    recording::settle_all();
    // Measured before anything is read, which would itself be time for a file to be written in
    let lengths: Vec<u64> =
        paths.iter().map(|path| std::fs::metadata(path).expect("the recording is there").len()).collect();
    for (path, length) in paths.iter().zip(lengths) {
        assert!(length >= (chunk.len() * chunks) as u64, "{} was left short, at {length} bytes", path.display());
        let output: usize = recording::read(path)
            .iter()
            .map(|timed| if let Entry::Output(bytes) = &timed.entry { bytes.len() } else { 0 })
            .sum();
        assert_eq!(output, chunk.len() * chunks, "{} does not hold what it was handed", path.display());
    }
}
