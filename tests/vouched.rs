//! A session spawned with a key: what its commands are recorded from, and what cannot move them.
//!
//! The marks here are written as a shell's hook writes them — `633;V;S` when a command begins and
//! `633;V;F` when it ends, each with the command's number and the session's key — and everything else a
//! terminal can be sent is written around them. What is claimed is that a keyed session's commands, and
//! whether one is running, are what those marks say and nothing else: not a marker of the families
//! anything can print, not a keyed mark with another key, and not the session's own marks shown again.

use std::sync::mpsc::{Receiver, channel};

use apack_terminal::commands::Outcome;
use apack_terminal::markers::{MarksKey, PAYLOAD_CAP};
use apack_terminal::protocol::{CommandOutcome, Unreported};
use apack_terminal::recording::{self, Entry, Recorder, Timed};
use apack_terminal::screen::{Screen, Vouching};
use apack_terminal::view::publish::ViewId;
use apack_terminal::vt::Effect;
use proptest::prelude::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";
/// A key of the right shape that is not the session's.
const ANOTHER: &str = "fedcba9876543210fedcba9876543210";
const BEL: &str = "\x07";

fn key(text: &str) -> MarksKey {
    MarksKey::new(text).expect("32 lowercase hexadecimal characters are a key")
}

fn keyed() -> Screen {
    Screen::keyed(80, 24, key(KEY))
}

/// A command beginning, as a hook holding `with` writes it.
fn start_with(with: &str, number: u64, line: &str) -> String {
    format!("\x1b]633;V;S;{number};{with};{line}{BEL}")
}

/// A command ending, as a hook holding `with` writes it. An empty `status` is a shell with none to give.
fn finish_with(with: &str, number: u64, status: &str, cwd: &str) -> String {
    format!("\x1b]633;V;F;{number};{with};{status};{cwd}{BEL}")
}

fn start(number: u64, line: &str) -> String {
    start_with(KEY, number, line)
}

fn finish(number: u64, status: &str, cwd: &str) -> String {
    finish_with(KEY, number, status, cwd)
}

const EXITED_0: CommandOutcome = CommandOutcome::Exited { code: 0 };

/// A command as `(id, line, where the shell was, how it ended)`, the last as a client is told it.
type Record = (u64, Option<String>, Option<String>, CommandOutcome);

fn records(screen: &mut Screen) -> Vec<Record> {
    let (_, commands) = screen.commands().expect("the commands read");
    commands.into_iter().map(|one| (one.id, one.command, one.cwd, one.outcome.into())).collect()
}

fn record(id: u64, line: &str, cwd: Option<&str>, outcome: CommandOutcome) -> Record {
    (id, Some(line.to_owned()), cwd.map(str::to_owned), outcome)
}

/// Whether a command is running, the line of the last to begin, and how the last to end ended.
type Said = (bool, Option<String>, Option<CommandOutcome>);

fn said(screen: &Screen) -> Said {
    let facts = screen.facts();
    (facts.running, facts.command.clone(), facts.outcome.map(Into::into))
}

fn refused(screen: &Screen) -> u32 {
    screen.vouching().refused
}

/// The commands a session was told began and ended, in order: the events among everything it was told.
fn told(screen: &mut Screen) -> Vec<String> {
    screen
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::CommandStarted { command } => Some(format!("started {command:?}")),
            Effect::CommandFinished { outcome } => Some(format!("finished {:?}", CommandOutcome::from(outcome))),
            _ => None,
        })
        .collect()
}

#[test]
fn a_keyed_start_and_its_finish_are_one_command_that_exited() {
    let mut screen = keyed();
    screen.feed(start(1, "make test").as_bytes());
    assert_eq!(said(&screen), (true, Some("make test".to_owned()), None));
    assert_eq!(records(&mut screen), vec![record(1, "make test", None, CommandOutcome::Running)]);

    screen.feed(finish(1, "2", "/src/app").as_bytes());
    assert_eq!(said(&screen), (false, Some("make test".to_owned()), Some(CommandOutcome::Exited { code: 2 })));
    assert_eq!(
        records(&mut screen),
        vec![record(1, "make test", Some("/src/app"), CommandOutcome::Exited { code: 2 })]
    );
    assert_eq!(told(&mut screen), ["started Some(\"make test\")", "finished Exited { code: 2 }"]);
    assert_eq!(screen.vouching(), Vouching { keyed: true, refused: 0 });
    assert_eq!(screen.facts().finished, 1, "one command ended, for a caller that asks after it has");
}

#[test]
fn a_finish_closes_only_the_command_of_its_number() {
    let mut screen = keyed();
    screen.feed(start(4, "sleep 100").as_bytes());
    for other in [3, 5, 1] {
        screen.feed(finish(other, "0", "/tmp").as_bytes());
    }
    assert_eq!(records(&mut screen), vec![record(1, "sleep 100", None, CommandOutcome::Running)]);
    assert!(screen.facts().running, "a finish for another command ended this one");
    assert_eq!(refused(&screen), 3);

    screen.feed(finish(4, "0", "/tmp").as_bytes());
    assert_eq!(records(&mut screen), vec![record(1, "sleep 100", Some("/tmp"), EXITED_0)]);

    // And once it has ended, its finish said again ends nothing and says nothing
    screen.feed(finish(4, "1", "/elsewhere").as_bytes());
    assert_eq!(records(&mut screen), vec![record(1, "sleep 100", Some("/tmp"), EXITED_0)]);
    assert_eq!(refused(&screen), 4);
}

#[test]
fn a_start_past_a_running_command_ends_that_one_with_its_end_unreported() {
    let mut screen = keyed();
    screen.feed([start(1, "first"), start(2, "second")].concat().as_bytes());
    assert_eq!(
        records(&mut screen),
        vec![
            record(1, "first", None, CommandOutcome::Unknown { why: Unreported::Superseded }),
            record(2, "second", None, CommandOutcome::Running),
        ]
    );
    assert_eq!(
        told(&mut screen),
        ["started Some(\"first\")", "finished Unknown { why: Superseded }", "started Some(\"second\")"]
    );
    assert_eq!(refused(&screen), 0, "a command beginning past the last is the shell's own to report");
}

/// The key is in the terminal's own output, so a mark that carries it may be the past shown again. What
/// tells the two apart is the number.
#[test]
fn a_start_whose_number_is_not_past_the_last_is_refused() {
    let mut screen = keyed();
    screen.feed([start(5, "five"), finish(5, "0", "/")].concat().as_bytes());
    for stale in [5, 4, 1] {
        screen.feed(start(stale, "shown again").as_bytes());
    }
    assert_eq!(records(&mut screen), vec![record(1, "five", Some("/"), EXITED_0)]);
    assert!(!screen.facts().running);
    assert_eq!(refused(&screen), 3);

    // Numbers need not be next to each other, only past the last
    screen.feed(start(9, "nine").as_bytes());
    assert_eq!(records(&mut screen).len(), 2);
}

#[test]
fn a_finish_with_no_status_is_a_command_that_is_over_and_not_an_exit() {
    let mut screen = keyed();
    screen.feed([start(1, "read"), finish(1, "", "/tmp")].concat().as_bytes());
    assert_eq!(
        records(&mut screen),
        vec![record(1, "read", Some("/tmp"), CommandOutcome::Unknown { why: Unreported::NoStatus })]
    );
}

/// A status only a keyed mark can give is the only one called an exit: a session with no key believes
/// every marker, and says of each that it was claimed.
#[test]
fn a_session_without_a_key_reports_how_a_command_ended_as_claimed_and_never_as_exited() {
    let mut screen = Screen::new(80, 24);
    let stream = [
        format!("\x1b]133;C{BEL}\x1b]133;D;0{BEL}"),
        format!("\x1b]633;E;make{BEL}\x1b]633;C{BEL}\x1b]633;D;3{BEL}"),
        format!("\x1b]133;C{BEL}\x1b]133;D{BEL}"),
        // Marks that carry a key are nothing to a session that has none, whatever they carry
        start(1, "vouched"),
        finish(1, "0", "/"),
    ]
    .concat();
    screen.feed(stream.as_bytes());

    let outcomes: Vec<CommandOutcome> = records(&mut screen).into_iter().map(|(.., outcome)| outcome).collect();
    assert_eq!(
        outcomes,
        vec![
            CommandOutcome::Claimed { code: Some(0) },
            CommandOutcome::Claimed { code: Some(3) },
            CommandOutcome::Claimed { code: None },
        ]
    );
    assert_eq!(screen.facts().outcome, Some(Outcome::Claimed(None)));
    assert_eq!(screen.vouching(), Vouching { keyed: false, refused: 2 });
}

#[test]
fn the_session_ending_closes_the_command_its_shell_left_running() {
    let mut screen = keyed();
    screen.feed([start(1, "make"), finish(1, "0", "/"), start(2, "exit")].concat().as_bytes());
    screen.take_effects();

    screen.session_ended();
    assert_eq!(
        records(&mut screen),
        vec![
            record(1, "make", Some("/"), EXITED_0),
            record(2, "exit", None, CommandOutcome::Unknown { why: Unreported::SessionEnded }),
        ]
    );
    assert!(!screen.facts().running);
    assert_eq!(told(&mut screen), ["finished Unknown { why: SessionEnded }"]);

    // Said once: a session that has ended has nothing left to close
    screen.session_ended();
    assert_eq!(told(&mut screen), Vec::<String>::new());
}

/// The hooks of a keyed session still print the standard marks, which is what prompt rows are flagged
/// from. Anything else can print them too, so they begin and end nothing.
#[test]
fn the_marks_anything_can_print_begin_and_end_nothing_in_a_keyed_session() {
    let unkeyed = [
        format!("\x1b]133;A{BEL}"),
        format!("\x1b]133;C{BEL}"),
        format!("\x1b]133;D;0{BEL}"),
        format!("\x1b]633;A{BEL}"),
        format!("\x1b]633;E;rm -rf /{BEL}"),
        format!("\x1b]633;C{BEL}"),
        format!("\x1b]633;D;0{BEL}"),
    ];

    let mut idle = keyed();
    idle.feed(unkeyed.concat().as_bytes());
    assert_eq!(records(&mut idle), vec![]);
    assert_eq!(said(&idle), (false, None, None));
    assert_eq!(told(&mut idle), Vec::<String>::new());

    let mut running = keyed();
    running.feed(start(1, "sleep 100").as_bytes());
    running.feed(unkeyed.concat().as_bytes());
    assert_eq!(records(&mut running), vec![record(1, "sleep 100", None, CommandOutcome::Running)]);
    assert_eq!(said(&running), (true, Some("sleep 100".to_owned()), None));
    assert_eq!(refused(&running), 0, "a mark of another family is not a keyed mark gone wrong");
}

/// The directory is heard from the families anything can print, in a keyed session as in any other: it
/// is what a tab is titled with before a command has run, and is a claim in both.
#[test]
fn a_keyed_session_still_hears_where_its_shell_says_it_is() {
    let mut screen = keyed();
    screen.feed(format!("\x1b]7;file://host/home/me{BEL}").as_bytes());
    assert_eq!(screen.facts().cwd.as_deref(), Some("/home/me"));
    screen.feed(format!("\x1b]633;P;Cwd=/srv{BEL}").as_bytes());
    assert_eq!(screen.facts().cwd.as_deref(), Some("/srv"));
}

#[test]
fn a_mark_with_the_wrong_key_a_number_that_is_not_one_or_a_field_missing_is_refused_and_counted() {
    let wrong_case = KEY.to_uppercase();
    let not_marks = [
        // Another key, and the session's own spelled another way, a character short and a character over
        start_with(ANOTHER, 1, "ls"),
        start_with(&wrong_case, 1, "ls"),
        start_with(&KEY[1..], 1, "ls"),
        start_with(&format!("{KEY}0"), 1, "ls"),
        start_with("", 1, "ls"),
        // Numbers a shell does not write: a sign, a zero in front, zero itself, none, a fraction, one too
        // large to be a number here
        format!("\x1b]633;V;S;+1;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S;-1;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S;01;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S;0;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S;;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S;1.5;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S; 1;{KEY};ls{BEL}"),
        format!("\x1b]633;V;S;99999999999999999999999;{KEY};ls{BEL}"),
        // Fields missing: no line, no key, no number, nothing at all
        format!("\x1b]633;V;S;1;{KEY}{BEL}"),
        format!("\x1b]633;V;S;1{BEL}"),
        format!("\x1b]633;V;S{BEL}"),
        format!("\x1b]633;V;{BEL}"),
        format!("\x1b]633;V{BEL}"),
        // The key where the number goes, and a kind that is neither
        format!("\x1b]633;V;S;{KEY};1;ls{BEL}"),
        format!("\x1b]633;V;X;1;{KEY};ls{BEL}"),
    ];
    let mut screen = keyed();
    for (count, not_a_mark) in not_marks.iter().enumerate() {
        screen.feed(not_a_mark.as_bytes());
        assert_eq!(refused(&screen), count as u32 + 1, "{not_a_mark:?} was not counted as refused");
        assert_eq!(said(&screen), (false, None, None), "{not_a_mark:?} was believed");
    }
    assert_eq!(records(&mut screen), vec![]);

    // A finish refused for what it carries leaves its command running
    screen.feed(start(1, "ls").as_bytes());
    let not_finishes = [
        finish_with(ANOTHER, 1, "0", "/"),
        format!("\x1b]633;V;F;1;{KEY};0{BEL}"),
        format!("\x1b]633;V;F;1;{KEY}{BEL}"),
        finish(1, "zero", "/"),
        finish(1, "+0", "/"),
        finish(1, "0x0", "/"),
        finish(1, "99999999999", "/"),
        finish(1, "-", "/"),
    ];
    for not_a_finish in &not_finishes {
        screen.feed(not_a_finish.as_bytes());
        assert!(screen.facts().running, "{not_a_finish:?} ended the command");
    }
    assert_eq!(refused(&screen), (not_marks.len() + not_finishes.len()) as u32);
}

#[test]
fn a_status_is_read_with_its_sign_where_a_platform_has_negative_ones() {
    let mut screen = keyed();
    screen.feed([start(1, "a.exe"), finish(1, "-1073741510", "C:/")].concat().as_bytes());
    assert_eq!(said(&screen).2, Some(CommandOutcome::Exited { code: -1_073_741_510 }));
}

/// The number and the key come before anything a command line or a directory can hold, and the last
/// field is taken whole: nothing in a line moves where the key is looked for.
#[test]
fn the_fields_are_where_the_grammar_puts_them_whatever_a_line_or_a_directory_holds() {
    let lines = [
        "echo a;b;c",
        "printf '\\033]633;V;F;1;0123456789abcdef0123456789abcdef;0;/\\007'",
        "; ;; ;",
        "633;V;S;2;key;nested",
        "",
    ];
    for line in lines {
        let mut screen = keyed();
        screen.feed(start(1, line).as_bytes());
        assert_eq!(records(&mut screen), vec![record(1, line, None, CommandOutcome::Running)]);
    }

    // Escaped as the line of any marker is: a percent sign and the bytes that would end the sequence
    let mut screen = keyed();
    screen.feed(start(1, "printf '100%25%0a%1b[0m'").as_bytes());
    screen.feed(finish(1, "0", "/tmp/50%25 off;now").as_bytes());
    assert_eq!(records(&mut screen), vec![record(1, "printf '100%\n\x1b[0m'", Some("/tmp/50% off;now"), EXITED_0)]);

    // A finish that names no directory records none, rather than an empty one
    screen.feed([start(2, "true"), finish(2, "0", "")].concat().as_bytes());
    assert_eq!(records(&mut screen)[1], record(2, "true", None, EXITED_0));
}

#[test]
fn a_keyed_mark_ended_the_other_way_is_read() {
    let mut screen = keyed();
    screen.feed(format!("\x1b]633;V;S;1;{KEY};ls\x1b\\\x1b]633;V;F;1;{KEY};0;/tmp\x1b\\").as_bytes());
    assert_eq!(records(&mut screen), vec![record(1, "ls", Some("/tmp"), EXITED_0)]);
}

/// Output arrives in whatever sizes a read returned, and where one ends is not where anything happened.
#[test]
fn keyed_marks_are_read_the_same_however_the_stream_is_cut() {
    let stream =
        [start(1, "make;test"), "ok\r\n".to_owned(), finish(1, "2", "/srv"), start(3, "sleep")].concat().into_bytes();
    let read = |cut: &dyn Fn(&mut Screen)| {
        let mut screen = keyed();
        cut(&mut screen);
        (records(&mut screen), said(&screen), refused(&screen))
    };
    let whole = read(&|screen| screen.feed(&stream));
    assert_eq!(whole.0.len(), 2);
    assert_eq!(whole.2, 0);

    for at in 1..stream.len() {
        let cut = read(&|screen| {
            screen.feed(&stream[..at]);
            screen.feed(&stream[at..]);
        });
        assert_eq!(cut, whole, "cut at {at}");
    }
    let a_byte_at_a_time = read(&|screen| stream.iter().for_each(|byte| screen.feed(&[*byte])));
    assert_eq!(a_byte_at_a_time, whole);
}

/// A sequence longer than the core keeps arrives cut short. What vouches for a keyed mark comes before
/// anything of a length a program chooses, so a command with a very long line is still a command
/// running — a session that dropped its start would say nothing is.
#[test]
fn a_start_longer_than_the_core_keeps_is_still_a_command_running() {
    let long = "x".repeat(PAYLOAD_CAP * 2);
    let mut screen = keyed();
    screen.feed(start(1, &long).as_bytes());
    assert!(screen.facts().running);
    let running = records(&mut screen);
    let [(_, Some(line), _, CommandOutcome::Running)] = running.as_slice() else {
        panic!("the command is not running with a line: {running:?}");
    };
    assert!(long.starts_with(line.as_str()) && !line.is_empty(), "the line is the beginning of the one sent");

    // Its finish with a directory too long to keep ends it, with the status and without the directory
    screen.feed(finish(1, "7", &format!("/{long}")).as_bytes());
    let ended = records(&mut screen);
    let [(_, _, cwd, outcome)] = ended.as_slice() else { panic!("expected one command: {ended:?}") };
    assert_eq!((cwd, outcome), (&None, &CommandOutcome::Exited { code: 7 }));
    assert_eq!(refused(&screen), 0);
}

/// A screen with a view attached, which is what gives its lines their numbers.
fn viewed(screen: &mut Screen) -> Receiver<Vec<u8>> {
    let (sink, incoming) = channel();
    screen.attach_view(ViewId { connection: 1, channel: 1 }, sink).expect("a view attaches");
    incoming
}

/// A command as `(line its output began on, line it ended on, what it printed)`.
fn rows(screen: &mut Screen) -> Vec<(Option<u64>, Option<u64>, Option<String>)> {
    let (_, commands) = screen.commands().expect("the commands read");
    commands
        .into_iter()
        .map(|one| (one.output, one.end, screen.command_output(one.id).expect("the output reads")))
        .collect()
}

#[test]
fn a_keyed_commands_rows_are_where_its_marks_were_written() {
    let mut screen = Screen::keyed(40, 20, key(KEY));
    let _view = viewed(&mut screen);
    let stream = [
        "$ make\r\n".to_owned(),
        start(1, "make"),
        "one\r\ntwo\r\n".to_owned(),
        finish(1, "0", "/"),
        "$ true\r\n".to_owned(),
        start(2, "true"),
        finish(2, "0", "/"),
        "$ ".to_owned(),
    ]
    .concat();
    screen.feed(stream.as_bytes());
    assert_eq!(
        rows(&mut screen),
        vec![(Some(1), Some(3), Some("one\ntwo".to_owned())), (Some(4), Some(4), Some(String::new()))]
    );
}

/// What a recording holds, read back once everything recorded has reached its file.
fn recorded(screen: &mut Screen, path: &std::path::Path) -> Vec<Timed> {
    screen.flush_recording();
    recording::read(path)
}

/// A session's commands rebuilt from its recording are the commands that were: what it refused is
/// refused again, and what it recorded ended as it ended.
#[test]
fn a_session_restored_from_a_recording_holds_its_commands_as_they_ended() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("session.rec");
    let stream = [
        start(1, "make"),
        "built\r\n".to_owned(),
        // Printed by what `make` ran, and by a log it showed: none of it carries the key
        format!("\x1b]133;D;0{BEL}\x1b]633;D;0{BEL}"),
        finish_with(ANOTHER, 1, "0", "/forged"),
        finish(1, "3", "/src"),
        start(2, "cat session.log"),
        // The session's own marks, shown again by the command that is running
        start(1, "make"),
        finish(1, "0", "/src"),
        start(2, "cat session.log"),
        finish(2, "0", "/src"),
        start(3, "sleep 100"),
        "zz".to_owned(),
    ]
    .concat();
    let before = {
        let mut first = Screen::keyed(40, 6, key(KEY));
        first.start_recording(&path).expect("the recording starts");
        first.feed(stream.as_bytes());
        let before = records(&mut first);
        recorded(&mut first, &path);
        before
    };
    assert_eq!(
        before,
        vec![
            record(1, "make", Some("/src"), CommandOutcome::Exited { code: 3 }),
            record(2, "cat session.log", Some("/src"), EXITED_0),
            record(3, "sleep 100", None, CommandOutcome::Running),
        ]
    );

    // The session that replaces it has a shell of its own, and so a key of its own
    let mut second = Screen::keyed(40, 6, key(ANOTHER));
    assert!(second.resume_recording(&path).expect("the recording is picked up"));
    let ended = |mut all: Vec<Record>| {
        all[2].3 = CommandOutcome::Unknown { why: Unreported::SessionEnded };
        all
    };
    assert_eq!(records(&mut second), ended(before.clone()), "what was running ended with its session");
    assert_eq!(second.vouching(), Vouching { keyed: true, refused: 0 }, "what the recording refused is its own");

    // Its own shell numbers its commands from one, and is heard with its own key and no other
    second.feed([start_with(ANOTHER, 1, "ls"), start(4, "forged"), finish(3, "0", "/")].concat().as_bytes());
    let now = records(&mut second);
    assert_eq!(now.len(), 4);
    assert_eq!(now[3], record(4, "ls", None, CommandOutcome::Running));
    assert_eq!(refused(&second), 2);

    // And a recording two sessions wrote is read back with each one's key over its own stretch
    second.feed(finish_with(ANOTHER, 1, "0", "/home").as_bytes());
    let entries = recorded(&mut second, &path);
    let mut looked_back = Screen::new(40, 6);
    assert!(looked_back.replay_all(&entries));
    let mut expected = ended(before);
    expected.push(record(4, "ls", Some("/home"), EXITED_0));
    assert_eq!(records(&mut looked_back), expected);
    assert_eq!(looked_back.vouching(), Vouching { keyed: false, refused: 0 });
}

fn output(at: u64, bytes: &str) -> Timed {
    Timed { at, entry: Entry::Output(bytes.as_bytes().to_vec()) }
}

/// A replay may begin at any keyframe. What a session refused for being the past is refused from there
/// too, because the recording says after each keyframe which number the session had reached.
#[test]
fn a_replay_that_begins_part_way_through_a_session_refuses_what_the_session_refused() {
    let entries = vec![
        Timed { at: 1_000, entry: Entry::Keyframe { cols: 40, rows: 6, redraw: b"$ cat log\r\n".to_vec() } },
        Timed { at: 1_000, entry: Entry::Marks { key: Some(key(KEY)), last: 7 } },
        // The rest of what command 7 printed: marks the session had already taken
        output(2_000, &[start(3, "make"), finish(3, "0", "/"), start(7, "cat log")].concat()),
        output(3_000, &[finish(7, "0", "/"), start(8, "ls"), finish(8, "1", "/tmp")].concat()),
    ];
    let mut screen = Screen::new(40, 6);
    assert!(screen.replay(&entries, u64::MAX));
    assert_eq!(
        records(&mut screen),
        vec![record(1, "ls", Some("/tmp"), CommandOutcome::Exited { code: 1 })],
        "the commands since the keyframe, and none shown again from before it"
    );
}

/// A screen rebuilt from a recording goes on being its own session afterwards: the replay is read with
/// the recording's key and numbering, and moves nothing of the session's.
#[test]
fn a_replay_moves_nothing_of_what_a_sessions_own_shell_has_been_heard_to_say() {
    let mut screen = keyed();
    screen.feed([start(1, "one"), finish(1, "0", "/"), start_with(ANOTHER, 2, "forged")].concat().as_bytes());
    assert_eq!(refused(&screen), 1);

    let entries = vec![
        Timed { at: 1_000, entry: Entry::Keyframe { cols: 40, rows: 6, redraw: Vec::new() } },
        Timed { at: 1_000, entry: Entry::Marks { key: Some(key(ANOTHER)), last: 0 } },
        output(2_000, &[start_with(ANOTHER, 41, "theirs"), start(1, "refused there"), start(2, "and this")].concat()),
    ];
    assert!(screen.replay_all(&entries));
    assert_eq!(refused(&screen), 1, "a replay's refusals were counted as the session's");

    // Its own last number stands: the one it had taken is the past, and the next is not
    screen.feed(start(1, "again").as_bytes());
    assert_eq!(refused(&screen), 2);
    screen.feed(start(2, "two").as_bytes());
    assert_eq!(said(&screen), (true, Some("two".to_owned()), None));
    // And a mark carrying the recording's key is nothing to it
    screen.feed(start_with(ANOTHER, 99, "theirs").as_bytes());
    assert_eq!(refused(&screen), 3);
}

/// A recording says a shell is another one by naming a key or a number that is not the one it had
/// reached: a session that picks a recording up with the key the last one had is still another shell,
/// numbering its commands from one. Said again with nothing changed, it changes nothing.
#[test]
fn a_recording_naming_its_key_again_changes_nothing_unless_the_number_is_another_shells() {
    let keyframe = Timed { at: 1_000, entry: Entry::Keyframe { cols: 40, rows: 6, redraw: Vec::new() } };
    let named = |at: u64, last: u64| Timed { at, entry: Entry::Marks { key: Some(key(KEY)), last } };

    // The same key at the number the session had reached: the same shell, part way through a command
    let entries = vec![
        keyframe.clone(),
        named(1_000, 0),
        output(2_000, &start(1, "make")),
        named(2_500, 1),
        output(3_000, &finish(1, "0", "/")),
    ];
    let mut screen = Screen::new(40, 6);
    assert!(screen.replay_all(&entries));
    assert_eq!(records(&mut screen), vec![record(1, "make", Some("/"), EXITED_0)]);

    // The same key at a number behind it: another shell, whose first command is its own
    let entries = vec![
        keyframe.clone(),
        named(1_000, 0),
        output(2_000, &[start(1, "make"), finish(1, "0", "/"), start(2, "sleep 100")].concat()),
        named(3_000, 0),
        output(4_000, &[start(1, "ls"), finish(1, "1", "/tmp")].concat()),
    ];
    let mut screen = Screen::new(40, 6);
    assert!(screen.replay_all(&entries));
    assert_eq!(
        records(&mut screen),
        vec![
            record(1, "make", Some("/"), EXITED_0),
            record(2, "sleep 100", None, CommandOutcome::Unknown { why: Unreported::SessionEnded }),
            record(3, "ls", Some("/tmp"), CommandOutcome::Exited { code: 1 }),
        ]
    );

    // And a recording that says no key vouches for it, of a stream no key vouched for, says nothing new
    let unnamed = Timed { at: 2_500, entry: Entry::Marks { key: None, last: 0 } };
    let entries = vec![
        keyframe,
        output(2_000, &format!("\x1b]133;C{BEL}")),
        unnamed,
        output(3_000, &format!("\x1b]133;D;0{BEL}")),
    ];
    let mut screen = Screen::new(40, 6);
    assert!(screen.replay_all(&entries));
    let outcomes: Vec<CommandOutcome> = records(&mut screen).into_iter().map(|(.., outcome)| outcome).collect();
    assert_eq!(outcomes, vec![CommandOutcome::Claimed { code: Some(0) }]);
}

/// A recording is one file however many sessions wrote it, with and without keys. Each stretch is read
/// back as the session that wrote it read it: its commands, ended as it heard them end.
#[test]
fn a_recording_passed_between_sessions_with_and_without_a_key_is_read_back_as_each_wrote_it() {
    let directory = tempfile::tempdir().expect("a directory");
    let path = directory.path().join("terminal.rec");
    let claimed_command = |status: u32| format!("\x1b]133;C{BEL}out\r\n\x1b]133;D;{status}{BEL}");

    let mut first = Screen::new(40, 6);
    first.start_recording(&path).expect("the recording starts");
    first.feed([claimed_command(0), start(1, "nothing here has a key")].concat().as_bytes());
    drop(first);

    let mut second = Screen::keyed(40, 6, key(KEY));
    assert!(second.resume_recording(&path).expect("the recording is picked up"));
    second.feed([start(1, "make"), claimed_command(1), finish(1, "0", "/src")].concat().as_bytes());
    drop(second);

    let mut third = Screen::new(40, 6);
    assert!(third.resume_recording(&path).expect("the recording is picked up"));
    third.feed([claimed_command(2), start(2, "nor has this")].concat().as_bytes());

    let written = vec![
        (None, None, CommandOutcome::Claimed { code: Some(0) }),
        (Some("make".to_owned()), Some("/src".to_owned()), EXITED_0),
        (None, None, CommandOutcome::Claimed { code: Some(2) }),
    ];
    let heard = |screen: &mut Screen| -> Vec<_> {
        records(screen).into_iter().map(|(_, line, cwd, outcome)| (line, cwd, outcome)).collect()
    };
    assert_eq!(heard(&mut third), written, "what the session holds, having picked it up");

    let entries = recorded(&mut third, &path);
    let mut replayed = Screen::new(40, 6);
    assert!(replayed.replay_all(&entries));
    assert_eq!(heard(&mut replayed), written, "what the whole recording replays as");

    // And looked back through by its keyframes, as a rewind reads it
    let mut index = recording::Index::open(&path).expect("the recording has a keyframe");
    let (last, moment) = (index.last(), index.moment(u64::MAX).expect("the recording reads"));
    assert_eq!(moment.len(), entries.len(), "the index stopped short of the recording's end");
    let mut looked_back = Screen::new(40, 6);
    assert!(looked_back.replay(&moment, last));
    assert_eq!(heard(&mut looked_back), written);
}

/// A prompt a session without a key left waiting was never a command. A keyed session that begins with
/// such a recording's screen takes none of it for its own first command.
#[test]
fn a_keyed_command_is_a_record_of_its_own_and_not_the_prompt_a_replay_left_waiting() {
    let entries = vec![
        Timed { at: 1_000, entry: Entry::Keyframe { cols: 40, rows: 6, redraw: Vec::new() } },
        output(2_000, &format!("\x1b]133;C{BEL}out\r\n\x1b]133;D;0{BEL}\x1b]133;A{BEL}$ ")),
    ];
    let mut screen = keyed();
    assert!(screen.replay_all(&entries));
    let (_, waiting) = screen.commands().expect("the commands read");
    assert_eq!(waiting.len(), 2, "the command the recording ran, and its next prompt waiting");
    assert_eq!(waiting[1].started_ms, None);

    screen.feed(start(1, "ls").as_bytes());
    let now = records(&mut screen);
    assert_eq!(now.len(), 2);
    assert_eq!(now[0].3, CommandOutcome::Claimed { code: Some(0) });
    assert_eq!((now[1].1.as_deref(), now[1].3), (Some("ls"), CommandOutcome::Running));
}

/// A recording says which key vouches for what follows only where one does, so a session without a key
/// records what it always has.
#[test]
fn a_recording_names_its_key_after_each_keyframe_and_a_session_without_one_names_none() {
    let directory = tempfile::tempdir().expect("a directory");
    let marks = |entries: &[Timed]| -> Vec<(usize, Option<String>, u64)> {
        entries
            .iter()
            .enumerate()
            .filter_map(|(index, timed)| match &timed.entry {
                Entry::Marks { key, last } => Some((index, key.as_ref().map(|key| key.as_str().to_owned()), *last)),
                _ => None,
            })
            .collect()
    };
    let keyframes = |entries: &[Timed]| -> Vec<usize> {
        (0..entries.len()).filter(|index| matches!(entries[*index].entry, Entry::Keyframe { .. })).collect()
    };

    let unkeyed = directory.path().join("unkeyed.rec");
    let mut screen = Screen::new(40, 6);
    screen.start_recording(&unkeyed).expect("the recording starts");
    screen.feed(format!("\x1b]133;C{BEL}out\r\n\x1b]133;D;0{BEL}").as_bytes());
    assert_eq!(marks(&recorded(&mut screen, &unkeyed)), vec![]);
    // And one picked up by another session without a key says no more than it did
    drop(screen);
    let mut next = Screen::new(40, 6);
    assert!(next.resume_recording(&unkeyed).expect("the recording is picked up"));
    next.feed(format!("\x1b]133;C{BEL}more\r\n\x1b]133;D;0{BEL}").as_bytes());
    assert_eq!(marks(&recorded(&mut next, &unkeyed)), vec![]);

    // A keyframe every 64 bytes, so a few commands are several
    let path = directory.path().join("keyed.rec");
    let mut screen = Screen::keyed(40, 6, key(KEY));
    screen
        .start_recording_with(Recorder::start_with(&path, recording::BUDGET, 64).expect("the recording starts"))
        .expect("the first keyframe is taken");
    for number in 1..=4 {
        screen.feed([start(number, "make"), "output\r\n".repeat(4), finish(number, "0", "/")].concat().as_bytes());
    }
    let entries = recorded(&mut screen, &path);
    let (marks, keyframes) = (marks(&entries), keyframes(&entries));
    assert!(keyframes.len() > 2, "the case took one keyframe: {entries:?}");
    assert_eq!(
        marks.iter().map(|(index, ..)| *index).collect::<Vec<_>>(),
        keyframes.iter().map(|index| index + 1).collect::<Vec<_>>(),
        "each keyframe is followed by what vouches for the stream after it"
    );
    assert!(marks.iter().all(|(_, named, _)| named.as_deref() == Some(KEY)));
    let reached: Vec<u64> = marks.iter().map(|(.., last)| *last).collect();
    assert_eq!(reached.first(), Some(&0), "nothing had begun when the recording did");
    assert!(
        reached.is_sorted() && reached.last() > Some(&0),
        "the number the session had reached at each: {reached:?}"
    );
}

/// One thing in a keyed session's stream: a whole mark, or text a command printed.
#[derive(Debug, Clone)]
struct Piece {
    bytes: Vec<u8>,
    /// A mark that carries the session's key
    keyed: bool,
    /// The number of the command this begins, for the mark that begins one
    begins: Option<u64>,
}

fn mark(bytes: String) -> Piece {
    Piece { bytes: bytes.into_bytes(), keyed: true, begins: None }
}

fn unkeyed(bytes: String) -> Piece {
    Piece { bytes: bytes.into_bytes(), keyed: false, begins: None }
}

/// What a command prints: lines of text with nothing in them a terminal reads as anything else — and no
/// `;`, so text that follows a mark cut short cannot be read as that mark's next field.
fn printed() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-z0-9 ./=-]{0,12}", 0..4)
        .prop_map(|lines| lines.iter().map(|line| format!("{line}\r\n")).collect())
}

/// One command as a keyed session's shell writes it, less its number: its line, what it printed, and its
/// end — or none, for a command whose end the shell never reported.
type Ran = (String, String, Option<(Option<i32>, String)>);

fn ran() -> impl Strategy<Value = Ran> {
    (
        "[a-z;% ]{0,16}",
        printed(),
        prop::option::weighted(0.8, (prop::option::weighted(0.8, -2i32..300), "(/[a-z;]{1,6}){0,3}")),
    )
}

/// A keyed session's own stream: commands numbered upwards, with gaps, each between the standard marks
/// its hook still prints around it.
fn session() -> impl Strategy<Value = Vec<Piece>> {
    prop::collection::vec((1u64..4, ran()), 1..6).prop_map(|commands| {
        let mut number = 0;
        let mut pieces = Vec::new();
        for (step, (line, text, end)) in commands {
            number += step;
            pieces.push(unkeyed(format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}{line}\r\n")));
            pieces.push(Piece { begins: Some(number), ..mark(start(number, &line)) });
            pieces.push(unkeyed(format!("\x1b]133;C{BEL}")));
            pieces.push(unkeyed(text));
            if let Some((status, cwd)) = end {
                let status = status.map(|status| status.to_string()).unwrap_or_default();
                pieces.push(mark(finish(number, &status, &cwd)));
                pieces.push(unkeyed(format!("\x1b]133;D;{status}{BEL}\x1b]7;file://host{cwd}{BEL}")));
            }
        }
        pieces
    })
}

fn bytes_of(pieces: &[Piece]) -> Vec<u8> {
    pieces.iter().flat_map(|piece| piece.bytes.iter().copied()).collect()
}

/// Markers written to be believed, none of which carries the session's key: the families anything can
/// print, and keyed marks with another key, with the session's key cut short, and with numbers a session
/// is likely to be at.
fn forgery() -> impl Strategy<Value = String> {
    let end = prop_oneof![Just(BEL), Just("\x1b\\")];
    let number = prop_oneof![1u64..12, any::<u64>()];
    let other =
        prop_oneof![Just(ANOTHER.to_owned()), "[0-9a-f]{32}", (0usize..32).prop_map(|kept| KEY[..kept].to_owned())];
    let body = prop_oneof![
        Just("133;A".to_owned()),
        Just("133;C".to_owned()),
        Just("133;D;0".to_owned()),
        Just("633;A".to_owned()),
        Just("633;C".to_owned()),
        Just("633;D;0".to_owned()),
        Just("633;E;rm -rf /".to_owned()),
        (number.clone(), other.clone()).prop_map(|(number, key)| format!("633;V;F;{number};{key};0;/")),
        (number.clone(), other.clone()).prop_map(|(number, key)| format!("633;V;S;{number};{key};true")),
        (number.clone(), other).prop_map(|(number, key)| format!("633;V;F;{number};{key}")),
        number.prop_map(|number| format!("633;V;F;{number};;0;/")),
    ];
    (body, end).prop_map(|(body, end)| format!("\x1b]{body}{end}"))
}

fn holds_the_key(bytes: &[u8]) -> bool {
    bytes.windows(KEY.len()).any(|window| window == KEY.as_bytes())
}

/// Anything at all that is not the key: bytes of no particular shape, forgeries, and forgeries cut short
/// so that what follows them runs on into them.
fn noise() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        prop::collection::vec(any::<u8>(), 0..48),
        forgery().prop_map(String::into_bytes),
        (forgery(), any::<prop::sample::Index>()).prop_map(|(forgery, cut)| {
            let bytes = forgery.into_bytes();
            bytes[..cut.index(bytes.len())].to_vec()
        }),
    ];
    prop::collection::vec(piece, 0..4)
        .prop_map(|pieces| pieces.concat())
        .prop_filter("the stream holds the session's key", |bytes| !holds_the_key(bytes))
}

/// A session's stream with something else written before each of its pieces and after the last.
fn among<T: Clone + std::fmt::Debug + 'static>(
    between: impl Strategy<Value = T> + 'static,
) -> impl Strategy<Value = (Vec<Piece>, Vec<T>)> {
    let between = between.boxed();
    session().prop_flat_map(move |pieces| {
        let places = pieces.len() + 1;
        (Just(pieces), prop::collection::vec(between.clone(), places..=places))
    })
}

/// Everything a keyed session is heard to say about its commands.
fn heard(screen: &mut Screen) -> (Vec<Record>, Said, u32, Vec<String>) {
    (records(screen), said(screen), screen.facts().finished, told(screen))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// **Nothing that does not carry the key changes what a keyed session records.** Whatever is written
    /// between the shell's own marks — a file shown, a log, a program trying — the commands, how each
    /// ended, whether one is running and every event that says so are what they are without it.
    #[test]
    fn what_does_not_carry_the_key_changes_nothing_a_keyed_session_records((pieces, noise) in among(noise())) {
        let mut alone = keyed();
        alone.feed(&bytes_of(&pieces));

        let mut among = keyed();
        for (piece, before) in pieces.iter().zip(&noise) {
            among.feed(before);
            among.feed(&piece.bytes);
        }
        among.feed(noise.last().expect("something follows the last piece"));

        prop_assert_eq!(heard(&mut among), heard(&mut alone));
        prop_assert_eq!(refused(&alone), 0);
    }

    /// **And a marker that does not carry the key moves no row of one.** Forgeries print nothing, so
    /// with them written anywhere between the shell's own marks each command's output is on the lines
    /// it was on and reads as it read.
    #[test]
    fn a_marker_that_does_not_carry_the_key_moves_no_row_of_a_keyed_command(
        (pieces, forgeries) in among(prop::collection::vec(forgery(), 0..4).prop_map(|forgeries| forgeries.concat())),
    ) {
        let mut alone = Screen::keyed(60, 30, key(KEY));
        let _view = viewed(&mut alone);
        alone.feed(&bytes_of(&pieces));

        let mut among = Screen::keyed(60, 30, key(KEY));
        let _view = viewed(&mut among);
        for (piece, before) in pieces.iter().zip(&forgeries) {
            among.feed(before.as_bytes());
            among.feed(&piece.bytes);
        }
        among.feed(forgeries.last().expect("something follows the last piece").as_bytes());

        prop_assert_eq!(rows(&mut among), rows(&mut alone));
        prop_assert_eq!(heard(&mut among), heard(&mut alone));
    }

    /// **A session's own stream shown again is the past.** Any beginning of what a session has printed,
    /// printed again at any later point — a log of it shown with `cat`, its recording — carries the key
    /// in every mark, and changes no command and nothing the session says: each of those marks is
    /// refused, and the session goes on to record what it would have.
    #[test]
    fn a_sessions_own_stream_shown_again_changes_nothing_and_every_mark_in_it_is_refused(
        (pieces, shown_at, shown) in session().prop_flat_map(|pieces| {
            let places = pieces.len();
            (Just(pieces), 0..=places, any::<prop::sample::Index>())
        }),
    ) {
        let (before, after) = pieces.split_at(shown_at);
        let printed = bytes_of(before);
        let again = &printed[..shown.index(printed.len() + 1)];
        // The marks that are whole in what is shown again: one cut short has not been written yet
        let ends = before.iter().scan(0, |end, piece| {
            *end += piece.bytes.len();
            Some((*end, piece.keyed))
        });
        let whole_marks = ends.filter(|(end, keyed)| *keyed && *end <= again.len()).count();

        let mut screen = keyed();
        screen.feed(&printed);
        let (was, counted) = (heard(&mut screen), refused(&screen));
        screen.feed(again);
        let now = heard(&mut screen);
        prop_assert_eq!((&now.0, &now.1, now.2), (&was.0, &was.1, was.2));
        prop_assert_eq!(now.3, Vec::<String>::new(), "a mark shown again was told as a command");
        prop_assert_eq!(refused(&screen) - counted, whole_marks as u32);

        // What the session goes on to record is what it would have with nothing shown again
        screen.feed(&bytes_of(after));
        let mut alone = keyed();
        alone.feed(&bytes_of(&pieces));
        prop_assert_eq!((records(&mut screen), said(&screen)), (records(&mut alone), said(&alone)));
    }

}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// **A replay may begin at any keyframe, and the commands it rebuilds are the ones that were.** A
    /// session is recorded with a keyframe after almost every read, while forgeries are written between
    /// its marks and its own stream is shown again part way through. Replayed from each keyframe in
    /// turn, the recording gives back every command that began after that keyframe, ended as the
    /// session recorded it — and nothing a forgery or the past shown again would have added.
    #[test]
    fn a_replay_from_any_keyframe_rebuilds_the_commands_recorded_since_it(
        (pieces, noise) in among(noise()),
        (shown_at, shown) in (any::<prop::sample::Index>(), any::<prop::sample::Index>()),
        cuts in prop::collection::vec(1usize..40, 1..50),
    ) {
        let shown_at = shown_at.index(pieces.len() + 1);
        let printed = bytes_of(&pieces[..shown_at]);
        let mut written = Vec::new();
        for (place, (piece, before)) in pieces.iter().zip(&noise).enumerate() {
            if place == shown_at {
                written.extend_from_slice(&printed[..shown.index(printed.len() + 1)]);
            }
            written.extend_from_slice(before);
            written.extend_from_slice(&piece.bytes);
        }
        written.extend_from_slice(noise.last().expect("something follows the last piece"));

        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("one.recording");
        let mut live = Screen::keyed(60, 12, key(KEY));
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

        // What the session recorded, as a recording of it ends: nothing is running a command any more
        let ended = |screen: &mut Screen| -> Vec<(Option<String>, Option<String>, CommandOutcome)> {
            let mut all: Vec<_> = records(screen).into_iter().map(|(_, line, cwd, outcome)| (line, cwd, outcome)).collect();
            if let Some((.., outcome @ CommandOutcome::Running)) = all.last_mut() {
                *outcome = CommandOutcome::Unknown { why: Unreported::SessionEnded };
            }
            all
        };
        let recorded_live = ended(&mut live);
        let numbers: Vec<u64> = pieces.iter().filter_map(|piece| piece.begins).collect();
        prop_assert_eq!(recorded_live.len(), numbers.len());

        let entries = recorded(&mut live, &path);
        let keyframes: Vec<usize> = (0..entries.len())
            .filter(|index| matches!(entries[*index].entry, Entry::Keyframe { .. }))
            .collect();
        prop_assert_eq!(keyframes.first(), Some(&0));
        for from in keyframes {
            let Some(Timed { entry: Entry::Marks { last, .. }, .. }) = entries.get(from + 1) else {
                return Err(TestCaseError::fail(format!("the keyframe at entry {from} names no key after it")));
            };
            let since = numbers.iter().filter(|number| *number > last).count();
            let mut replayed = Screen::new(60, 12);
            prop_assert!(replayed.replay_all(&entries[from..]));
            prop_assert_eq!(
                ended(&mut replayed),
                recorded_live[recorded_live.len() - since..].to_vec(),
                "replayed from the keyframe at entry {}, where the session had reached {}", from, last
            );
        }
    }
}
