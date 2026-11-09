//! What the coalescer sends, driven directly rather than through a pty and a socket.
//!
//! **A pty read is bytes and a character can straddle two of them.** `read` returns whatever the kernel
//! had, so a multi-byte sequence is split whenever the boundary falls inside it. A flush between the two
//! reads used to convert each half on its own, replacing both with U+FFFD — every TUI's box drawing, all
//! CJK and every emoji, while ASCII was never affected, which is why it went unnoticed. These drive
//! `coalesce` with the split made on purpose, which no case through a shell can do reliably.
use std::sync::mpsc::channel;
use std::time::Duration;

use apack_terminal::output::{Credit, OUTPUT_CREDIT, coalesce};
use apack_terminal::protocol::Event;

/// What a client would see, as one string
fn seen(chunks: &[&[u8]], ending: bool) -> String {
    events(chunks, ending).concat()
}

/// The same, **as the events it arrived in** — which is the thing a consumer may not assume anything about
fn events(chunks: &[&[u8]], ending: bool) -> Vec<String> {
    let (sending, incoming) = channel::<Vec<u8>>();
    let (outgoing, answers) = channel();
    let credit = Credit::new(OUTPUT_CREDIT);
    std::thread::spawn(move || coalesce("s-1".to_owned(), incoming, outgoing, credit));

    for (at, chunk) in chunks.iter().enumerate() {
        sending.send(chunk.to_vec()).expect("the coalescer is listening");
        // Past the window, so each chunk is its own flush — which is the split this is about
        if at + 1 < chunks.len() {
            std::thread::sleep(Duration::from_millis(60));
        }
    }
    if ending {
        drop(sending);
    } else {
        std::thread::sleep(Duration::from_millis(60));
    }

    let mut arrived = Vec::new();
    while let Ok(out) = answers.recv_timeout(Duration::from_secs(2)) {
        if let Event::Output { text: chunk, .. } = out.event {
            arrived.push(chunk);
        }
    }
    arrived
}

/// The defect, with the bytes that showed it: `é` is `C3 A9`, and a read can end between them
#[test]
fn a_character_split_across_two_reads_arrives_whole() {
    assert_eq!(seen(&[b"a\xC3", b"\xA9b"], true), "aéb");
}

/// Every width, because each holds back a different number of bytes: two, three and four
#[test]
fn a_split_is_rejoined_whatever_the_character_is_worth() {
    // `字` is E5 AD 97 and `😀` is F0 9F 98 80 — split at every interior boundary
    assert_eq!(seen(&[b"\xE5", b"\xAD\x97"], true), "字");
    assert_eq!(seen(&[b"\xE5\xAD", b"\x97"], true), "字");
    assert_eq!(seen(&[b"\xF0", b"\x9F\x98\x80"], true), "😀");
    assert_eq!(seen(&[b"\xF0\x9F\x98", b"\x80"], true), "😀");
}

/// **The held-back byte is not held for ever.** A stream that ends mid-character will never complete it, so
/// the last flush sends what it has and the replacement character is the honest answer. Without this the
/// bytes would be dropped silently, which is worse than showing that something arrived broken.
#[test]
fn a_stream_ending_mid_character_still_delivers_what_it_had() {
    assert_eq!(seen(&[b"a\xC3"], true), "a\u{FFFD}");
}

/// **Invalid is not incomplete**, and waiting cannot mend it: `0xFF` begins no sequence, so it is sent and
/// replaced as it always was rather than held back against a continuation that cannot come.
///
/// **The byte has to be the last one**, which a mutation is what showed: with `a\xFFb` the scan stops at the
/// `b` and never looks at the `\xFF`, so treating an invalid lead as a four-byte sequence passed. Only a
/// buffer *ending* in one reaches that arm, and under that mutation this waits for ever and sees only `a`.
#[test]
fn a_byte_that_is_invalid_rather_than_incomplete_is_not_held_back() {
    assert_eq!(seen(&[b"a\xFF"], false), "a\u{FFFD}");
}

/// And a complete character is never held back, which is what would make every flush one byte short
#[test]
fn a_complete_character_is_sent_at_once() {
    assert_eq!(seen(&[b"ab\xC3\xA9"], false), "abé");
}

/// **An escape sequence may arrive in pieces, and this is what says so out loud.**
///
/// What is held back is a partial *UTF-8* sequence — at most three bytes, because that is the longest tail
/// of an incomplete character (`output::sendable`). An escape sequence is not that: `\x1b[?1049h` is eight
/// bytes of perfectly valid ASCII, so a flush falling inside it sends the first half immediately and the
/// rest in the next event.
///
/// Both ends that consume this feed are fine with it — a VT parser is a state machine, and the broker's own
/// marker scanner carries its state across chunks (`tests/markers.rs` cuts every one) — so the contract
/// stays where it is rather than growing. What it costs is that **a consumer must never match a pattern
/// against one event**, and a comment saying so is weaker than a case: this is the behaviour, asserted, next
/// to the helper that would otherwise look safe.
///
/// A test in this repository's own suite matched `\x1b[?1049h` against a single event and passed for weeks,
/// until a loaded CI runner fragmented the read and it waited ten seconds for a match that could no longer
/// happen.
#[test]
fn an_escape_sequence_is_not_held_back_the_way_a_character_is() {
    let halves = events(&[b"before\x1b[?10", b"49hafter"], true);

    assert_eq!(halves.len(), 2, "each flush is its own event: {halves:?}");
    assert_eq!(halves.concat(), "before\u{1b}[?1049hafter", "and the stream is whole across them");
    assert!(
        !halves.iter().any(|event| event.contains("\u{1b}[?1049h")),
        "no single event holds the sequence, which is the whole point: {halves:?}"
    );
}

/// **The flow gate, driven directly**, because what it does to a reader is not observable through a socket.
///
/// A paused session is one whose pty is not read, and the only witness to that from outside is output that
/// does not arrive — which a case can only assert by waiting. Here the parked thread says so itself.
mod flow {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    use apack_terminal::output::Flow;

    /// Long enough that a reader which was never parked has certainly got through, short enough that a case
    /// asserting it did not ends quickly. Load can only make a parked reader later, never earlier
    const SHORT: Duration = Duration::from_millis(250);

    /// A thread that parks on `flow` and says when it got through.
    fn reader(flow: &std::sync::Arc<Flow>) -> std::sync::mpsc::Receiver<()> {
        let (through, got) = channel();
        let flow = std::sync::Arc::clone(flow);
        std::thread::spawn(move || {
            flow.wait_while_paused();
            let _ = through.send(());
        });
        got
    }

    #[test]
    fn a_reader_is_parked_while_paused_and_goes_through_on_a_resume() {
        let flow = Flow::reading();
        flow.pause();
        let got = reader(&flow);

        assert_eq!(got.recv_timeout(SHORT), Err(RecvTimeoutError::Timeout), "a paused reader was not parked");

        flow.resume();
        assert_eq!(got.recv_timeout(SHORT), Ok(()), "a resumed reader is still parked");
    }

    #[test]
    fn a_reader_goes_straight_through_when_nothing_paused_it() {
        let flow = Flow::reading();

        assert_eq!(reader(&flow).recv_timeout(SHORT), Ok(()), "an unpaused reader was parked");
    }

    /// **The session is gone and nobody is left to resume it.** Without this the reader thread of a session
    /// that was paused when it ended parks for ever: its `Exited` still arrives, since that is waited for on
    /// the child, so nothing else says anything is wrong — a thread per such session, held until the broker
    /// exits.
    #[test]
    fn closing_wakes_a_parked_reader_and_never_parks_another() {
        let flow = Flow::reading();
        flow.pause();
        let got = reader(&flow);
        assert_eq!(got.recv_timeout(SHORT), Err(RecvTimeoutError::Timeout), "a paused reader was not parked");

        flow.close();

        assert_eq!(got.recv_timeout(SHORT), Ok(()), "closing left a reader parked with nobody to resume it");
        // And a pause after the close cannot park the next pass of that same reader on its way to EOF
        flow.pause();
        assert_eq!(reader(&flow).recv_timeout(SHORT), Ok(()), "a closed session's reader was parked again");
    }
}
