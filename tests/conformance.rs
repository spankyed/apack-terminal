//! `libghostty-vt` against the parser this broker ships, which is Phase 0a of apack's one-VT-core goal.
//!
//! **The question is whether the two produce the same screen**, because the goal buys one implementation
//! parsing a session's bytes instead of two that can disagree — and that is only worth doing if the one
//! that would replace ours agrees with it first. A difference here is not automatically a defect in either:
//! it is a decision, recorded in the goal's evidence, about which parser is right.
//!
//! Behind the `ghostty` feature, so the default build acquires neither the dependency nor its archive.
//! `cargo xtask vt-prebuilt` prints the two variables a build needs.
#![cfg(feature = "ghostty")]

use apack_terminal::vt::Terminal;

/// The link, which is the first thing Phase 0a has to prove and the cheapest thing to lose.
///
/// Everything else in this suite reads the grid, and a grid read that returns nothing looks the same
/// whether the library is absent, the archive is for another target, or the parse genuinely produced an
/// empty screen. So this case does the three things that cannot be mistaken for each other — construct,
/// write, read back a number the terminal must answer — and does them before any comparison runs.
#[test]
fn the_library_is_linked_and_answers() {
    let mut terminal = Terminal::new(20, 4).expect("a 20x4 terminal");

    assert_eq!(terminal.cols().expect("cols"), 20, "the terminal answers the width it was built with");
    assert_eq!(terminal.rows().expect("rows"), 4, "and the height");

    // A write the parser has to do something with, rather than an empty one it could ignore: text, a CSI
    // cursor move and an SGR reset, which is three different paths through the state machine
    terminal.feed(b"hello\x1b[2;3Hworld\x1b[0m");

    // Still answering afterwards, which is what distinguishes a parse from a crash in the library. The
    // grid itself is read by the cases that compare it; this one is about whether the call works at all.
    assert_eq!(terminal.cols().expect("cols after a write"), 20, "the terminal survived a parse");
}

#[path = "_support/streams.rs"]
mod support;
use apack_terminal::screen::{Screen, Snapshot};
use apack_terminal::vt::GhosttyScreen;
use support::streams;

/// How much history a comparison asks for: the same figure `parser.rs` snapshots with.
const HISTORY: u32 = 20;

/// The grid both implementations produce for one delivery of a stream.
fn both(chunks: &[&[u8]], cols: u16, rows: u16) -> (Snapshot, Snapshot) {
    let mut ours = Screen::new(cols, rows);
    let mut theirs = GhosttyScreen::new(cols, rows).expect("a ghostty terminal");
    for chunk in chunks {
        ours.feed(chunk);
        theirs.feed(chunk);
    }
    (ours.snapshot(HISTORY), theirs.snapshot(HISTORY).expect("a ghostty snapshot"))
}

/// Every way two snapshots differ, named well enough to decide from without rerunning.
fn differences(what: &str, ours: &Snapshot, theirs: &Snapshot) -> Vec<String> {
    let mut found = Vec::new();
    if ours.alt_screen != theirs.alt_screen {
        found.push(format!("{what}: alt_screen ours={} theirs={}", ours.alt_screen, theirs.alt_screen));
    }
    if (ours.cursor_row, ours.cursor_column) != (theirs.cursor_row, theirs.cursor_column) {
        found.push(format!(
            "{what}: cursor ours={},{} theirs={},{}",
            ours.cursor_row, ours.cursor_column, theirs.cursor_row, theirs.cursor_column,
        ));
    }
    if ours.lines.len() != theirs.lines.len() {
        found.push(format!("{what}: rows ours={} theirs={}", ours.lines.len(), theirs.lines.len()));
    }
    for (row, (ours, theirs)) in ours.lines.iter().zip(&theirs.lines).enumerate() {
        if ours != theirs {
            found.push(format!("{what}: row {row} ours={ours:?} theirs={theirs:?}"));
        }
    }
    found
}

/// **Phase 0a's question.** Every difference is reported, not the first: the deliverable is a decided list.
#[test]
fn the_two_parsers_produce_the_same_grid() {
    let mut found = Vec::new();
    for (name, stream) in streams() {
        let (ours, theirs) = both(&[&stream], 20, 4);
        found.extend(differences(name, &ours, &theirs));
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// The same, one byte at a time: where a wrapper that buffers or resets per call stops agreeing.
#[test]
fn the_two_parsers_agree_byte_by_byte() {
    let mut found = Vec::new();
    for (name, stream) in streams() {
        let singles: Vec<&[u8]> = stream.iter().map(std::slice::from_ref).collect();
        let (ours, theirs) = both(&singles, 20, 4);
        found.extend(differences(&format!("{name} (one byte at a time)"), &ours, &theirs));
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// **Not a comparison**: the new wrapper must agree with *itself* however the stream is cut.
///
/// This is the property `parser.rs` was built for, and the one a wrapper breaks on its own. It can fail
/// while every whole-stream grid above matches, so passing those is not a substitute for it.
#[test]
fn ghostty_agrees_with_itself_at_every_boundary() {
    let mut found = Vec::new();
    for (name, stream) in streams() {
        let whole = {
            let mut screen = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
            screen.feed(&stream);
            screen.snapshot(HISTORY).expect("a snapshot")
        };
        for at in 1..stream.len() {
            let mut screen = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
            screen.feed(&stream[..at]);
            screen.feed(&stream[at..]);
            let split = screen.snapshot(HISTORY).expect("a snapshot");
            if split != whole {
                found.extend(differences(&format!("{name} split at {at}"), &whole, &split));
            }
        }
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// **The grids compared above are not empty**, which a comparison of two blank screens cannot tell you.
///
/// This repo's own rule is that a check reporting nothing may have looked at nothing, and two
/// implementations that both answered an empty grid would agree perfectly. So the corpus is asserted to
/// put text on the screen: every stream produces at least one non-blank row, and together they produce
/// more rows than the viewport, which is what proves history is being read rather than skipped.
#[test]
fn the_streams_being_compared_put_something_on_the_screen() {
    let mut drawn = 0;
    let mut rows = 0;
    for (name, stream) in streams() {
        let (_, theirs) = both(&[&stream], 20, 4);
        let text: Vec<&String> = theirs.lines.iter().filter(|line| !line.is_empty()).collect();
        assert!(!text.is_empty(), "{name} drew nothing: {:?}", theirs.lines);
        drawn += text.len();
        rows += theirs.lines.len();
    }
    assert!(drawn >= 8, "the corpus drew {drawn} non-blank rows, which is too few to be comparing anything");
    assert!(rows > 8 * 4, "{rows} rows over eight streams means no history was read past the viewport");
}

/// **The comparison can fail**, proven by planting a difference rather than by trusting that it would.
///
/// Three, one per field the comparison reads, because a reporter that only noticed rows would have passed
/// every case above just as quietly.
#[test]
fn a_planted_difference_is_reported() {
    let text = {
        let (ours, _) = both(&[b"hello"], 20, 4);
        let (mut theirs, _) = both(&[b"hello"], 20, 4);
        theirs.lines[0] = "goodbye".to_owned();
        differences("planted text", &ours, &theirs)
    };
    assert_eq!(text.len(), 1, "a changed row is one difference: {text:?}");
    assert!(text[0].contains("row 0"), "it names the row: {}", text[0]);

    let cursor = {
        let (ours, _) = both(&[b"hello"], 20, 4);
        let (mut theirs, _) = both(&[b"hello"], 20, 4);
        theirs.cursor_column += 1;
        differences("planted cursor", &ours, &theirs)
    };
    assert_eq!(cursor.len(), 1, "a moved cursor is one difference: {cursor:?}");
    assert!(cursor[0].contains("cursor"), "it says so: {}", cursor[0]);

    let screen = {
        let (ours, _) = both(&[b"hello"], 20, 4);
        let (mut theirs, _) = both(&[b"hello"], 20, 4);
        theirs.alt_screen = !theirs.alt_screen;
        differences("planted screen", &ours, &theirs)
    };
    assert_eq!(screen.len(), 1, "the wrong screen is one difference: {screen:?}");
    assert!(screen[0].contains("alt_screen"), "it says so: {}", screen[0]);
}

/// **A terminal answers questions, and the broker is what answers them.**
///
/// ConPTY's first act is a cursor-position request and it waits for the reply before initialising, so a
/// session that answers nothing does not start on Windows — which is invisible on POSIX and is why this is
/// compared in 0a rather than met in Phase B. The stream is a device status report and a primary device
/// attributes request, the two a shell or a TUI actually sends.
#[test]
fn both_answer_what_a_program_asks() {
    let asks: &[(&str, &[u8])] = &[
        ("cursor position (DSR 6)", b"\x1b[6n"),
        ("primary device attributes", b"\x1b[c"),
        ("both, after output", b"before\x1b[6n\x1b[cafter"),
    ];
    let mut found = Vec::new();
    for (what, stream) in asks {
        let mut ours = Screen::new(20, 4);
        let mut theirs = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
        ours.feed(stream);
        theirs.feed(stream);
        let (ours, theirs) = (ours.take_replies(), theirs.take_replies());
        // Compared as one string: the two may split a reply across callbacks differently, and where a
        // boundary falls is not something a program can observe
        let (mut ours, theirs) = (ours.concat(), theirs.concat());
        assert!(!ours.is_empty(), "{what}: the wrapper answered nothing, so there is nothing to compare");
        // The decided difference is rewritten to what the core says, so everything else in the same
        // answer is still compared rather than the whole case being skipped
        for (from, to) in DECIDED_REPLIES {
            ours = ours.replace(from, to);
        }
        if ours != theirs {
            found.push(format!("{what}: ours={ours:?} theirs={theirs:?}"));
        }
    }
    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// **The one answer the two parsers give differently, and the core is right.**
///
/// Primary device attributes. `alacritty_terminal` answers `CSI ?6c`, a VT102; the core answers
/// `CSI ?62;22c`, a VT220 with ANSI colour. The broker advertises `TERM=xterm-256color`, and a program
/// that asks what it is talking to and hears "VT102" may turn off features the terminal has — so this is
/// a difference where adopting the core *fixes* something, which is the decision recorded in
/// `docs/references/evidence/one-vt-core.md` rather than a divergence to preserve.
///
/// Written as a rewrite rather than a skipped case so that everything else in the same answer — the
/// cursor report beside it in the third stream — is still compared.
const DECIDED_REPLIES: &[(&str, &str)] = &[("\u{1b}[?6c", "\u{1b}[?62;22c")];

/// The rewrite above is not hiding a disagreement that has since changed.
///
/// If either parser's answer moves, the entry stops matching and the case above reports the whole reply
/// as a difference. This one fails sooner and says why: an entry that no longer describes either side is
/// dead text, and dead text in an exceptions table is what this repo refuses.
#[test]
fn the_decided_reply_is_still_what_each_parser_says() {
    let mut ours = Screen::new(20, 4);
    let mut theirs = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
    ours.feed(b"\x1b[c");
    theirs.feed(b"\x1b[c");
    let (from, to) = DECIDED_REPLIES[0];
    assert_eq!(ours.take_replies().concat(), from, "the wrapper's device attributes answer moved");
    assert_eq!(theirs.take_replies().concat(), to, "the core's device attributes answer moved");
}

/// An alternate-screen application, resized under it, and left: the case `parser.rs` drives a real `less`
/// through. **Resize is where a reflow difference would show**, and the corpus above never resizes.
#[test]
fn both_survive_a_resize_under_an_alternate_screen() {
    let mut ours = Screen::new(40, 10);
    let mut theirs = GhosttyScreen::new(40, 10).expect("a ghostty terminal");
    for chunk in [&b"shell scrollback\r\n"[..], &b"\x1b[?1049h\x1b[2J\x1b[HTUI content here"[..]] {
        ours.feed(chunk);
        theirs.feed(chunk);
    }
    let mut found = differences("on the alternate screen", &ours.snapshot(0), &theirs.snapshot(0).expect("a snapshot"));

    ours.resize(20, 5);
    theirs.resize(20, 5).expect("a resize");
    found.extend(differences("resized under it", &ours.snapshot(0), &theirs.snapshot(0).expect("a snapshot")));

    ours.feed(b"\x1b[?1049l");
    theirs.feed(b"\x1b[?1049l");
    found.extend(differences("after leaving it", &ours.snapshot(20), &theirs.snapshot(20).expect("a snapshot")));

    assert!(found.is_empty(), "{} difference(s):\n{}", found.len(), found.join("\n"));
}

/// **The two fields `markers.rs` exists for — and the finding that the two read different sequences.**
///
/// OSC 133 draws no cells, so a stream carrying it would earn a committed grid snapshot that is a blank
/// picture: a recorded file that does not derive the thing it is about. The assertion it deserves is
/// `Facts` equality, so it belongs here with the comparison.
///
/// **What the comparison found is that neither side reads the other's family.** `markers.rs` reads
/// OSC 633, which is VS Code's shell integration; `libghostty-vt` 1.0 reads OSC 133, which is the
/// FinalTerm sequence iTerm2, Ghostty's own integration and most shell scripts emit. Each reports nothing
/// for the other. So this case asserts each side against the family it reads, rather than against the
/// other side, and the consequence for Phase B is recorded in the goal's evidence: the union is strictly
/// better than either, so `markers.rs` keeps its 633 scanning and gains 133 from the core.
#[test]
fn each_parser_reads_the_shell_integration_it_knows() {
    /// What a reporting shell says, in both families: the same four steps either way.
    fn run(prefix: &str, line: &str) -> Vec<u8> {
        // 633 carries the command line in `E`, 133 as `C`'s `cmdline` property
        let command = if prefix == "633" { format!("\x1b]633;E;{line}\x07") } else { String::new() };
        let starting =
            if prefix == "633" { "\x1b]633;C\x07".to_owned() } else { format!("\x1b]133;C;cmdline={line}\x07") };
        format!("\x1b]{prefix};A\x07\x1b]{prefix};B\x07{command}{starting}").replace("\\x1b", "\x1b").into_bytes()
    }

    // Ours reads 633 and the core reads 133, so each is driven with its own and asserted to read it.
    // A case driven with the family a side ignores would pass by reading nothing, which is why these
    // assert values rather than equality.
    let mut ours = Screen::new(20, 4);
    ours.feed(&run("633", "ls -la"));
    assert!(ours.facts().running, "the wrapper reads OSC 633's command start");
    assert_eq!(ours.facts().command.as_deref(), Some("ls -la"), "and its command line");
    ours.feed(b"\x1b]633;D;1\x07");
    assert!(!ours.facts().running, "and its command end");
    assert_eq!(ours.facts().exit_status, Some(1), "with the status it reported");

    let mut theirs = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
    theirs.feed(&run("133", "ls -la"));
    assert!(theirs.facts().running, "the core reads OSC 133's command start");
    assert_eq!(theirs.facts().command.as_deref(), Some("ls -la"), "and its command line");
    theirs.feed(b"\x1b]133;D;1\x07");
    assert!(!theirs.facts().running, "and its command end");
    assert_eq!(theirs.facts().exit_status, Some(1), "with the status it reported");

    // **A finish with no status is absent rather than zero**, which is the contract both keep: a shell
    // that says nothing about how a command ended is not saying it succeeded.
    let mut ours = Screen::new(20, 4);
    ours.feed(b"\x1b]633;C\x07\x1b]633;D\x07");
    assert_eq!(ours.facts().exit_status, None, "the wrapper reports no status as none");
    let mut theirs = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
    theirs.feed(b"\x1b]133;C\x07\x1b]133;D\x07");
    assert_eq!(theirs.facts().exit_status, None, "and so does the core");

    // **OSC 7 is the one both read, and the decode is the broker's either way**: the core stores the raw
    // URI and says so, so `cwd_from_url` is shared rather than written twice.
    let reported = b"\x1b]7;file:///tmp/some%20where\x1b\\";
    let mut ours = Screen::new(20, 4);
    let mut theirs = GhosttyScreen::new(20, 4).expect("a ghostty terminal");
    ours.feed(reported);
    theirs.feed(reported);
    assert_eq!(ours.facts().cwd.as_deref(), Some("/tmp/some where"), "the wrapper decodes the URI");
    assert_eq!(theirs.facts().cwd, ours.facts().cwd, "and the core agrees once it is decoded");
}
