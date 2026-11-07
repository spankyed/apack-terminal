//! What a shell says about itself, read out of the bytes it wrote.

use apack_terminal::markers::{Marker, Markers};

/// Everything the scanner finds in one stream, fed whole.
fn markers(stream: &str) -> Vec<Marker> {
    Markers::default().feed(stream.as_bytes())
}

/// The same stream, fed one byte at a time.
fn byte_at_a_time(stream: &str) -> Vec<Marker> {
    let mut scanner = Markers::default();
    stream
        .as_bytes()
        .iter()
        .flat_map(|byte| scanner.feed(&[*byte]))
        .collect()
}

/// The same stream, split at `at`.
fn split_at(stream: &str, at: usize) -> Vec<Marker> {
    let mut scanner = Markers::default();
    let bytes = stream.as_bytes();
    let mut found = scanner.feed(&bytes[..at]);
    found.extend(scanner.feed(&bytes[at..]));
    found
}

const BEL: &str = "\x07";

#[test]
fn reads_the_markers_a_prompt_and_a_command_emit() {
    let stream = format!(
        "\x1b]633;A{BEL}$ \x1b]633;B{BEL}ls\r\n\x1b]633;E;ls -la{BEL}\x1b]633;C{BEL}a.txt\r\n\x1b]633;D;0{BEL}"
    );
    assert_eq!(
        markers(&stream),
        vec![
            Marker::PromptStart,
            Marker::PromptEnd,
            Marker::CommandLine("ls -la".into()),
            Marker::CommandStart,
            Marker::CommandDone(Some(0)),
        ]
    );
}

/// **A status the shell did not report is not zero**, which is the distinction `Option` is carrying: a
/// `D` with no argument says the command ended and says nothing about how.
#[test]
fn tells_a_missing_status_from_a_zero_one() {
    assert_eq!(
        markers(&format!("\x1b]633;D{BEL}")),
        vec![Marker::CommandDone(None)]
    );
    assert_eq!(
        markers(&format!("\x1b]633;D;0{BEL}")),
        vec![Marker::CommandDone(Some(0))]
    );
    assert_eq!(
        markers(&format!("\x1b]633;D;130{BEL}")),
        vec![Marker::CommandDone(Some(130))]
    );
}

#[test]
fn reads_a_working_directory_from_every_sequence_that_carries_one() {
    // OSC 633's own property
    assert_eq!(
        markers(&format!("\x1b]633;P;Cwd=/home/me/a project{BEL}")),
        vec![Marker::Cwd("/home/me/a project".into())]
    );
    // OSC 7, whose payload is a URL and so is encoded
    assert_eq!(
        markers(&format!("\x1b]7;file://host/home/me/a%20project{BEL}")),
        vec![Marker::Cwd("/home/me/a project".into())]
    );
    // iTerm2's, for a user whose configuration already emits it
    assert_eq!(
        markers(&format!("\x1b]1337;CurrentDir=/tmp{BEL}")),
        vec![Marker::Cwd("/tmp".into())]
    );
}

/// A Windows OSC 7 URL keeps the shape the app's own reader expects, slash and drive letter included: the
/// scanner reports what was claimed and does not rewrite it into a path for a platform it is not on.
#[test]
fn leaves_a_windows_url_as_the_claim_it_is() {
    assert_eq!(
        markers(&format!("\x1b]7;file://PC/C:/Users/me{BEL}")),
        vec![Marker::Cwd("/C:/Users/me".into())]
    );
}

/// **ST terminates a string as well as BEL**, and a shell that uses it is not unusual.
#[test]
fn reads_a_string_terminated_the_other_way() {
    assert_eq!(
        markers("\x1b]633;P;Cwd=/tmp\x1b\\"),
        vec![Marker::Cwd("/tmp".into())]
    );
}

/// An `ESC` inside a payload that is not a terminator is payload.
#[test]
fn keeps_an_escape_that_does_not_terminate() {
    assert_eq!(
        markers(&format!("\x1b]633;E;printf '\x1b[31m'{BEL}")),
        vec![Marker::CommandLine("printf '\x1b[31m'".into())]
    );
}

#[test]
fn ignores_sequences_that_are_not_ours() {
    // A title, a colour, a CSI, and an OSC 633 code with no meaning here
    let stream = format!("\x1b]0;a title{BEL}\x1b]11;?{BEL}\x1b[31mred\x1b[0m\x1b]633;Z;what{BEL}");
    assert_eq!(markers(&stream), vec![]);
}

/// **A program that opens an OSC and never closes it must not grow the buffer**, and the scanner must come
/// back to its senses at the next string. The abandoned payload's tail is not read as a marker either.
#[test]
fn abandons_a_payload_that_never_ends_and_recovers() {
    let forever = "x".repeat(64 * 1024);
    let stream = format!("\x1b]633;E;{forever}{BEL}\x1b]633;P;Cwd=/tmp{BEL}");
    assert_eq!(markers(&stream), vec![Marker::Cwd("/tmp".into())]);
}

/// The same, where the overlong string is ended with ST rather than BEL — the path that reads the tail of
/// an abandoned payload if the overrun state pushes the escape onto it.
#[test]
fn recovers_from_an_overlong_payload_ended_with_st() {
    let forever = "x".repeat(64 * 1024);
    let stream = format!("\x1b]633;E;{forever}\x1b\\\x1b]633;A{BEL}");
    assert_eq!(markers(&stream), vec![Marker::PromptStart]);
}

/// **The property the scanner exists to satisfy**, and the one a wrapper that buffered whole chunks would
/// pass every other test without: a marker split at any byte reads the same as one that arrived whole.
/// A 23-byte `P;Cwd=` sequence very often is split, because a boundary falls wherever `read` returned.
#[test]
fn reads_the_same_markers_however_the_stream_is_chunked() {
    let stream = format!(
        "ls\r\n\x1b]633;A{BEL}$ \x1b]633;B{BEL}\x1b]633;E;cargo test{BEL}\x1b]633;C{BEL}ok\r\n\
         \x1b]633;P;Cwd=/home/me/w%20d{BEL}\x1b]633;D;1\x1b\\done"
    );
    let whole = markers(&stream);
    assert!(whole.len() == 6, "the stream carries six markers: {whole:?}");

    assert_eq!(byte_at_a_time(&stream), whole, "one byte at a time");
    for at in 0..=stream.len() {
        assert_eq!(split_at(&stream, at), whole, "split at {at}");
    }
}
