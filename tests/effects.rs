//! What a program asks of the world outside its screen, reported as it happens.
//!
//! Each of these is read by the core and handed on. None is drawn, and none is acted on here: a title is
//! for whatever labels the session, a clipboard write is for whoever decides the person meant it.

use apack_terminal::commands::Outcome;
use apack_terminal::screen::Screen;
use apack_terminal::vt::{Effect, Progress};

fn effects_after(stream: &[u8]) -> Vec<Effect> {
    let mut screen = Screen::new(80, 24);
    screen.feed(stream);
    screen.take_effects()
}

#[test]
fn a_title_is_reported_each_time_the_program_sets_one() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b]0;first\x07");
    assert_eq!(screen.take_effects(), vec![Effect::Title("first".to_owned())]);
    screen.feed(b"\x1b]2;second\x1b\\");
    assert_eq!(screen.take_effects(), vec![Effect::Title("second".to_owned())]);
    assert_eq!(effects_after(b"\x1b]2;\x07"), vec![Effect::Title(String::new())], "a name taken back");
}

/// **What a program says faster than anyone can be told is told as where it ended up.** Inside one read
/// a bell rung a thousand times is a bell, and a title or a progress report replaced before it could
/// have been shown is the one that replaced it. What is not a state — a notification, a clipboard write,
/// a command starting — is each told, where it happened among the rest.
#[test]
fn what_a_program_repeats_within_one_read_is_told_once() {
    let mut stream = Vec::new();
    stream.extend_from_slice(b"\x1b]0;first\x07");
    stream.extend_from_slice(&[0x07; 1_000]);
    stream.extend_from_slice(b"\x1b]9;4;1;10\x07\x1b]9;one\x07\x1b]2;second\x07");
    stream.extend_from_slice(&[0x07; 1_000]);
    stream.extend_from_slice(b"\x1b]9;4;1;90\x07\x1b]9;two\x07\x1b]2;third\x07");
    assert_eq!(
        effects_after(&stream),
        vec![
            Effect::Bell,
            Effect::Notification { title: String::new(), body: "one".to_owned() },
            Effect::Progress { state: Progress::Running, percent: Some(90) },
            Effect::Notification { title: String::new(), body: "two".to_owned() },
            Effect::Title("third".to_owned()),
        ]
    );
}

#[test]
fn a_bell_is_reported_and_one_that_ends_a_sequence_is_not() {
    assert_eq!(effects_after(b"ding\x07"), vec![Effect::Bell]);
    assert_eq!(effects_after(b"\x1b]0;t\x07"), vec![Effect::Title("t".to_owned())]);
}

/// OSC 52: the text arrives decoded, and nothing here puts it anywhere.
#[test]
fn a_clipboard_write_is_reported_as_the_text_it_carries() {
    assert_eq!(effects_after(b"\x1b]52;c;aGVsbG8gd29ybGQ=\x07"), vec![Effect::Clipboard("hello world".to_owned())]);
    assert_eq!(effects_after(b"\x1b]52;c;not base64!\x07"), vec![], "what does not decode is not a write");
    // **Only the clipboard.** The selection — what a middle click pastes — is another place, and a write
    // to it told as a clipboard write would put what a program selected over what the person copied
    assert_eq!(effects_after(b"\x1b]52;p;aGVsbG8gd29ybGQ=\x07"), vec![], "the primary selection");
    assert_eq!(effects_after(b"\x1b]52;s;aGVsbG8gd29ybGQ=\x07"), vec![], "the selection");
}

#[test]
fn a_notification_is_reported_with_its_title_where_it_has_one() {
    assert_eq!(
        effects_after(b"\x1b]9;build finished\x07"),
        vec![Effect::Notification { title: String::new(), body: "build finished".to_owned() }]
    );
    assert_eq!(
        effects_after(b"\x1b]777;notify;Tests;all green\x07"),
        vec![Effect::Notification { title: "Tests".to_owned(), body: "all green".to_owned() }]
    );
}

#[test]
fn progress_is_reported_as_the_program_states_it() {
    let stated: Vec<Vec<Effect>> =
        [&b"\x1b]9;4;1;40\x07"[..], b"\x1b]9;4;3\x07", b"\x1b]9;4;2;90\x07", b"\x1b]9;4;0\x07"]
            .into_iter()
            .map(effects_after)
            .collect();
    assert_eq!(
        stated.concat(),
        vec![
            Effect::Progress { state: Progress::Running, percent: Some(40) },
            Effect::Progress { state: Progress::Indeterminate, percent: None },
            Effect::Progress { state: Progress::Error, percent: Some(90) },
            Effect::Progress { state: Progress::None, percent: None },
        ]
    );
}

/// The standard prompt marks (OSC 133) push the same two events the 633 family does, and a command's end
/// is said once however the shell reported it.
#[test]
fn a_command_starting_and_ending_are_pushed_once_each() {
    let stream = b"\x1b]133;A\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07\x1b]133;D;1\x07\x1b]133;A\x07$ ";
    assert_eq!(
        effects_after(stream),
        vec![Effect::CommandStarted { command: None }, Effect::CommandFinished { outcome: Outcome::Claimed(Some(1)) }],
        "the prompt after a reported end is the same command"
    );
    // A prompt with no end reported still ends what was running, with no status rather than a zero
    assert_eq!(
        effects_after(b"\x1b]133;C\x07\x1b]133;A\x07"),
        vec![Effect::CommandStarted { command: None }, Effect::CommandFinished { outcome: Outcome::Claimed(None) }]
    );
}

#[test]
fn effects_are_taken_once() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x07");
    assert_eq!(screen.take_effects(), vec![Effect::Bell]);
    assert_eq!(screen.take_effects(), vec![]);
}

/// The last name a program gave is kept for whoever lists the sessions, and a name taken back leaves none.
#[test]
fn the_last_title_is_remembered_until_the_program_takes_it_back() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(screen.title(), None);
    screen.feed(b"\x1b]0;first\x07\x1b]2;second\x07");
    screen.take_effects();
    assert_eq!(screen.title(), Some("second"));
    screen.feed(b"\x1b]2;\x07");
    screen.take_effects();
    assert_eq!(screen.title(), None);
}
