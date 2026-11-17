//! What a session's programs say they are doing (OSC 7501), as the broker keeps it.
//!
//! The core reads a report and holds it to the specification. What these cases hold is what the broker
//! does with one it is handed: the rules a record is kept and let go by, which every client is spared
//! because it is sent the records whole.

use std::time::{Duration, SystemTime};

use apack_terminal::markers::MarksKey;
use apack_terminal::recording::{Entry, Timed};
use apack_terminal::screen::Screen;
use apack_terminal::status::{CAP, Confirmed, Kind, Record, State, confirmed};

use crate::screens::keyframe;

const BEL: &str = "\x07";
const KEY: &str = "0123456789abcdef0123456789abcdef";

/// One report, as a program writes it.
fn report(body: &str) -> String {
    format!("\x1b]7501;{body}\x1b\\")
}

fn fed(stream: &str) -> Screen {
    let mut screen = Screen::new(80, 24);
    screen.feed(stream.as_bytes());
    screen
}

/// Each record as `(id, state)`, in the order a client is told them.
fn states(screen: &Screen) -> Vec<(String, State)> {
    screen.statuses().into_iter().map(|record| (record.id, record.state)).collect()
}

fn the(screen: &Screen, id: &str) -> Record {
    screen.statuses().into_iter().find(|record| record.id == id).unwrap_or_else(|| panic!("no record {id:?}"))
}

fn args(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|argument| (*argument).to_owned()).collect()
}

#[test]
fn a_report_is_kept_as_what_it_said() {
    // "Apply?" in base64, as the specification's own example sends it
    let screen = fed(&report("state=blocked:kind=permission:app=terraform:progress=40:msg=QXBwbHk/"));
    let record = the(&screen, "");
    assert_eq!(record.state, State::Blocked);
    assert_eq!(record.kind, Some(Kind::Permission));
    assert_eq!(record.progress, Some(40));
    assert_eq!(record.app.as_deref(), Some("terraform"));
    assert_eq!(record.message.as_deref(), Some("Apply?"));
    assert_eq!(record.title, None, "it sent none");
}

/// A report replaces its record: what the next one leaves out is gone, not kept from the one before.
#[test]
fn a_report_replaces_its_record_whole() {
    let mut screen = fed(&report("state=working:app=build:progress=10:msg=QXBwbHk/"));
    screen.feed(report("state=working").as_bytes());
    let record = the(&screen, "");
    assert_eq!((record.app, record.progress, record.message), (None, None, None));
    assert_eq!(screen.statuses().len(), 1, "the same record, replaced");
}

/// When a record entered its state is kept through a report that keeps the state, and moves with one
/// that changes it: "blocked for four minutes" is counted from when it was first blocked.
#[test]
fn a_record_keeps_when_it_entered_its_state_until_the_state_changes() {
    let mut screen = fed(&report("state=working:progress=1"));
    let entered = the(&screen, "").since;
    std::thread::sleep(Duration::from_millis(5));
    screen.feed(report("state=working:progress=2").as_bytes());
    assert_eq!(the(&screen, "").since, entered, "still working");
    screen.feed(report("state=blocked").as_bytes());
    assert!(the(&screen, "").since > entered, "a new state began later");
}

/// Later is larger: what "has the person seen this" is compared on.
#[test]
fn every_report_that_changes_a_record_moves_the_count() {
    let mut screen = fed(&report("state=working:id=a"));
    let first = the(&screen, "a").changed;
    screen.feed(report("state=done:id=b").as_bytes());
    assert!(the(&screen, "b").changed > first);
    screen.feed(report("state=done:id=a").as_bytes());
    assert!(the(&screen, "a").changed > the(&screen, "b").changed);
}

#[test]
fn a_clear_takes_a_record_and_everything_beneath_it() {
    let mut screen =
        fed(&["state=working", "state=working:id=build", "state=working:id=build/test", "state=working:id=builder"]
            .map(report)
            .concat());
    screen.feed(report("state=clear:id=build").as_bytes());
    assert_eq!(
        states(&screen),
        vec![(String::new(), State::Working), ("builder".to_owned(), State::Working)],
        "`builder` is not beneath `build`"
    );
    screen.feed(report("state=clear").as_bytes());
    assert_eq!(states(&screen), vec![], "a clear that names none takes them all");
}

/// A record that names no program is told with its nearest ancestor's, and the program's own record is
/// every record's ancestor.
#[test]
fn a_record_without_a_program_is_told_with_its_nearest_ancestors() {
    let screen = fed(&[
        "state=working:app=deploy",
        "state=working:id=us-east",
        "state=working:id=build:app=cargo",
        "state=working:id=build/test",
    ]
    .map(report)
    .concat());
    assert_eq!(the(&screen, "us-east").app.as_deref(), Some("deploy"));
    assert_eq!(the(&screen, "build/test").app.as_deref(), Some("cargo"), "the nearer one");
}

#[test]
fn the_record_changed_longest_ago_is_let_go_past_the_cap() {
    let mut stream = String::new();
    for index in 0..=CAP {
        stream.push_str(&report(&format!("state=working:id=r{index}")));
    }
    let mut screen = fed(&stream);
    assert_eq!(screen.statuses().len(), CAP);
    assert!(screen.statuses().iter().all(|record| record.id != "r0"), "the first is the one that went");
    // Touched again, `r1` is no longer the oldest: `r2` goes for the next
    screen.feed([report("state=working:id=r1:progress=1"), report("state=working:id=more")].concat().as_bytes());
    let ids: Vec<String> = screen.statuses().into_iter().map(|record| record.id).collect();
    assert!(ids.contains(&"r1".to_owned()) && !ids.contains(&"r2".to_owned()));
}

const EVERY_STATE: [&str; 5] =
    ["state=idle:id=i", "state=working:id=w", "state=blocked:id=b", "state=done:id=d", "state=error:id=e"];

fn finished() -> Vec<(String, State)> {
    vec![("d".to_owned(), State::Done), ("e".to_owned(), State::Error)]
}

/// The program that was reporting ended with its command: what it was in the middle of is over, and
/// what it finished or failed at waits for the person.
#[test]
fn a_command_ending_takes_what_was_under_way_and_keeps_what_finished() {
    let mut screen = fed(&format!("\x1b]133;C{BEL}{}", EVERY_STATE.map(report).concat()));
    assert_eq!(screen.statuses().len(), 5);
    screen.feed(format!("\x1b]133;A{BEL}").as_bytes());
    assert_eq!(states(&screen), finished());
}

/// In a session whose commands are taken from keyed marks, a prompt mark is something any program can
/// print, and it ends nothing; the finish that carries the key does.
#[test]
fn a_keyed_session_lets_go_only_on_a_finish_that_carries_its_key() {
    let mut screen = Screen::keyed(80, 24, MarksKey::new(KEY).expect("a key"));
    screen.feed(format!("\x1b]633;V;S;1;{KEY};codex{BEL}{}", EVERY_STATE.map(report).concat()).as_bytes());
    screen.feed(format!("\x1b]133;D;0{BEL}\x1b]133;A{BEL}\x1b]633;A{BEL}").as_bytes());
    assert_eq!(screen.statuses().len(), 5, "forged marks ended nothing");
    screen.feed(format!("\x1b]633;V;F;1;{KEY};0;/tmp{BEL}").as_bytes());
    assert_eq!(states(&screen), finished());
}

#[test]
fn a_session_ending_takes_what_was_under_way_whether_or_not_a_mark_said_so() {
    let mut screen = fed(&EVERY_STATE.map(report).concat());
    screen.session_ended();
    assert_eq!(states(&screen), finished());
}

/// A full reset is the terminal put back as it started, and the core says so as a clear.
#[test]
fn a_full_reset_takes_every_record() {
    let mut screen = fed(&EVERY_STATE.map(report).concat());
    screen.feed(b"\x1bc");
    assert_eq!(states(&screen), vec![]);
}

/// A program that asks is answered, with the states and kinds this terminal takes. Claude Code reports
/// nothing until it is.
#[test]
fn a_program_that_asks_is_told_the_protocol_is_read() {
    let mut screen = fed("\x1b]7501;?\x1b\\");
    assert_eq!(
        screen.take_replies(),
        vec!["\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\".to_owned()]
    );
}

/// The records are told once for what one read changed, however many reports it held, and not again
/// until something changes.
#[test]
fn what_one_read_changed_is_told_once() {
    let mut screen = fed(&[report("state=working"), report("state=blocked"), report("state=working:id=a")].concat());
    let told = screen.take_statuses(None).expect("the read changed them");
    assert_eq!(told.len(), 2);
    assert_eq!(screen.take_statuses(None), None, "nothing since");
    // Saying again what a record already says changes nothing
    screen.feed(report("state=blocked").as_bytes());
    assert_eq!(screen.take_statuses(None), None);
    screen.feed(report("state=clear:id=nothing-by-this-name").as_bytes());
    assert_eq!(screen.take_statuses(None), None, "a clear that took nothing");
    screen.feed(report("state=clear").as_bytes());
    assert_eq!(screen.take_statuses(None), Some(vec![]), "the last record going is a change");
}

/// A record is checked against the program in front when it is taken, and keeps that answer: what is
/// in front later is not what wrote it.
#[test]
fn a_record_is_checked_once_against_the_program_in_front_when_it_is_taken() {
    let mut screen = fed(&[report("state=working:app=codex"), report("state=working:id=other:app=cargo")].concat());
    let codex = args(&["/usr/local/lib/codex/bin/codex", "-s", "read-only"]);
    let told = screen.take_statuses(Some(&codex)).expect("changed");
    assert_eq!(told[0].confirmed, Confirmed::Matched);
    assert_eq!(told[1].confirmed, Confirmed::Mismatched, "codex is in front, and this one says cargo");

    screen.feed(report("state=done:id=later:app=codex").as_bytes());
    let told = screen.take_statuses(Some(&args(&["cat", "codex"]))).expect("changed");
    assert_eq!(
        told.iter().find(|record| record.id.is_empty()).map(|record| record.confirmed),
        Some(Confirmed::Matched)
    );
    assert_eq!(
        told.iter().find(|record| record.id == "later").map(|record| record.confirmed),
        Some(Confirmed::Mismatched)
    );
}

/// A record that names no program of its own is checked as the one it inherits.
#[test]
fn a_record_is_checked_as_the_program_it_inherits() {
    let mut screen = fed(&[report("state=working:app=deploy"), report("state=working:id=us-east")].concat());
    let told = screen.take_statuses(Some(&args(&["deploy"]))).expect("changed");
    assert_eq!(told[1].confirmed, Confirmed::Matched);
}

#[test]
fn the_program_in_front_is_what_it_was_run_as_or_the_script_an_interpreter_runs() {
    let matched = |app: &str, argv: &[&str]| confirmed(Some(app), Some(&args(argv)));
    assert_eq!(matched("codex", &["/opt/bin/codex"]), Confirmed::Matched);
    assert_eq!(matched("Codex", &["codex.exe"]), Confirmed::Matched, "case, and a Windows suffix");
    assert_eq!(matched("codex", &["C:\\tools\\codex.exe"]), Confirmed::Matched);
    assert_eq!(matched("zsh", &["-zsh"]), Confirmed::Matched, "a login shell");
    assert_eq!(matched("claude-code", &["claude"]), Confirmed::Matched, "run by another name than it reports");
    assert_eq!(matched("claude", &["claude"]), Confirmed::Matched);
    assert_eq!(matched("codex", &["node", "--no-warnings", "/lib/codex/bin/codex.js", "resume"]), Confirmed::Matched);
    assert_eq!(matched("node", &["node"]), Confirmed::Matched, "an interpreter with no script is itself");

    assert_eq!(matched("codex", &["cat", "codex"]), Confirmed::Mismatched, "an argument is not the program");
    assert_eq!(matched("codex", &["node", "other.js", "codex"]), Confirmed::Mismatched);
    assert_eq!(matched("claude", &["claude-code"]), Confirmed::Mismatched, "an alias reads one way");
    assert_eq!(matched("codex", &["codex-helper"]), Confirmed::Mismatched);
}

#[test]
fn nothing_to_compare_is_unknown_and_never_a_mismatch() {
    assert_eq!(confirmed(None, Some(&args(&["codex"]))), Confirmed::Unknown, "the record names no program");
    assert_eq!(confirmed(Some("codex"), None), Confirmed::Unknown, "no leader was read");
    assert_eq!(confirmed(Some("codex"), Some(&[])), Confirmed::Unknown);
    assert_eq!(confirmed(Some("codex"), Some(&args(&[""]))), Confirmed::Unknown);
    let mut screen = fed(&report("state=working:app=codex"));
    assert_eq!(screen.take_statuses(None).expect("changed")[0].confirmed, Confirmed::Unknown);
}

/// What a program said it was doing in a recording is not what anything is doing now.
#[test]
fn a_recording_replayed_holds_no_record_and_answers_nothing() {
    let recorded = format!("{}{}{}", "\x1b]7501;?\x1b\\", report("state=blocked:kind=permission:app=codex"), "text");
    let entries =
        [Timed { at: 1, entry: keyframe(80, 24, b"") }, Timed { at: 2, entry: Entry::Output(recorded.into_bytes()) }];
    let mut screen = Screen::new(80, 24);
    assert!(screen.replay_all(&entries));
    assert_eq!(states(&screen), vec![]);
    assert_eq!(screen.take_statuses(None), None);
    assert_eq!(screen.take_replies(), Vec::<String>::new());
    // And what the last shell's programs said is not carried into the next one's
    let mut live = fed(&report("state=done:app=codex"));
    live.settle_restored();
    assert_eq!(states(&live), vec![]);
}

/// The states a program itself went through, from a fixture of what one really sent: each line the body
/// of one OSC 7501 sequence, a `?` being the program asking.
fn walked(fixture: &str) -> (Screen, Vec<Option<State>>, usize) {
    let mut screen = Screen::new(80, 24);
    let (mut walked, mut asked) = (Vec::new(), 0);
    for body in fixture.lines() {
        screen.feed(report(body).as_bytes());
        if body == "?" {
            asked += screen.take_replies().len();
        } else {
            walked.push(screen.status());
        }
    }
    (screen, walked, asked)
}

#[test]
fn what_codex_sent_is_kept_as_it_went() {
    use State::{Blocked, Done, Idle, Working};
    let (screen, walked, asked) = walked(include_str!("fixtures/program-status/codex-0.167.0.txt"));
    assert_eq!(
        walked,
        [
            // A prompt it answered, and one whose command its sandbox refused
            Some(Idle),
            Some(Working),
            Some(Idle),
            Some(Working),
            Some(Done),
            Some(Idle),
            None,
            // A prompt whose command needed approval, which was denied
            Some(Idle),
            Some(Working),
            Some(Idle),
            Some(Working),
            Some(Blocked),
            Some(Working),
            Some(Idle),
            None,
        ]
    );
    assert_eq!(asked, 0, "Codex reports without asking");
    assert_eq!(states(&screen), vec![], "it cleared on its way out");
}

#[test]
fn what_claude_code_sent_is_kept_as_it_went() {
    use State::{Blocked, Idle, Working};
    let fixture = include_str!("fixtures/program-status/claude-code-2.1.326.txt");
    let (_, walked, asked) = walked(fixture);
    assert_eq!(walked, [Some(Idle), Some(Working), Some(Blocked), Some(Working), None]);
    assert_eq!(asked, 1, "it asked first, and was answered");

    let mut screen = Screen::new(80, 24);
    for body in fixture.lines().take(4) {
        screen.feed(report(body).as_bytes());
    }
    let blocked = the(&screen, "");
    assert_eq!(blocked.kind, Some(Kind::Auth));
    assert_eq!(blocked.app.as_deref(), Some("claude-code"));
    assert_eq!(blocked.message.as_deref(), Some("Login reached its time limit · Please run /login"));
}

/// When a record is said to have entered its state is the time a client is told, in milliseconds.
#[test]
fn a_client_is_told_when_in_milliseconds_since_the_epoch() {
    let before = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("after 1970").as_millis();
    let screen = fed(&report("state=working"));
    let told: apack_terminal::protocol::StatusRecord = the(&screen, "").into();
    let after = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("after 1970").as_millis();
    assert!((before..=after).contains(&u128::from(told.since_ms)));
}
