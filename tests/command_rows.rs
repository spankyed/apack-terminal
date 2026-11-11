//! A session's commands, with the lines of the terminal that are theirs.
//!
//! `tests/commands.rs` covers the bookkeeping. What only a terminal can show is that each moment is marked
//! on the row the cursor was on *when the shell said so* — whatever else arrived in the same read — and
//! that the mark stays on its row as the screen scrolls.

use std::sync::mpsc::{Receiver, channel};

use apack_terminal::screen::Screen;
use apack_terminal::view::publish::ViewId;

/// A screen with a view attached, which is what gives its lines their numbers. The receiver is the
/// view's end, kept so the view stays.
fn viewed(cols: u16, rows: u16) -> (Screen, Receiver<Vec<u8>>) {
    let mut screen = Screen::new(cols, rows);
    let (sink, incoming) = channel();
    screen.attach_view(ViewId { connection: 1, channel: 1 }, sink).expect("a view attaches");
    (screen, incoming)
}

const BEL: &str = "\x07";

/// A prompt, a command line and its output, as a shell with the standard marks writes them.
fn ran(command: &str, output: &str, status: i32) -> String {
    format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}{command}\r\n\x1b]133;C{BEL}{output}\x1b]133;D;{status}{BEL}")
}

/// A command as `(command, exit status, prompt line, output line, end line)`.
type Lines = (Option<String>, Option<i32>, Option<u64>, Option<u64>, Option<u64>);

fn lines(screen: &mut Screen) -> Vec<Lines> {
    let (_, commands) = screen.commands().expect("the commands read");
    commands.into_iter().map(|one| (one.command, one.exit_status, one.prompt, one.output, one.end)).collect()
}

/// **The whole session in one read.** Three commands and their prompts arrive as a single chunk, so the
/// cursor at the end of the read is nowhere near where any of them began — and each is still marked where
/// it was.
#[test]
fn each_moment_is_marked_where_the_cursor_was_when_the_shell_said_so() {
    let (mut screen, _view) = viewed(40, 20);
    let stream = [ran("one", "a\r\nb\r\n", 0), ran("two", "", 3), ran("three", "c\r\n", 0)].concat();
    screen.feed(stream.as_bytes());

    assert_eq!(
        lines(&mut screen),
        vec![
            (None, Some(0), Some(0), Some(1), Some(3)),
            (None, Some(3), Some(3), Some(4), Some(4)),
            (None, Some(0), Some(4), Some(5), Some(6)),
        ]
    );
}

/// And the same marks however the stream is cut, since where a read ends is not where anything happened.
#[test]
fn the_marks_are_the_same_however_the_stream_is_cut() {
    let stream = [ran("one", "a\r\nb\r\n", 0), ran("two", "c\r\n", 1)].concat();
    let whole = {
        let (mut screen, _view) = viewed(40, 20);
        screen.feed(stream.as_bytes());
        lines(&mut screen)
    };
    let bytes = stream.as_bytes();
    for at in 1..bytes.len() {
        let (mut screen, _view) = viewed(40, 20);
        screen.feed(&bytes[..at]);
        screen.feed(&bytes[at..]);
        assert_eq!(lines(&mut screen), whole, "cut at {at}");
    }
}

#[test]
fn a_command_keeps_its_lines_as_the_screen_scrolls_and_loses_them_when_they_are_gone() {
    let (mut screen, _view) = viewed(40, 5);
    screen.feed(ran("first", "out\r\n", 0).as_bytes());
    let before = lines(&mut screen);
    assert_eq!(before[0].2, Some(0));

    let more: String = (0..60).map(|line| format!("line {line}\r\n")).collect();
    screen.feed(more.as_bytes());
    assert_eq!(lines(&mut screen), before, "scrolling into history moved a command's lines");

    // Erasing the history takes the rows, and the marks with them
    screen.feed(b"\x1b[3J");
    let after = lines(&mut screen);
    assert_eq!((after[0].2, after[0].3, after[0].4), (None, None, None));
    assert_eq!(after[0].1, Some(0), "what is known of the command is kept");
}

#[test]
fn a_commands_output_is_read_as_text_from_where_it_began_to_where_it_ended() {
    let (mut screen, _view) = viewed(12, 10);
    let stream = [
        ran("seq", "1\r\n2\r\n3\r\n", 0),
        // A line longer than the screen is wide is one line of output
        ran("long", "abcdefghijklmnopqrst\r\n", 0),
        ran("quiet", "", 0),
        // A last line with no newline is output all the same
        ran("printf", "no newline", 0),
    ]
    .concat();
    screen.feed(stream.as_bytes());

    let output = |id: u64| screen.command_output(id).expect("output reads");
    assert_eq!(output(1).as_deref(), Some("1\n2\n3"));
    assert_eq!(output(2).as_deref(), Some("abcdefghijklmnopqrst"));
    assert_eq!(output(3).as_deref(), Some(""));
    assert_eq!(output(4).as_deref(), Some("no newline"));
    assert_eq!(output(99), None, "a command that is not kept");
}

#[test]
fn the_output_of_a_command_still_running_is_what_it_has_printed_so_far() {
    let (mut screen, _view) = viewed(40, 10);
    screen.feed(format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}make\r\n\x1b]133;C{BEL}compiling\r\nlinking").as_bytes());
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("compiling\nlinking"));
}

/// The family most shells already emit says the same things another way, and names the command line.
#[test]
fn the_other_family_of_marks_records_the_same_and_the_line_it_read() {
    let (mut screen, _view) = viewed(40, 10);
    let stream = format!(
        "\x1b]633;A{BEL}$ \x1b]633;B{BEL}ls -la\r\n\x1b]633;E;ls -la{BEL}\x1b]633;C{BEL}a.txt\r\n\x1b]633;D;0{BEL}"
    );
    screen.feed(stream.as_bytes());
    assert_eq!(lines(&mut screen), vec![(Some("ls -la".to_owned()), Some(0), Some(0), Some(1), Some(2))]);
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("a.txt"));
}

#[test]
fn the_generation_says_when_to_ask_again() {
    let (mut screen, _view) = viewed(40, 10);
    let before = screen.command_generation();
    screen.feed(b"plain output\r\n");
    assert_eq!(screen.command_generation(), before);
    screen.feed(ran("x", "", 0).as_bytes());
    assert_ne!(screen.command_generation(), before);
}

/// A command whose last line had no newline ended part way along a row, and the shell's next prompt is
/// drawn on from there: the output stops at the column the command ended on.
#[test]
fn output_that_ended_part_way_along_a_row_stops_where_the_command_did() {
    let (mut screen, _view) = viewed(40, 10);
    screen.feed(
        format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}printf abc\r\n\x1b]133;C{BEL}abc\x1b]133;D;0{BEL}%  next").as_bytes(),
    );
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("abc"));
}

/// A program still running can move the cursor above where its output began — a redraw from the top of
/// the screen does. There is nothing between the two to read then, rather than the rows above read
/// backwards.
#[test]
fn a_running_command_whose_cursor_is_above_where_it_began_has_printed_nothing_yet() {
    let (mut screen, _view) = viewed(40, 10);
    screen.feed(format!("earlier line\r\n\x1b]133;A{BEL}$ \x1b]133;B{BEL}top\r\n\x1b]133;C{BEL}\x1b[H").as_bytes());
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some(""));
}

/// **A mark means something on the screen it was made on.** The shell's screen and an application's count
/// their rows from their own tops, so while `vim` is showing, the marks of what ran before it name rows of
/// `vim`: a command's lines are not told, and its output is not read out of the editor. Back on the
/// shell's screen they are what they were.
#[test]
fn a_commands_marks_say_nothing_while_another_screen_is_showing() {
    let (mut screen, _view) = viewed(40, 10);
    screen.feed(ran("ls", "a.txt\r\nb.txt\r\n", 0).as_bytes());
    screen.feed(format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}vim\r\n\x1b]133;C{BEL}").as_bytes());
    let on_the_shell = lines(&mut screen);
    assert_eq!(on_the_shell[0], (None, Some(0), Some(0), Some(1), Some(3)));
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("a.txt\nb.txt"));

    screen.feed(b"\x1b[?1049h\x1b[2J\x1b[Hthe editor's first line\r\nits second\r\nits third\r\nits fourth");
    assert_eq!(screen.command_output(1).expect("output reads"), None, "read out of the editor");
    assert_eq!(screen.command_output(2).expect("output reads"), None, "the editor's own command, begun on the shell");
    for (_, _, prompt, output, end) in lines(&mut screen) {
        assert_eq!((prompt, output, end), (None, None, None), "a line of the shell's, told as a line of the editor's");
    }

    // A command marked while the editor is up is on the editor's screen, and is read there
    screen.feed(format!("\x1b[H\x1b]133;C{BEL}").as_bytes());
    assert_eq!(screen.command_output(3).expect("output reads").as_deref(), Some("the editor's first line"));

    screen.feed(b"\x1b[?1049l");
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("a.txt\nb.txt"));
    assert_eq!(screen.command_output(3).expect("output reads"), None, "the editor's, read out of the shell");
    assert_eq!(lines(&mut screen)[0].1, Some(0));
    assert!(lines(&mut screen)[0].2.is_some());
}

/// **Where a command ended goes with its text when the screen is reflowed.** A command whose last line
/// stopped a hundred columns along a grid of a hundred and twenty ends, once the grid is eighty wide,
/// twenty columns along the next row: its end is a mark on the last cell it wrote, which the core moves
/// with the line, and not a column number that only meant something at the old width.
#[test]
fn a_commands_output_is_still_all_of_it_after_a_resize_wraps_it_elsewhere() {
    let (mut screen, _view) = viewed(120, 10);
    let printed = "x".repeat(100);
    screen.feed(
        format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}long\r\n\x1b]133;C{BEL}first\r\n{printed}\x1b]133;D;0{BEL}\r\n$ ")
            .as_bytes(),
    );
    let expected = format!("first\n{printed}");
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some(expected.as_str()));

    for (cols, rows) in [(80, 10), (30, 20), (7, 30), (200, 5), (120, 10)] {
        screen.resize(cols, rows, 0, 0);
        assert_eq!(
            screen.command_output(1).expect("output reads").as_deref(),
            Some(expected.as_str()),
            "at {cols} columns"
        );
    }
}

/// **Only the main prompt begins a command.** A shell also marks the prompt it draws for each further
/// line of a command still being typed, and the one at the right edge. Neither ends what is running, and
/// neither moves where the command's prompt is.
#[test]
fn a_continuation_prompt_is_not_a_new_command() {
    let (mut screen, _view) = viewed(40, 10);
    // A prompt, a first line ending in a backslash, and the shell's prompt for the rest of it
    screen.feed(
        format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}echo \\\r\n\x1b]133;A;k=c{BEL}> \x1b]133;B{BEL}hi\r\n").as_bytes(),
    );
    assert_eq!(
        lines(&mut screen),
        vec![(None, None, Some(0), None, None)],
        "one command, whose prompt is on its first line"
    );

    // Running, and a right-hand or a secondary prompt drawn meanwhile ends nothing
    screen.feed(format!("\x1b]133;C{BEL}hi\r\n").as_bytes());
    assert!(screen.facts().running);
    for kind in ["r", "c", "s"] {
        screen.feed(format!("\x1b]133;A;k={kind}{BEL}").as_bytes());
        assert!(screen.facts().running, "a prompt of kind {kind} ended the command");
    }
    assert_eq!(lines(&mut screen).len(), 1);

    // The main prompt does: with the kind said, and with none
    screen.feed(format!("\x1b]133;A;k=i{BEL}$ ").as_bytes());
    assert!(!screen.facts().running);
    assert_eq!(lines(&mut screen).len(), 2);
}

/// **A command has the line the shell named for it, and no other command's.** A shell that names the line
/// before it starts a command (`E`, then `C`) and one that names it just after (`C`, then `E`) are both
/// read; one that starts a command without naming it has run a command with no known line — not the line
/// of the command before.
#[test]
fn a_command_is_given_its_own_line_and_never_the_last_commands() {
    let named = |screen: &mut Screen| lines(screen).into_iter().map(|one| one.0).collect::<Vec<_>>();
    let (mut screen, _view) = viewed(40, 10);
    let prompt = format!("\x1b]633;A{BEL}$ \x1b]633;B{BEL}");
    screen.feed(format!("{prompt}\x1b]633;E;first{BEL}\x1b]633;C{BEL}out\r\n\x1b]633;D;0{BEL}").as_bytes());
    assert_eq!(screen.facts().command.as_deref(), Some("first"));

    // Started with no line named
    screen.feed(format!("{prompt}\x1b]633;C{BEL}").as_bytes());
    assert_eq!(screen.facts().command, None, "it was given the line of the command before");
    assert_eq!(named(&mut screen), vec![Some("first".to_owned()), None]);

    // And named once it had started
    screen.feed(format!("\x1b]633;E;second{BEL}").as_bytes());
    assert_eq!(screen.facts().command.as_deref(), Some("second"));
    assert_eq!(named(&mut screen), vec![Some("first".to_owned()), Some("second".to_owned())]);
    screen.feed(format!("out\r\n\x1b]633;D;0{BEL}").as_bytes());

    // A line named at a prompt that was then abandoned is not the next command's
    screen.feed(format!("{prompt}\x1b]633;E;abandoned{BEL}{prompt}\x1b]633;C{BEL}").as_bytes());
    assert_eq!(named(&mut screen).last(), Some(&None));
}

/// A command whose last line filled the row to its last column, with no newline after it, ended with the
/// cursor still on that last cell: the row is full, and the cursor moves on only when more is printed.
/// The last character is the command's output like any other.
#[test]
fn output_that_ended_on_the_last_cell_of_a_row_keeps_its_last_character() {
    let (mut screen, _view) = viewed(10, 6);
    let stream = format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}fill\r\n\x1b]133;C{BEL}0123456789\x1b]133;D;0{BEL}\r\n$ ");
    screen.feed(stream.as_bytes());
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("0123456789"));
    // One short of the edge ends where it always did
    let (mut screen, _view) = viewed(10, 6);
    screen.feed(stream.replace("0123456789", "012345678").as_bytes());
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("012345678"));
}
