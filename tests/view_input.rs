//! What a view's input becomes, which depends on modes only the terminal knows.
//!
//! Each case sets a mode the way a program does — by writing the sequence — and reads what the same key,
//! click or paste sends before and after. That the answer changes is the reason a view sends events
//! rather than bytes.

use apack_terminal::screen::Screen;
use apack_terminal::vt::input::{Key, Pasted, Pointer};

const SHIFT: u16 = 1;
const CONTROL: u16 = 2;
const ALT: u16 = 4;

fn key<'a>(code: &'a str, modifiers: u16, text: &'a str, unshifted: char) -> Key<'a> {
    Key { action: 0, code, modifiers, text, unshifted: u32::from(unshifted) }
}

fn sent(screen: &mut Screen, key: &Key<'_>) -> String {
    String::from_utf8(screen.key(key).expect("a key encodes")).expect("a key's bytes are text")
}

fn click(action: u8, button: u8, column: f32, row: f32) -> Pointer {
    // Cells ten pixels by twenty, and the pointer in the middle of one
    Pointer {
        action,
        button,
        modifiers: 0,
        x: column * 10.0 + 5.0,
        y: row * 20.0 + 10.0,
        cell_width: 10.0,
        cell_height: 20.0,
    }
}

fn pointed(screen: &mut Screen, pointer: &Pointer) -> String {
    String::from_utf8(screen.pointer(pointer).expect("a pointer event encodes")).expect("a report is text")
}

#[test]
fn a_key_that_types_sends_what_it_types() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("KeyA", 0, "a", 'a')), "a");
    assert_eq!(sent(&mut screen, &key("KeyA", SHIFT, "A", 'a')), "A");
    assert_eq!(sent(&mut screen, &key("Digit1", SHIFT, "!", '1')), "!");
    assert_eq!(sent(&mut screen, &key("KeyE", 0, "\u{e9}", 'e')), "\u{e9}");
    assert_eq!(sent(&mut screen, &key("Enter", 0, "", '\0')), "\r");
    assert_eq!(sent(&mut screen, &key("Tab", 0, "", '\0')), "\t");
    assert_eq!(sent(&mut screen, &key("Backspace", 0, "", '\0')), "\x7f");
    assert_eq!(sent(&mut screen, &key("Escape", 0, "", '\0')), "\x1b");
}

#[test]
fn control_and_alt_change_what_a_letter_sends() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("KeyC", CONTROL, "", 'c')), "\x03");
    assert_eq!(sent(&mut screen, &key("KeyD", CONTROL, "", 'd')), "\x04");
}

/// Alt before a letter is the escape prefix — where the key *is* alt. On a Mac it is the option key, which
/// types a character of its own unless the person chose otherwise, and that choice is the session's.
#[test]
fn alt_is_the_escape_prefix_once_the_option_key_is_chosen_as_alt() {
    let mut screen = Screen::new(80, 24);
    screen.set_option_as_alt(true).expect("the choice is taken");
    assert_eq!(sent(&mut screen, &key("KeyB", ALT, "b", 'b')), "\x1bb");
    // Composing instead, the character the option key typed is what is sent
    screen.set_option_as_alt(false).expect("the choice is taken");
    assert_eq!(sent(&mut screen, &key("KeyB", ALT, "\u{222b}", 'b')), "\u{222b}");
}

/// **The reason keys are encoded here.** The same arrow is two different sequences depending on a mode a
/// full-screen program sets, and a window that got it wrong moved nothing in `vim`.
#[test]
fn an_arrow_follows_the_cursor_key_mode_the_program_set() {
    let mut screen = Screen::new(80, 24);
    let up = key("ArrowUp", 0, "", '\0');
    assert_eq!(sent(&mut screen, &up), "\x1b[A");
    screen.feed(b"\x1b[?1h");
    assert_eq!(sent(&mut screen, &up), "\x1bOA");
    screen.feed(b"\x1b[?1l");
    assert_eq!(sent(&mut screen, &up), "\x1b[A");
    assert_eq!(sent(&mut screen, &key("ArrowUp", SHIFT, "", '\0')), "\x1b[1;2A");
    assert_eq!(sent(&mut screen, &key("F5", 0, "", '\0')), "\x1b[15~");
    assert_eq!(sent(&mut screen, &key("PageDown", 0, "", '\0')), "\x1b[6~");
}

/// A release is reported only to a program that asked for one, in the protocol that has a way to say it.
#[test]
fn a_release_is_sent_only_under_the_keyboard_protocol_that_reports_them() {
    let mut screen = Screen::new(80, 24);
    let release = Key { action: 2, ..key("KeyA", 0, "a", 'a') };
    assert_eq!(sent(&mut screen, &release), "");

    // Disambiguate, and report event types
    screen.feed(b"\x1b[>3u");
    assert_eq!(sent(&mut screen, &release), "\x1b[97;1:3u");
    assert_eq!(sent(&mut screen, &key("Escape", 0, "", '\0')), "\x1b[27u", "and escape is no longer ambiguous");
}

#[test]
fn a_key_the_core_has_no_name_for_still_types_its_text() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("SomeFutureKey", 0, "x", 'x')), "x");
    assert_eq!(sent(&mut screen, &key("SomeFutureKey", 0, "", '\0')), "");
}

#[test]
fn the_pointer_is_reported_only_to_a_program_that_asked_and_in_the_format_it_chose() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(pointed(&mut screen, &click(0, 0, 2.0, 1.0)), "", "nobody asked");
    assert_eq!(pointed(&mut screen, &click(1, 0, 2.0, 1.0)), "");

    // Clicks, in the SGR format
    screen.feed(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(pointed(&mut screen, &click(0, 0, 2.0, 1.0)), "\x1b[<0;3;2M");
    assert_eq!(pointed(&mut screen, &click(2, 0, 3.0, 1.0)), "", "a drag is not a click");
    assert_eq!(pointed(&mut screen, &click(1, 0, 3.0, 1.0)), "\x1b[<0;4;2m");
    assert_eq!(pointed(&mut screen, &click(0, 2, 0.0, 0.0)), "\x1b[<2;1;1M", "the right button");
    assert_eq!(pointed(&mut screen, &click(1, 2, 0.0, 0.0)), "\x1b[<2;1;1m");
    assert_eq!(pointed(&mut screen, &click(3, 255, 5.0, 5.0)), "\x1b[<64;6;6M", "the wheel is a fourth button");
    assert_eq!(pointed(&mut screen, &click(4, 255, 5.0, 5.0)), "\x1b[<65;6;6M");
}

/// A program that asked for drags is told about a move with a button down, once a cell.
#[test]
fn a_drag_is_reported_once_a_cell() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?1002h\x1b[?1006h");
    assert_eq!(pointed(&mut screen, &click(2, 255, 1.0, 1.0)), "", "a move with nothing held");
    assert_eq!(pointed(&mut screen, &click(0, 0, 1.0, 1.0)), "\x1b[<0;2;2M");
    assert_eq!(pointed(&mut screen, &click(2, 0, 2.0, 1.0)), "\x1b[<32;3;2M");
    let within = Pointer { x: 27.0, ..click(2, 0, 2.0, 1.0) };
    assert_eq!(pointed(&mut screen, &within), "", "the same cell again");
    assert_eq!(pointed(&mut screen, &click(1, 0, 2.0, 1.0)), "\x1b[<0;3;2m");
    assert_eq!(pointed(&mut screen, &click(2, 255, 4.0, 1.0)), "", "and released, a move is not a drag");
}

fn pasted(screen: &Screen, text: &str, allow_unsafe: bool) -> Option<String> {
    match screen.paste(text, allow_unsafe).expect("a paste encodes") {
        Pasted::Encoded(bytes) => Some(String::from_utf8(bytes).expect("a paste is text")),
        Pasted::Unsafe => None,
    }
}

/// With no bracket, a line ending in a paste is the Enter key.
#[test]
fn a_paste_into_a_program_that_does_not_bracket_is_refused_when_it_holds_a_line_ending() {
    let screen = Screen::new(80, 24);
    assert_eq!(pasted(&screen, "ls -la", false).as_deref(), Some("ls -la"));
    assert_eq!(pasted(&screen, "rm -rf x\n", false), None);
    assert_eq!(pasted(&screen, "one\rtwo", false), None);
    // The person was asked: it goes, a newline as the carriage return the Enter key sends
    assert_eq!(pasted(&screen, "one\ntwo", true).as_deref(), Some("one\rtwo"));
}

/// With one, a line ending is text, and the one thing that can run a command is the marker that ends the
/// bracket.
#[test]
fn a_bracketed_paste_is_refused_only_when_it_holds_the_marker_that_ends_it() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?2004h");
    assert_eq!(pasted(&screen, "one\ntwo", false).as_deref(), Some("\x1b[200~one\ntwo\x1b[201~"));
    assert_eq!(pasted(&screen, "x\x1b[201~\rrm -rf y\r", false), None);

    // Allowed, the marker no longer ends anything: its escape is blanked
    let allowed = pasted(&screen, "x\x1b[201~\rrm", true).expect("an allowed paste is sent");
    assert_eq!(allowed.matches("\x1b[201~").count(), 1, "one end marker, the bracket's own: {allowed:?}");
    assert!(allowed.starts_with("\x1b[200~x") && allowed.ends_with("rm\x1b[201~"));
}

#[test]
fn focus_is_reported_only_to_a_program_that_asked() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(screen.focus(true).expect("focus encodes"), b"");
    screen.feed(b"\x1b[?1004h");
    assert_eq!(screen.focus(true).expect("focus encodes"), b"\x1b[I");
    assert_eq!(screen.focus(false).expect("focus encodes"), b"\x1b[O");
}

/// Shift that went into what was typed is spent, and shift held with control is not: control never
/// composes, so the key is still the letter with both down. A program that asked for the keyboard
/// protocol is where the two are told apart.
#[test]
fn shift_is_spent_on_what_it_typed_and_held_beside_control() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("KeyA", SHIFT | CONTROL, "A", 'a')), "\x1b[97;6u");
    assert_eq!(sent(&mut screen, &key("Digit1", SHIFT | CONTROL, "!", '1')), "\x1b[33;5u");

    screen.feed(b"\x1b[>1u");
    assert_eq!(sent(&mut screen, &key("Digit1", SHIFT, "!", '1')), "!");
    assert_eq!(sent(&mut screen, &key("KeyA", SHIFT, "A", 'a')), "A");
    assert_eq!(sent(&mut screen, &key("KeyA", SHIFT | CONTROL, "A", 'a')), "\x1b[97;6u");
    assert_eq!(sent(&mut screen, &key("KeyB", ALT, "\u{222b}", 'b')), "\u{222b}");
}

/// **A drag that leaves the grid is still a drag.** The pointer goes on being reported, held to the
/// nearest edge, for as long as a button is down — which is what lets a selection in a full-screen
/// program run past the window's side. With nothing held, a pointer outside the grid is nobody's.
#[test]
fn a_drag_past_the_edge_is_reported_at_the_edge_while_a_button_is_held() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?1002h\x1b[?1006h");
    // Eighty columns of ten pixels: 900 is past the right edge, and -30 past the left
    let outside = |button: u8, x: f32| Pointer { x, ..click(2, button, 0.0, 1.0) };
    assert_eq!(pointed(&mut screen, &outside(0, 900.0)), "", "nothing is held");

    assert_eq!(pointed(&mut screen, &click(0, 2, 1.0, 1.0)), "\x1b[<2;2;2M");
    assert_eq!(pointed(&mut screen, &outside(2, 900.0)), "\x1b[<34;80;2M", "held to the last column");
    assert_eq!(pointed(&mut screen, &outside(2, -30.0)), "\x1b[<34;1;2M", "and to the first");
    assert_eq!(pointed(&mut screen, &click(1, 2, 1.0, 1.0)), "\x1b[<2;2;2m");
    assert_eq!(pointed(&mut screen, &outside(2, 900.0)), "", "released");

    // Two down and one let go: the other still holds it
    assert_eq!(pointed(&mut screen, &click(0, 0, 1.0, 1.0)), "\x1b[<0;2;2M");
    assert_eq!(pointed(&mut screen, &click(0, 1, 1.0, 1.0)), "\x1b[<1;2;2M");
    assert_eq!(pointed(&mut screen, &click(1, 1, 1.0, 1.0)), "\x1b[<1;2;2m");
    assert_eq!(pointed(&mut screen, &outside(0, 900.0)), "\x1b[<32;80;2M");
    assert_eq!(pointed(&mut screen, &click(1, 0, 1.0, 1.0)), "\x1b[<0;2;2m");
    assert_eq!(pointed(&mut screen, &outside(0, 950.0)), "");

    // A button the core has no name for is not one that can be held
    assert_eq!(pointed(&mut screen, &click(0, 8, 1.0, 1.0)), "");
    assert_eq!(pointed(&mut screen, &outside(0, 900.0)), "");
}
