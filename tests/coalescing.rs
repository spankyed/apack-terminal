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
        if let Some(Event::Output { text: chunk, .. }) = out.event {
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

/// **When a session's bytes leave**, with a window long enough that "at once" and "held" cannot be confused.
mod pacing {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use apack_terminal::output::{Credit, OUTPUT_CREDIT, Outgoing, coalesce_within};
    use apack_terminal::protocol::Event;

    /// Far longer than any wait below, so a held chunk is still held when the case looks
    const WINDOW: Duration = Duration::from_secs(30);
    /// Load can only make an arrival later, so this bounds "has not arrived" and never "arrived"
    const SHORT: Duration = Duration::from_millis(300);
    const SOON: Duration = Duration::from_secs(10);

    fn text(out: Outgoing) -> String {
        match out.event {
            Some(Event::Output { text, .. }) => text,
            other => panic!("expected output, got {other:?}"),
        }
    }

    /// **A quiet session's bytes are not held.** The window exists to batch a burst; on a keystroke's echo
    /// it is only delay — a median of 8.8 ms per character, measured, when it applied to everything.
    #[test]
    fn the_first_bytes_after_a_quiet_spell_leave_at_once() {
        let (sending, incoming) = channel::<Vec<u8>>();
        let (outgoing, answers) = channel();
        let credit = Credit::new(OUTPUT_CREDIT);
        std::thread::spawn(move || coalesce_within(WINDOW, "s-1".to_owned(), incoming, outgoing, credit));

        sending.send(b"echo".to_vec()).expect("the coalescer is listening");
        assert_eq!(text(answers.recv_timeout(SOON).expect("held for the window")), "echo");
    }

    /// **A burst that stutters is still one burst.** Bytes that arrive inside the window after a flush wait
    /// out the window, however long the pauses between them: a producer a busy machine holds up pauses often,
    /// and sending at each pause made its output a message a read.
    ///
    /// The window here is longer than the case, so what is held is still held when the stream ends, and the
    /// last flush says what the window had gathered without the case waiting for it.
    #[test]
    fn a_burst_that_pauses_inside_the_window_is_sent_as_one_message() {
        let (sending, incoming) = channel::<Vec<u8>>();
        let (outgoing, answers) = channel();
        let credit = Credit::new(OUTPUT_CREDIT);
        std::thread::spawn(move || coalesce_within(WINDOW, "s-1".to_owned(), incoming, outgoing, credit));

        sending.send(b"echo".to_vec()).expect("the coalescer is listening");
        assert_eq!(text(answers.recv_timeout(SOON).expect("the first flush")), "echo");

        for piece in [b"one ".as_slice(), b"two ", b"three"] {
            sending.send(piece.to_vec()).expect("the coalescer is listening");
            // A pause hundreds of times what a quiet gap was: nothing is sent during it
            assert!(answers.recv_timeout(SHORT).is_err(), "a pause inside the window sent what it held");
        }
        drop(sending);
        assert_eq!(text(answers.recv_timeout(SOON).expect("the stream's end")), "one two three", "one message");
    }

    /// **What a burst holds leaves when its window closes**, with nothing more arriving to push it out.
    #[test]
    fn what_a_burst_holds_is_sent_when_its_window_closes() {
        /// Short enough to wait out here, and far under `SOON`
        const BRIEF: Duration = Duration::from_millis(500);
        let (sending, incoming) = channel::<Vec<u8>>();
        let (outgoing, answers) = channel();
        let credit = Credit::new(OUTPUT_CREDIT);
        std::thread::spawn(move || coalesce_within(BRIEF, "s-1".to_owned(), incoming, outgoing, credit));

        sending.send(b"echo".to_vec()).expect("the coalescer is listening");
        assert_eq!(text(answers.recv_timeout(SOON).expect("the first flush")), "echo");
        sending.send(b"held".to_vec()).expect("the coalescer is listening");
        assert_eq!(text(answers.recv_timeout(SOON).expect("the window closed on it")), "held");
        drop(sending);
    }

    /// A buffer that reaches the cap does not wait for the window
    #[test]
    fn a_full_buffer_leaves_without_waiting_for_the_window() {
        let (sending, incoming) = channel::<Vec<u8>>();
        let (outgoing, answers) = channel();
        let credit = Credit::new(OUTPUT_CREDIT);
        std::thread::spawn(move || coalesce_within(WINDOW, "s-1".to_owned(), incoming, outgoing, credit));

        sending.send(b"first".to_vec()).expect("the coalescer is listening");
        assert_eq!(text(answers.recv_timeout(SOON).expect("the first flush")), "first");

        sending.send(vec![b'x'; apack_terminal::output::OUTPUT_CAP]).expect("the coalescer is listening");
        let full = text(answers.recv_timeout(SOON).expect("a full buffer waited for the window"));
        assert_eq!(full.len(), apack_terminal::output::OUTPUT_CAP);
    }
}

/// **Chunks that arrive together leave as one message a cap, and a message is the whole cap.**
///
/// Every chunk is queued before the coalescer starts, so none of them arrives after a quiet spell: what is
/// measured is how much a message holds, and not how a machine happened to schedule a producer.
///
/// **The sizes are a literal**, so the cap moving is a failure rather than a budget that moves with it: a
/// cap of 1,088 bytes sends these as sixty-four messages of two chunks each. apack's Phase G measured the
/// before, one bus event per pty chunk: 115 messages for 64 KiB.
#[test]
fn chunks_that_arrive_together_leave_as_one_message_a_cap() {
    let (sending, incoming) = channel::<Vec<u8>>();
    let (outgoing, answers) = channel();
    // A pty read is at most eight kilobytes; one kilobyte is a hundred and twenty-eight of them
    for _ in 0..128 {
        sending.send(vec![b'x'; 1024]).expect("the coalescer has not started");
    }
    drop(sending);
    std::thread::spawn(move || coalesce("s-1".to_owned(), incoming, outgoing, Credit::new(OUTPUT_CREDIT)));

    let mut sizes = Vec::new();
    // Ends when the coalescer does, which is once it has flushed the last of what it was given
    while let Ok(out) = answers.recv() {
        if let Some(Event::Output { text, .. }) = out.event {
            sizes.push(text.len());
        }
    }
    assert_eq!(sizes, [64 * 1024, 64 * 1024], "128 chunks of 1 KiB, sent as messages of these sizes");
}
