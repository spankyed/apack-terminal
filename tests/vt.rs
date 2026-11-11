//! The safe layer over `libghostty-vt`'s C ABI, driven directly.
//!
//! **`screen.rs` is the only caller, and that is why this exists.** A `Screen` answers a snapshot, and a
//! snapshot hides most of what the layer below it does: a setter that silently did nothing, or a read that
//! answered a plausible constant, would leave the grid looking right. Each case here drives one call and
//! observes its effect through a different one, which is what makes a mutant of it visible.
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
