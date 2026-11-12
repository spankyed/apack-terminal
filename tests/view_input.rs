//! What a view's input becomes, which depends on modes only the terminal knows.
//!
//! Each case sets a mode the way a program does — by writing the sequence — and reads what the same key,
//! click or paste sends before and after. That the answer changes is the reason a view sends events
//! rather than bytes.

use apack_terminal::screen::Screen;
use apack_terminal::view::publish::ViewId;
use apack_terminal::vt::input::{Key, Pasted, Pointer};

const SHIFT: u16 = 1;
const CONTROL: u16 = 2;
const ALT: u16 = 4;

fn key<'a>(code: &'a str, modifiers: u16, text: &'a str, unshifted: char) -> Key<'a> {
    Key { action: 0, code, modifiers, text, unshifted: u32::from(unshifted), composed: false }
}

/// The same key where the view says alt went into what was typed, as the option key does on a Mac.
fn composed<'a>(code: &'a str, modifiers: u16, text: &'a str, unshifted: char) -> Key<'a> {
    Key { composed: true, ..key(code, modifiers, text, unshifted) }
}

/// The view a case's pointer events come from, where the case has only one.
const VIEW: ViewId = ViewId { connection: 1, channel: 1 };

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
    pointed_by(screen, VIEW, pointer)
}

fn pointed_by(screen: &mut Screen, view: ViewId, pointer: &Pointer) -> String {
    String::from_utf8(screen.pointer(view, pointer).expect("a pointer event encodes")).expect("a report is text")
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

/// Alt before a letter is the escape prefix — where the key *is* alt, which is everywhere but a Mac whose
/// option key is typing characters. Only the view knows which keyboard it is in front of, so it says:
/// alt is alt unless the key is marked as having composed its character.
#[test]
fn alt_is_the_escape_prefix_unless_the_view_says_it_composed_the_character() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("KeyB", ALT, "b", 'b')), "\x1bb");
    // **With shift as well, shift made the capital and alt is still held**: this is the key Linux and
    // Windows report for Alt+Shift+A, and what a shell binds is the escape and the capital
    assert_eq!(sent(&mut screen, &key("KeyA", ALT | SHIFT, "A", 'a')), "\x1bA");
    // Composing instead, the character the option key typed is what is sent
    assert_eq!(sent(&mut screen, &composed("KeyB", ALT, "\u{222b}", 'b')), "\u{222b}");
    assert_eq!(sent(&mut screen, &composed("KeyA", ALT | SHIFT, "\u{c5}", 'a')), "\u{c5}");
}

/// A character typed through AltGr arrives as its text with control and alt left out, which is how a
/// view says they were the way it was typed: it is sent as that character.
#[test]
fn a_character_typed_through_altgr_is_sent_as_the_character() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("KeyQ", 0, "@", 'q')), "@");
    assert_eq!(sent(&mut screen, &key("Digit7", 0, "{", '7')), "{");
    screen.feed(b"\x1b[>1u");
    assert_eq!(sent(&mut screen, &key("KeyQ", 0, "@", 'q')), "@");
}

/// A browser reports some keys by the control character they stand for. The core derives that from the
/// key itself, and given the character as well it would be sent twice over; so it is given the key alone.
#[test]
fn a_key_reported_as_a_control_character_is_sent_once() {
    let mut screen = Screen::new(80, 24);
    assert_eq!(sent(&mut screen, &key("KeyC", CONTROL, "\x03", 'c')), "\x03");
    assert_eq!(sent(&mut screen, &key("Enter", 0, "\r", '\0')), "\r");
    assert_eq!(sent(&mut screen, &key("Backspace", 0, "\x7f", '\0')), "\x7f");
    assert_eq!(sent(&mut screen, &key("Escape", 0, "\x1b", '\0')), "\x1b");
    // The code point a Mac reports an arrow as names the key too, and is no character to send
    assert_eq!(sent(&mut screen, &key("ArrowUp", 0, "\u{f700}", '\0')), "\x1b[A");
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
    assert_eq!(sent(&mut screen, &composed("KeyB", ALT, "\u{222b}", 'b')), "\u{222b}");
    // Alt that composed nothing is reported as alt, with the key it was held with
    assert_eq!(sent(&mut screen, &key("KeyB", ALT, "b", 'b')), "\x1b[98;3u");
    // And shift held beside a letter it did not change — caps lock made the capital small again — was
    // not spent on it: the key is the letter with shift down
    assert_eq!(sent(&mut screen, &key("KeyA", SHIFT, "a", 'a')), "\x1b[97;2u");
}

/// A key held down repeats, which a program that asked to be told of key events is told as a repeat and
/// not as the key pressed again.
#[test]
fn a_held_key_is_reported_as_a_repeat_to_a_program_that_asked() {
    let mut screen = Screen::new(80, 24);
    let repeat = Key { action: 1, ..key("KeyA", 0, "a", 'a') };
    assert_eq!(sent(&mut screen, &repeat), "a", "to a program that did not ask, a held key types again");
    // Disambiguate, report event types, and every key as an escape code
    screen.feed(b"\x1b[>11u");
    assert_eq!(sent(&mut screen, &key("KeyA", 0, "a", 'a')), "\x1b[97u");
    assert_eq!(sent(&mut screen, &repeat), "\x1b[97;1:2u");
    assert_eq!(sent(&mut screen, &Key { action: 2, ..repeat }), "\x1b[97;1:3u");
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

    // A button the core has no name for is not one that can be held: it has no release to let it go
    for unnamed in [3, 5, 7, 8] {
        assert_eq!(pointed(&mut screen, &click(0, unnamed, 1.0, 1.0)), "");
        assert_eq!(pointed(&mut screen, &outside(0, 900.0)), "", "button {unnamed} is not held");
    }
}

/// **Which buttons are down is a view's, not the session's.** One view's press does not make another
/// view's move a drag, and a press whose release never comes — the view went, or lost focus with the
/// button down, so the release was somebody else's — is let go with the view.
#[test]
fn a_button_one_view_holds_is_not_held_by_another_and_goes_with_the_view() {
    let (first, second) = (ViewId { connection: 1, channel: 1 }, ViewId { connection: 2, channel: 1 });
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?1002h\x1b[?1006h");
    // A move past the right edge, which is reported — at the edge — only to a view holding a button down
    let dragged_out = || Pointer { x: 900.0, ..click(2, 0, 0.0, 1.0) };
    let at_the_edge = "\x1b[<32;80;2M";

    assert_eq!(pointed_by(&mut screen, first, &click(0, 0, 1.0, 1.0)), "\x1b[<0;2;2M");
    assert_eq!(pointed_by(&mut screen, second, &dragged_out()), "", "the other view holds nothing");
    assert_eq!(pointed_by(&mut screen, first, &dragged_out()), at_the_edge, "the one that pressed is dragging");

    // The view that pressed detaches without ever releasing, and comes back: it holds nothing
    screen.detach_view(first);
    assert_eq!(pointed_by(&mut screen, first, &dragged_out()), "");

    // Pressed again, and its connection goes
    assert_eq!(pointed_by(&mut screen, first, &click(0, 0, 1.0, 1.0)), "\x1b[<0;2;2M");
    assert_eq!(pointed_by(&mut screen, first, &dragged_out()), at_the_edge);
    screen.detach_connection(1);
    assert_eq!(pointed_by(&mut screen, first, &dragged_out()), "");

    // Pressed again, and it loses focus with the button down
    assert_eq!(pointed_by(&mut screen, first, &click(0, 0, 1.0, 1.0)), "\x1b[<0;2;2M");
    assert_eq!(pointed_by(&mut screen, first, &dragged_out()), at_the_edge);
    screen.release_pointer(first);
    assert_eq!(pointed_by(&mut screen, first, &dragged_out()), "");
}

/// A position is four numbers a view sent, and a number need not be one: what is not a position is
/// reported as nothing, and one far outside the grid is the grid's edge and not a cell number made of it.
#[test]
fn a_pointer_position_that_is_not_a_number_is_no_position_and_a_far_one_is_the_edge() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?1002h\x1b[?1006h");
    assert_eq!(pointed(&mut screen, &click(0, 0, 1.0, 1.0)), "\x1b[<0;2;2M");
    let held = click(2, 0, 4.0, 1.0);
    for not_a_number in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(pointed(&mut screen, &Pointer { x: not_a_number, ..held }), "");
        assert_eq!(pointed(&mut screen, &Pointer { y: not_a_number, ..held }), "");
        assert_eq!(pointed(&mut screen, &Pointer { cell_width: not_a_number, ..held }), "");
        assert_eq!(pointed(&mut screen, &Pointer { cell_height: not_a_number, ..held }), "");
    }
    assert_eq!(pointed(&mut screen, &Pointer { x: 1e30, y: 1e30, ..held }), "\x1b[<32;80;24M");
    assert_eq!(pointed(&mut screen, &Pointer { x: -1e30, y: -1e30, ..held }), "\x1b[<32;1;1M");
    // A cell of no size, or of a size no view measures, is taken as the nearest a cell can be
    assert_eq!(
        pointed(&mut screen, &Pointer { x: 9.0, y: 0.0, cell_width: 0.0, cell_height: 1e30, ..held }),
        "\x1b[<32;10;1M"
    );
}

/// **How far outside is one cell, on every side.** A program that asked for the pointer in pixels is told
/// the position as it stands rather than the cell it is in, so it is where what the core is given can be
/// read: a drag carried any distance past an edge is a cell's width or height past it, and no further.
#[test]
fn a_drag_far_outside_is_told_in_pixels_as_one_cell_past_the_edge() {
    let mut screen = Screen::new(80, 24);
    screen.feed(b"\x1b[?1002h\x1b[?1016h");
    assert_eq!(pointed(&mut screen, &click(0, 0, 1.0, 1.0)), "\x1b[<0;15;30M");
    let held = click(2, 0, 4.0, 1.0);
    assert_eq!(pointed(&mut screen, &held), "\x1b[<32;45;30M", "inside, where it is");
    // Eighty columns of ten pixels and twenty-four rows of twenty: the grid ends at 800 by 480
    assert_eq!(pointed(&mut screen, &Pointer { x: 1e30, y: 1e30, ..held }), "\x1b[<32;810;500M");
    assert_eq!(pointed(&mut screen, &Pointer { x: -1e30, y: -1e30, ..held }), "\x1b[<32;-10;-20M");
    // A cell's width past the right edge and its height past the bottom are each as far as it goes
    assert_eq!(pointed(&mut screen, &Pointer { x: 810.0, y: 500.0, ..held }), "\x1b[<32;810;500M");
    assert_eq!(pointed(&mut screen, &Pointer { x: 811.0, y: 501.0, ..held }), "\x1b[<32;810;500M");
    assert_eq!(pointed(&mut screen, &Pointer { x: 809.0, y: 499.0, ..held }), "\x1b[<32;809;499M");
}
