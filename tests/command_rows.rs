//! A session's commands, with the lines of the terminal that are theirs.
//!
//! `tests/commands.rs` covers the bookkeeping. What only a terminal can show is that each moment is marked
//! on the row the cursor was on *when the shell said so* — whatever else arrived in the same read — and
//! that the mark stays on its row as the screen scrolls.

use apack_terminal::screen::Screen;

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
    let mut screen = Screen::new(40, 20);
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
        let mut screen = Screen::new(40, 20);
        screen.feed(stream.as_bytes());
        lines(&mut screen)
    };
    let bytes = stream.as_bytes();
    for at in 1..bytes.len() {
        let mut screen = Screen::new(40, 20);
        screen.feed(&bytes[..at]);
        screen.feed(&bytes[at..]);
        assert_eq!(lines(&mut screen), whole, "cut at {at}");
    }
}

#[test]
fn a_command_keeps_its_lines_as_the_screen_scrolls_and_loses_them_when_they_are_gone() {
    let mut screen = Screen::new(40, 5);
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
    let mut screen = Screen::new(12, 10);
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
    let mut screen = Screen::new(40, 10);
    screen.feed(format!("\x1b]133;A{BEL}$ \x1b]133;B{BEL}make\r\n\x1b]133;C{BEL}compiling\r\nlinking").as_bytes());
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("compiling\nlinking"));
}

/// The family most shells already emit says the same things another way, and names the command line.
#[test]
fn the_other_family_of_marks_records_the_same_and_the_line_it_read() {
    let mut screen = Screen::new(40, 10);
    let stream = format!(
        "\x1b]633;A{BEL}$ \x1b]633;B{BEL}ls -la\r\n\x1b]633;E;ls -la{BEL}\x1b]633;C{BEL}a.txt\r\n\x1b]633;D;0{BEL}"
    );
    screen.feed(stream.as_bytes());
    assert_eq!(lines(&mut screen), vec![(Some("ls -la".to_owned()), Some(0), Some(0), Some(1), Some(2))]);
    assert_eq!(screen.command_output(1).expect("output reads").as_deref(), Some("a.txt"));
}

#[test]
fn the_generation_says_when_to_ask_again() {
    let mut screen = Screen::new(40, 10);
    let before = screen.command_generation();
    screen.feed(b"plain output\r\n");
    assert_eq!(screen.command_generation(), before);
    screen.feed(ran("x", "", 0).as_bytes());
    assert_ne!(screen.command_generation(), before);
}
