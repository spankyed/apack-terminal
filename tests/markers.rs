//! What a shell says about itself in the family the core does not read (OSC 633), read out of a session.
//!
//! The core finds each sequence — where it starts and ends, across however many reads — and hands it over
//! whole. So these drive a `Screen`: what is being claimed is that a session ends up knowing what the shell
//! said, not that some scanner found a marker.

use apack_terminal::commands::Outcome;
use apack_terminal::markers::PAYLOAD_CAP;
use apack_terminal::screen::{Facts, Screen};
use apack_terminal::vt::Effect;

const BEL: &str = "\x07";

fn facts_after(stream: &str) -> Facts {
    let mut screen = Screen::new(80, 24);
    screen.feed(stream.as_bytes());
    screen.facts().clone()
}

fn effects_after(stream: &str) -> Vec<Effect> {
    let mut screen = Screen::new(80, 24);
    screen.feed(stream.as_bytes());
    screen.take_effects()
}

#[test]
fn reads_the_markers_a_prompt_and_a_command_emit() {
    let stream = format!(
        "\x1b]633;A{BEL}$ \x1b]633;B{BEL}ls\r\n\x1b]633;E;ls -la{BEL}\x1b]633;C{BEL}a.txt\r\n\x1b]633;D;0{BEL}"
    );
    assert_eq!(
        effects_after(&stream),
        vec![
            Effect::CommandStarted { command: Some("ls -la".to_owned()) },
            Effect::CommandFinished { outcome: Outcome::Claimed(Some(0)) },
        ]
    );
    let facts = facts_after(&stream);
    assert_eq!(
        (facts.command.as_deref(), facts.running, facts.outcome, facts.finished),
        (Some("ls -la"), false, Some(Outcome::Claimed(Some(0))), 1)
    );
}

/// A shell that reports no status is reporting that it has none, which is not the same as zero.
#[test]
fn tells_a_missing_status_from_a_zero_one() {
    let ended = |finish: &str| facts_after(&format!("\x1b]633;C{BEL}\x1b]633;{finish}{BEL}")).outcome;
    assert_eq!(ended("D"), Some(Outcome::Claimed(None)));
    assert_eq!(ended("D;0"), Some(Outcome::Claimed(Some(0))));
    assert_eq!(ended("D;130"), Some(Outcome::Claimed(Some(130))));
}

#[test]
fn reads_a_working_directory_from_every_sequence_that_carries_one() {
    for stream in
        [format!("\x1b]633;P;Cwd=/home/me/a%20project{BEL}"), format!("\x1b]7;file://host/home/me/a%20project{BEL}")]
    {
        assert_eq!(facts_after(&stream).cwd.as_deref(), Some("/home/me/a project"), "{stream:?}");
        assert_eq!(effects_after(&stream), vec![Effect::CwdChanged("/home/me/a project".to_owned())]);
    }
}

/// A directory reported again is not a change, so a prompt that reports it every time says it once.
#[test]
fn a_working_directory_reported_again_is_not_a_change() {
    let stream = format!("\x1b]633;P;Cwd=/tmp{BEL}\x1b]633;P;Cwd=/tmp{BEL}\x1b]7;file://host/tmp{BEL}");
    assert_eq!(effects_after(&stream), vec![Effect::CwdChanged("/tmp".to_owned())]);
}

#[test]
fn reads_a_string_terminated_the_other_way() {
    assert_eq!(facts_after("\x1b]633;P;Cwd=/tmp\x1b\\").cwd.as_deref(), Some("/tmp"));
}

#[test]
fn ignores_sequences_that_are_not_ours() {
    let facts = facts_after(&format!("\x1b]633;Z;what{BEL}\x1b]633;P;Other=1{BEL}\x1b]7400;status=busy{BEL}"));
    assert_eq!(facts, Facts::default());
}

/// However the stream is cut, the same things are learned: the core carries a sequence across reads.
#[test]
fn reads_the_same_markers_however_the_stream_is_chunked() {
    let stream = format!("\x1b]633;E;make test{BEL}\x1b]633;C{BEL}ok\r\n\x1b]633;D;2{BEL}\x1b]633;P;Cwd=/srv{BEL}");
    let whole = facts_after(&stream);
    assert_eq!(
        (whole.command.as_deref(), whole.outcome, whole.cwd.as_deref()),
        (Some("make test"), Some(Outcome::Claimed(Some(2))), Some("/srv"))
    );

    let bytes = stream.as_bytes();
    for at in 1..bytes.len() {
        let mut screen = Screen::new(80, 24);
        screen.feed(&bytes[..at]);
        screen.feed(&bytes[at..]);
        assert_eq!(screen.facts(), &whole, "split at {at}");
    }
    let mut screen = Screen::new(80, 24);
    for byte in bytes {
        screen.feed(&[*byte]);
    }
    assert_eq!(screen.facts(), &whole, "a byte at a time");
}

/// **A ceiling on what a program can make the broker hold.** Content up to the cap is read; past it the
/// sequence is cut short and not read at all — half a path is not a claim about a directory — and the
/// next one is read as usual.
#[test]
fn content_up_to_the_cap_is_read_and_longer_is_not() {
    let prefix = "633;P;Cwd=/";
    let fits = "d".repeat(PAYLOAD_CAP - prefix.len());
    assert_eq!(facts_after(&format!("\x1b]{prefix}{fits}{BEL}")).cwd, Some(format!("/{fits}")));

    let over = "d".repeat(PAYLOAD_CAP);
    let stream = format!("\x1b]{prefix}{over}{BEL}\x1b]633;C{BEL}");
    let facts = facts_after(&stream);
    assert_eq!(facts.cwd, None, "a path cut short was read as a path");
    assert!(facts.running, "and the sequence after it was still read");
}

/// A sequence that never ends is abandoned by whatever does end it, and what follows is read.
#[test]
fn a_sequence_that_never_ends_does_not_take_the_next_with_it() {
    let facts = facts_after(&format!("\x1b]633;P;Cwd=/never\x18\x1b]633;C{BEL}"));
    assert_eq!(facts.cwd, None);
    assert!(facts.running);
}

#[test]
fn a_percent_escape_is_decoded_and_one_with_no_pair_is_kept_as_itself() {
    let line = |written: &str| facts_after(&format!("\x1b]633;E;{written}{BEL}")).command;
    assert_eq!(line("%41bc").as_deref(), Some("Abc"));
    assert_eq!(line("cd 50%254").as_deref(), Some("cd 50%4"));
    assert_eq!(line("cd 50%").as_deref(), Some("cd 50%"));
    // An escape inside a command line arrives escaped, which is how the shell's script writes one
    assert_eq!(line("printf '%1b[31m'").as_deref(), Some("printf '\x1b[31m'"));
    // **Two hexadecimal digits and nothing else.** A sign is not a digit, though a number parser takes
    // one: `%+1` is a percent, a plus and a one, as the shell wrote them
    assert_eq!(line("echo %+1 %-1 %+f").as_deref(), Some("echo %+1 %-1 %+f"));
    assert_eq!(line("100%, %zz, %4").as_deref(), Some("100%, %zz, %4"));
    // A pair that ends the line is a pair, and one in either case
    assert_eq!(line("a%2f%2Fb%7e").as_deref(), Some("a//b~"));
}
