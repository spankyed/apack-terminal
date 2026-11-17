//! The safe layer over `libghostty-vt`'s C ABI, driven directly.
//!
//! **`screen.rs` is the only caller, and that is why this exists.** A `Screen` answers a snapshot, and a
//! snapshot hides most of what the layer below it does: a setter that silently did nothing, or a read that
//! answered a plausible constant, would leave the grid looking right. Each case here drives one call and
//! observes its effect through a different one, which is what makes a mutant of it visible.
use apack_terminal::protocol::{CursorBlink, CursorStyle};
use apack_terminal::vt::Terminal;
use apack_terminal::vt::rows::{Changed, ROW_LINK_BYTES, Rendering, Space};

#[test]
fn a_terminal_has_the_size_it_was_built_with_and_the_size_it_was_resized_to() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    assert_eq!((terminal.cols().expect("cols"), terminal.rows().expect("rows")), (20, 4));

    terminal.resize(40, 10, 0, 0).expect("a resize");
    assert_eq!(
        (terminal.cols().expect("cols"), terminal.rows().expect("rows")),
        (40, 10),
        "a resize that reported success did not change the grid",
    );
}

/// The line limit, observed where `Screen` cannot show it.
///
/// **`Screen::snapshot` caps what it answers at `HISTORY_LINES`**, so a session holding far more reads back
/// the same as one holding exactly that — which means the broker's own scrollback cases pass whether or not
/// the limit was ever set. This asks the terminal what it is holding instead.
#[test]
fn the_scrollback_limit_bounds_what_the_terminal_keeps() {
    let fed = 5_000;
    let mut bounded = Terminal::new(80, 24).expect("a terminal");
    bounded.set_scrollback_lines(300).expect("a limit");
    bounded.set_scrollback_unbounded_bytes().expect("no byte ceiling");

    let mut unbounded = Terminal::new(80, 24).expect("a terminal");
    unbounded.set_scrollback_unbounded_bytes().expect("no byte ceiling");

    for line in 0..fed {
        let bytes = format!("line {line}\r\n").into_bytes();
        bounded.feed(&bytes);
        unbounded.feed(&bytes);
    }

    let held = bounded.scrollback_rows().expect("rows held");
    assert!(held < 1_000, "a 300-line limit kept {held} rows of {fed} fed, which is no limit at all");
    assert!(
        unbounded.scrollback_rows().expect("rows held") > held,
        "a terminal told to keep no more than 300 lines held as much as one with no line limit at all",
    );
}

/// The byte ceiling, which the core applies by default and this broker lifts.
///
/// Its default is 10,000 bytes — small enough that a session holds a few hundred rows of short output and
/// fewer as lines lengthen, which is less than the broker promises a window. Lifted, the line limit is the
/// only rule left, so the same output is held in full.
#[test]
fn lifting_the_byte_ceiling_is_what_lets_a_session_hold_its_lines() {
    let fed = 3_000;
    let mut default_ceiling = Terminal::new(80, 24).expect("a terminal");
    default_ceiling.set_scrollback_lines(2_000).expect("a limit");

    let mut lifted = Terminal::new(80, 24).expect("a terminal");
    lifted.set_scrollback_lines(2_000).expect("a limit");
    lifted.set_scrollback_unbounded_bytes().expect("no byte ceiling");

    for line in 0..fed {
        let bytes = format!("line {line} of a session writing steadily\r\n").into_bytes();
        default_ceiling.feed(&bytes);
        lifted.feed(&bytes);
    }

    let (kept, lost) = (lifted.scrollback_rows().expect("rows"), default_ceiling.scrollback_rows().expect("rows"));
    assert!(
        kept > lost,
        "lifting the byte ceiling held {kept} rows and leaving it held {lost}: the call changed nothing",
    );
    assert!(kept > 1_000, "with the ceiling lifted a 2,000-line limit should hold well over a thousand: {kept}");
}

/// **A call the core refuses is reported, and this is the case that proves the result is read at all.**
///
/// The core caps a terminfo name at 128 bytes and refuses a longer one. That matters on its own — a broker
/// that accepted one silently would leave a program's XTGETTCAP query unanswered with nothing said about
/// why — and it is the one failure in this layer that reaches only the result check. `Terminal::new` cannot
/// show that: it guards twice, on the result *and* on a null handle, so dropping the first leaves the
/// second reporting the same error, and a case built on it passes either way.
#[test]
fn a_call_the_core_refuses_is_reported_rather_than_passed_over() {
    let terminal = Terminal::new(20, 4).expect("a terminal");
    let too_long = "x".repeat(200);
    let Err(failed) = terminal.set_terminfo_name(&too_long) else {
        panic!("a terminfo name of 200 bytes is past the core's 128-byte cap and must be refused")
    };
    assert_eq!(failed.call, "ghostty_terminal_set(TERMINFO_NAME)", "the refusal names its call: {failed}");

    // And a name within the cap is accepted, so the case above is about the length rather than the call
    terminal.set_terminfo_name("xterm-256color").expect("a name within the cap");
}

/// A refusal names the call it came from, and says nothing about an unsupported build unless it was one.
///
/// Zero columns is refused by `ghostty_terminal_new` — the one failure this layer can provoke without a
/// build that lacks a feature. The second half is the point: `UNSUPPORTED` means a build does not have
/// something, and reporting that for an ordinary bad argument would send a reader looking at the wrong
/// thing entirely.
#[test]
fn a_refused_call_names_itself_and_does_not_blame_the_build() {
    let Err(failed) = Terminal::new(0, 0) else { panic!("zero columns must be refused") };
    assert_eq!(failed.call, "ghostty_terminal_new", "a refusal names the call: {failed}");
    let said = failed.to_string();
    assert!(said.contains("ghostty_terminal_new"), "and says so when printed: {said}");
    assert!(!said.contains("does not provide it"), "a bad argument was reported as a missing feature: {said}",);
}

/// Feeds each chunk an OSC's end at a time, as a caller that looks at the terminal wherever one ended
/// does, and answers each place in the whole stream a call returned at before its chunk ran out.
fn osc_stops(terminal: &mut Terminal, chunks: &[&[u8]]) -> Vec<usize> {
    let (mut stops, mut before) = (Vec::new(), 0);
    for chunk in chunks {
        let mut fed = 0;
        while fed < chunk.len() {
            let (taken, _) = terminal.feed_to(&chunk[fed..], false);
            assert!(taken > 0, "a call given bytes took none of them, so its caller would never finish");
            fed += taken;
            if fed < chunk.len() {
                stops.push(before + fed);
            }
        }
        before += chunk.len();
    }
    stops
}

/// **A parse stops on the byte that ends an OSC string, and on no other, wherever a read was cut.**
///
/// A shell's marker is an OSC string, and where the parse stops is where the cursor is read for it. The
/// stream holds a string ended by BEL, one ended by ST — which has ended at ST's first byte, the ESC — and
/// then what must not stop a parse: a BEL that ends nothing, and an ESC that begins a CSI sequence. Cut
/// anywhere, inside a string or between an ESC and what follows it, the stops are at the same bytes; one
/// that falls on a read's last byte is where that read ended anyway.
#[test]
fn a_parse_stops_where_an_osc_string_ends_however_the_stream_was_cut() {
    let titled = b"ab\x1b]0;one\x07";
    let named = b"cd\x1b]2;two\x1b";
    let rest = b"\\ef\x07\x1b[1mgh";
    let stream = [&titled[..], &named[..], &rest[..]].concat();
    let ends = [titled.len(), titled.len() + named.len()];

    let mut whole = Terminal::new(20, 4).expect("a terminal");
    assert_eq!(osc_stops(&mut whole, &[&stream]), ends);
    assert_eq!(whole.cursor_column().expect("a column"), 8, "every byte is parsed, and once");

    for cut in 1..stream.len() {
        let mut terminal = Terminal::new(20, 4).expect("a terminal");
        let stops = osc_stops(&mut terminal, &[&stream[..cut], &stream[cut..]]);
        let within: Vec<usize> = ends.iter().copied().filter(|end| *end != cut).collect();
        assert_eq!(stops, within, "cut at {cut}");
        assert_eq!(terminal.cursor_column().expect("a column"), 8, "cut at {cut}");
    }
}

/// **Ground is a state, so a parser already there has reached it before taking a byte.** The bytes are
/// still the caller's to feed: said to be taken, they would never be parsed, and a session would lose
/// the first character of whatever came next.
#[test]
fn a_parser_between_sequences_is_at_ground_having_taken_nothing() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    assert_eq!(terminal.feed_to(b"abc", true), (0, true));
    assert_eq!(terminal.cursor_column().expect("a column"), 0, "nothing was taken, and nothing was parsed");

    // Not asked about ground, the same bytes are all taken and nothing is said of it
    assert_eq!(terminal.feed_to(b"abc", false), (3, false));
    assert_eq!(terminal.cursor_column().expect("a column"), 3);
}

/// Given no bytes at all there is still one thing to answer, and it is answered only to a caller that
/// asked and only by a parser that is there.
#[test]
fn a_parse_of_nothing_says_ground_only_when_asked_and_only_when_there() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    assert_eq!(terminal.feed_to(&[], false), (0, false), "between sequences, and not asked");
    assert_eq!(terminal.feed_to(&[], true), (0, true), "between sequences, and asked");

    terminal.feed(b"\x1b]0;ti");
    assert_eq!(terminal.feed_to(&[], true), (0, false), "part way through a string, and asked");
    assert_eq!(terminal.feed_to(&[], false), (0, false), "part way through a string, and not asked");
}

/// **Asked for ground as well, a parse stops at whichever comes first.** The byte that ends a CSI
/// sequence reaches ground. The ESC that ends an OSC string does not — the parser is then part way
/// through the ST — so that stop is the string's end and not ground, and the next byte is ground.
#[test]
fn a_parse_asked_for_ground_stops_on_the_byte_that_reaches_it_and_still_where_a_string_ends() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    terminal.feed(b"\x1b[3");
    assert_eq!(terminal.feed_to(b"1mabc", true), (2, true), "the `m` ends the sequence");
    assert_eq!(terminal.cursor_column().expect("a column"), 0, "what follows it is not parsed yet");

    terminal.feed(b"\x1b]0;t");
    assert_eq!(terminal.feed_to(b"\x1b\\abc", true), (1, false), "the string ended, part way through its ST");
    assert_eq!(terminal.feed_to(b"\\abc", true), (1, true), "and the ST's last byte is ground");
}

/// **A text is measured in bytes against what its caller can carry**: one exactly as long as the limit
/// is handed over, and one a byte longer is refused with how long it is.
#[test]
fn a_text_exactly_as_long_as_the_limit_is_given_and_a_byte_longer_is_refused_with_its_length() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    terminal.feed("na\u{ef}ve text".as_bytes());
    let (start, end) = ((0, 0), (4, 0));
    assert_eq!(terminal.text_within(start, end, 6), Ok(Ok("na\u{ef}ve".to_owned())), "five characters in six bytes");
    assert_eq!(terminal.text_within(start, end, 5), Ok(Err(6)));
}

/// **A row's links may come to exactly `ROW_LINK_BYTES`.** Addresses that fill the budget to its last
/// byte are all kept; the next new one, however short, is left off and its cell drawn unlinked; and an
/// address the row already holds is not a new one, so a cell after that is still linked to it.
#[test]
fn a_rows_links_fill_their_budget_to_the_last_byte_and_the_next_address_is_left_off() {
    const LONG: usize = 2_000;
    let mut lengths = vec![LONG; ROW_LINK_BYTES / LONG];
    lengths.push(ROW_LINK_BYTES % LONG);
    // Each cell's own address: its number, padded to the length wanted
    let addresses: Vec<String> = lengths.iter().enumerate().map(|(cell, length)| format!("{cell:0>length$}")).collect();
    assert_eq!(addresses.iter().map(String::len).sum::<usize>(), ROW_LINK_BYTES);

    let mut written = String::new();
    for address in addresses.iter().map(String::as_str).chain(["!", addresses[0].as_str()]) {
        written.push_str(&format!("\x1b]8;;{address}\x1b\\x"));
    }
    let mut terminal = Terminal::new(600, 2).expect("a terminal");
    terminal.feed(written.as_bytes());

    let row = terminal.read_row(Space::Active, 0).expect("a row reads").expect("the first row");
    let links: Vec<Option<&str>> = row.cells().map(|cell| cell.link).collect();
    assert_eq!(links.len(), addresses.len() + 2, "a cell past the budget is still drawn");
    for (cell, address) in addresses.iter().enumerate() {
        assert_eq!(links[cell], Some(address.as_str()), "cell {cell} of {}", addresses.len());
    }
    assert_eq!(links[addresses.len()], None, "one byte more than a row may carry");
    assert_eq!(links[addresses.len() + 1], Some(addresses[0].as_str()), "an address the row already holds");
}

/// **A look says what changed since the last one that was seen.** A terminal never looked at is all
/// new. From then on a look names rows: the one written to and no other, again for as long as nobody
/// has said it was read, and none once somebody has.
#[test]
fn a_look_names_the_rows_written_since_the_last_one_that_was_seen() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    let mut rendering = Rendering::new().expect("a render state");
    let mut changed = Vec::new();
    assert_eq!(rendering.look(&terminal, &mut changed), Ok(Changed::Everything));
    rendering.seen().expect("seen");

    assert_eq!(rendering.look(&terminal, &mut changed), Ok(Changed::Rows), "nothing has been written since");
    assert_eq!(changed, [false; 4]);

    terminal.feed(b"x");
    for look in ["the first look after the write", "a second, the first not having been seen"] {
        assert_eq!(rendering.look(&terminal, &mut changed), Ok(Changed::Rows), "{look}");
        assert_eq!(changed, [true, false, false, false], "{look}");
    }
    rendering.seen().expect("seen");
    assert_eq!(rendering.look(&terminal, &mut changed), Ok(Changed::Rows));
    assert_eq!(changed, [false; 4], "what was seen is not said again");
}

/// A terminal whose person has said whether the cursor blinks, with the cursor taken there.
fn with_a_blink_chosen(blink: CursorBlink) -> Terminal {
    let mut terminal = Terminal::new(40, 4).expect("a terminal");
    terminal.set_cursor_defaults(CursorStyle::Block, blink).expect("the defaults are set");
    terminal.reset_cursor();
    terminal
}

/// Whether the cursor blinks, as the mode a program sets and a cursor it asks for both leave it: DEC mode 12.
fn blinks(terminal: &Terminal) -> bool {
    terminal.mode(12).expect("the mode reads")
}

fn first_row(terminal: &Terminal) -> String {
    terminal.text_between((0, 0), (39, 0)).expect("the row reads")
}

/// **Output parsed whole is all of it parsed, around a sequence that is watched.** A session's output is
/// usually parsed up to a boundary and the rest asked for again, which hides a parse that stopped early:
/// the rest is asked for anyway. Parsed with nothing to stop at — as an image's body is, and a recording's
/// redraw — what is before a watched sequence, what is after it, and a sequence naming other modes ahead
/// of it in the same bytes are each parsed, and mode 12 still leaves the blink as it was.
#[test]
fn output_parsed_whole_is_all_parsed_around_a_sequence_that_is_watched() {
    for (blink, blinking, against) in [(CursorBlink::On, true, 'l'), (CursorBlink::Off, false, 'h')] {
        let mut terminal = with_a_blink_chosen(blink);
        terminal.feed(format!("before\x1b[?12{against}after").as_bytes());
        assert_eq!(first_row(&terminal), "beforeafter", "{blink:?}");
        assert_eq!(blinks(&terminal), blinking, "{blink:?}: mode 12 changed a chosen blink");

        terminal.feed(format!(" \x1b[?25l\x1b[?25hand\x1b[?12{against} more").as_bytes());
        assert_eq!(first_row(&terminal), "beforeafter and more", "{blink:?}");
        assert_eq!(blinks(&terminal), blinking, "{blink:?}: mode 12 after a sequence naming other modes");
    }
}

/// **A chosen blink has the sequences that may change mode 12 watched, and no others.** Watching is what
/// lets mode 12 be undone, and costs a sequence several calls into the core; a program writes thousands of
/// sequences that have nothing to do with it. So what is counted here is the work: none for text, colours
/// and the private modes a full-screen program sets all day, one for each sequence that names mode 12 or
/// that cannot be told from one that does, and none at all while nobody has chosen a blink.
#[test]
fn a_chosen_blink_watches_the_sequences_that_may_change_mode_12_and_no_others() {
    let unwatched = [
        "plain text\r\n",
        "\x1b[31mred\x1b[0m \x1b[1;38;5;208mbold\x1b[0m",
        "\x1b[H\x1b[2K\x1b[5;10H",
        "\x1b]0;a title\x07",
        "\x1b[?25l\x1b[?25h",
        "\x1b[?1049h\x1b[?1049l",
        "\x1b[?2026h\x1b[?2026l",
        "\x1b[?25;1000h\x1b[?1000;25l",
        "\x1b[?120h\x1b[?112l\x1b[?1;2h",
        "\x1b[?h\x1b[?;25l",
    ];
    let watched = [
        "\x1b[?12h",
        "\x1b[?12l",
        "\x1b[?25;12h",
        "\x1b[?12;25l",
        "\x1b[?25;12;1000h",
        "\x1b[?012h",
        "\x1b[?12s",
        "\x1b[?12r",
        // Not all numbers and separators, so not judged: the core says what it is
        "\x1b[?1:2h",
        "\x1b[?25$p",
    ];

    let mut chosen = with_a_blink_chosen(CursorBlink::On);
    for written in unwatched {
        chosen.feed(written.as_bytes());
        assert_eq!(chosen.sequences_watched(), 0, "watched {written:?}, which names no mode 12");
    }
    for (nth, written) in watched.iter().enumerate() {
        chosen.feed(written.as_bytes());
        assert_eq!(chosen.sequences_watched(), nth as u64 + 1, "after {written:?}");
    }

    let mut left_alone = Terminal::new(40, 4).expect("a terminal");
    for written in unwatched.iter().chain(&watched) {
        left_alone.feed(written.as_bytes());
    }
    assert_eq!(left_alone.sequences_watched(), 0, "a blink nobody chose has nothing to watch for");
}

/// **A sequence cut anywhere by the end of a read is the same sequence.** What begins one may be the last
/// byte read, or the last two, and is then known only by what comes next: it is watched until it is, once,
/// and mode 12 leaves the blink as it was wherever the cut fell.
#[test]
fn a_sequence_naming_mode_12_is_watched_once_wherever_a_read_ends_inside_it() {
    let sequence = "\x1b[?12h";
    for cut in 1..sequence.len() {
        let mut terminal = with_a_blink_chosen(CursorBlink::Off);
        terminal.feed(b"a");
        terminal.feed(&sequence.as_bytes()[..cut]);
        terminal.feed(&sequence.as_bytes()[cut..]);
        terminal.feed(b"b");
        assert!(!blinks(&terminal), "cut after {cut} bytes: mode 12 changed a chosen blink");
        assert_eq!(terminal.sequences_watched(), 1, "cut after {cut} bytes");
        assert_eq!(first_row(&terminal), "ab", "cut after {cut} bytes");
    }
}

/// **A sequence abandoned for another is parsed once.** An escape inside a sequence ends it and begins the
/// next, which is watched in its place. The byte before that escape is one the core would act on if it
/// were handed it again after the escape — `ESC 8` puts the cursor back where it was saved — so text
/// written after lands where it should only if every byte was parsed once.
#[test]
fn a_sequence_abandoned_for_another_leaves_the_rest_parsed_once() {
    let mut terminal = with_a_blink_chosen(CursorBlink::Off);
    terminal.feed(b"ab\x1b[?8\x1b[5 qcd");
    assert_eq!(first_row(&terminal), "abcd");
    assert!(blinks(&terminal), "the cursor asked for by what cut the sequence short was not given");
}

/// Where `feed_to` stops over `reads`, each fed until it is all taken: the read, how far into it, and
/// whether ground was where.
fn stops(blink: CursorBlink, reads: &[&[u8]], or_ground: bool) -> Vec<(usize, usize, bool)> {
    let mut terminal = Terminal::new(80, 24).expect("a terminal");
    terminal.set_cursor_defaults(CursorStyle::Bar, blink).expect("the defaults are set");
    let mut stops = Vec::new();
    for (read, bytes) in reads.iter().enumerate() {
        let mut at = 0;
        while at < bytes.len() {
            let (taken, ground) = terminal.feed_to(&bytes[at..], or_ground);
            at += taken;
            stops.push((read, at, ground));
            if taken == 0 {
                // Stopped between sequences having taken nothing: the rest is fed without that stop
                at += terminal.feed_to(&bytes[at..], false).0;
                stops.push((read, at, false));
            }
        }
    }
    stops
}

#[test]
fn a_chosen_blink_stops_where_a_string_ends_and_between_sequences_as_any_session_does() {
    let cases: &[(&[&[u8]], bool)] = &[
        // A read that ends on the escape of the string's end, and on the escape that begins the next string
        (&[b"\x1b]0;one\x1b", b"\\after"], false),
        (&[b"\x1b]0;one\x1b\\\x1b", b"]0;two\x1b\\after"], false),
        (&[b"text\x1b", b"]0;two\x1b\\after\x1b]0;three\x07z"], false),
        // A string cut short by a sequence that is watched
        (&[b"\x1b]0;one\x1b[?12hafter"], false),
        // Between sequences, with a watched sequence next: stopped having taken nothing
        (&[b"\x1b[?12hafter"], true),
        (&[b"ab\x1b[?25l\x1b[?12l"], true),
    ];
    for (reads, or_ground) in cases {
        let left_to_programs = stops(CursorBlink::Program, reads, *or_ground);
        for chosen in [CursorBlink::On, CursorBlink::Off] {
            assert_eq!(stops(chosen, reads, *or_ground), left_to_programs, "{chosen:?} over {reads:?}");
        }
    }
}
