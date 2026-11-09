//! The safe layer over `libghostty-vt`'s C ABI, driven directly.
//!
//! **`screen.rs` is the only caller, and that is why this exists.** A `Screen` answers a snapshot, and a
//! snapshot hides most of what the layer below it does: a setter that silently did nothing, or a read that
//! answered a plausible constant, would leave the grid looking right. Each case here drives one call and
//! observes its effect through a different one, which is what makes a mutant of it visible.
use apack_terminal::vt::Terminal;

#[test]
fn a_terminal_has_the_size_it_was_built_with_and_the_size_it_was_resized_to() {
    let mut terminal = Terminal::new(20, 4).expect("a terminal");
    assert_eq!((terminal.cols().expect("cols"), terminal.rows().expect("rows")), (20, 4));

    terminal.resize(40, 10).expect("a resize");
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
