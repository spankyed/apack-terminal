//! What this broker needs of `libghostty-vt`, asserted against the core itself.
//!
//! **It was a comparison and is not one any more.** Phase 0a of the one-VT-core goal drove this suite and
//! `screen::Screen`'s old `alacritty_terminal` implementation over the same byte streams and diffed the
//! grids; Phase B made `Screen` the core, so those cases would now compare one implementation with itself.
//! Their result is recorded in `docs/references/evidence/one-vt-core.md`, and the guarantee they bought
//! outlived them: `tests/parser.rs`'s committed snapshots were recorded from the old parser and still pass
//! unchanged, which is a cross-implementation claim no case here can make again.
//!
//! What is left is what was never a comparison: the values this broker depends on, each of which a case
//! would have to be written to notice if the core stopped answering it.
use apack_terminal::screen::{Screen, TERMINFO_NAME};

/// **The union of the two shell-integration families, which is the whole of D5's claim.**
///
/// Measured in Phase 0a: `markers.rs` reads OSC 633, which VS Code's integration emits, and the core reads
/// OSC 133, which iTerm2's, Ghostty's own and most shell scripts emit. Neither reads the other's. So the
/// scan was kept rather than deleted when the core arrived, and this is the case that says the union
/// holds — delete either half and one of these two runs stops reporting anything.
#[test]
fn a_shell_is_understood_whichever_integration_it_speaks() {
    for (family, start, finish) in [
        ("633", &b"\x1b]633;C\x07"[..], &b"\x1b]633;D;3\x07"[..]),
        ("133", &b"\x1b]133;C\x07"[..], &b"\x1b]133;D;3\x07"[..]),
    ] {
        let mut screen = Screen::new(20, 4);
        screen.feed(start);
        assert!(screen.facts().running, "OSC {family}'s command start was not read");
        screen.feed(finish);
        assert!(!screen.facts().running, "OSC {family}'s command end was not read");
        assert_eq!(screen.facts().exit_status, Some(3), "OSC {family} reported a status and it was lost");
    }
}

/// **A command is counted once, however its end was reported, so a caller asking afterwards learns one ran.**
///
/// `running` is false both before a quick command and after it, which is why the count exists. Each family
/// is driven through the ways a shell ends a command: a `D`, a `D` followed by the prompt, and a prompt with
/// no `D` at all. A prompt or a `D` with nothing running is not a command.
#[test]
fn a_finished_command_is_counted_once_in_either_family() {
    for family in ["633", "133"] {
        let mut screen = Screen::new(20, 4);
        let mut says = |code: &str| {
            screen.feed(format!("\x1b]{family};{code}\x07").as_bytes());
            screen.facts().finished
        };
        assert_eq!(says("A"), 0, "OSC {family}: the first prompt is not a command ending");
        assert_eq!(says("D"), 0, "OSC {family}: a finish with nothing running is not a command");
        assert_eq!(says("C"), 0, "OSC {family}: a command starting has not finished");
        assert_eq!(says("D;0"), 1, "OSC {family}: the command finished");
        assert_eq!(says("A"), 1, "OSC {family}: the prompt after a finish is the same command");
        assert_eq!(says("C"), 1);
        assert_eq!(says("A"), 2, "OSC {family}: a prompt ends a command whose finish was never reported");
    }
}

/// A finish with no status is absent rather than zero: a shell that says nothing about how a command
/// ended is not saying it succeeded, which is `Facts`' stated contract and easy to lose in a mapping.
#[test]
fn a_command_that_ended_without_saying_how_reports_no_status() {
    for family in ["633", "133"] {
        let mut screen = Screen::new(20, 4);
        screen.feed(format!("\x1b]{family};C\x07\x1b]{family};D\x07").replace("\\x1b", "\x1b").as_bytes());
        assert_eq!(screen.facts().exit_status, None, "OSC {family}: no status is not a zero status");
    }
}

/// The working directory, which both families and OSC 7 carry, and whose decode is this broker's.
///
/// The core stores whatever bytes the shell emitted and documents that it does not parse them, so for
/// OSC 7 that is a `file://` URI. `cwd_from_url` is shared with the `vt` path rather than written twice.
#[test]
fn a_reported_directory_is_decoded_however_it_arrives() {
    for (what, stream, wanted) in [
        ("OSC 7", &b"\x1b]7;file:///tmp/some%20where\x1b\\"[..], "/tmp/some where"),
        ("OSC 633", &b"\x1b]633;P;Cwd=/tmp/some%20where\x07"[..], "/tmp/some where"),
    ] {
        let mut screen = Screen::new(20, 4);
        screen.feed(stream);
        assert_eq!(screen.facts().cwd.as_deref(), Some(wanted), "{what} was not read or not decoded");
    }
}

/// **A terminal answers questions, and a session that answers none does not start on Windows.**
///
/// ConPTY's first act is a cursor-position request and it waits for the reply before initialising, so this
/// is the one behaviour whose absence is invisible on POSIX and fatal there. The device-attributes answer
/// is asserted by value because it *changed* when the core arrived — `alacritty_terminal` answered
/// `CSI ?6c`, a VT102, and the core answers a VT220 with ANSI colour, which is what a terminal
/// advertising `xterm-256color` should say.
#[test]
fn the_terminal_answers_what_a_program_asks() {
    let mut screen = Screen::new(20, 4);
    screen.feed(b"\x1b[6n");
    let cursor = screen.take_replies().concat();
    assert!(cursor.starts_with('\u{1b}') && cursor.ends_with('R'), "a cursor position report: {cursor:?}");

    screen.feed(b"\x1b[c");
    assert_eq!(
        screen.take_replies().concat(),
        "\u{1b}[?62;22c",
        "primary device attributes: a VT220 with ANSI colour, which is what xterm-256color claims to be",
    );
    assert!(screen.take_replies().is_empty(), "and taking them leaves none to send twice");
}

/// The advertised terminal, which a program can ask for and which decides what it emits.
///
/// XTGETTCAP's `TN` capability — `DCS + q 544E ST`, where `544E` is the hex of "TN" — is answered with the
/// configured name, hex-encoded. **The core answers nothing when no name was set**, rather than claiming
/// to be Ghostty's own entry, so a name that never reached it shows up here as silence: this case needs no
/// negative half, because a setter that did nothing would fail the assertion below.
#[test]
fn a_program_asking_what_terminal_this_is_is_told_what_the_pty_says() {
    let encoded: String = TERMINFO_NAME.bytes().map(|byte| format!("{byte:02X}")).collect();
    let mut screen = Screen::new(20, 4);
    screen.feed(b"\x1bP+q544E\x1b\\");
    let answer = screen.take_replies().concat();
    assert!(
        answer.contains(&encoded),
        "the terminfo name was not answered: wanted the hex of {TERMINFO_NAME} ({encoded}) in {answer:?}",
    );
}
