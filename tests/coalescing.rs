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

/// What a client would see, reading until the coalescer has nothing more to say
fn seen(chunks: &[&[u8]], ending: bool) -> String {
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

    let mut text = String::new();
    while let Ok(out) = answers.recv_timeout(Duration::from_secs(2)) {
        if let Event::Output { text: chunk, .. } = out.event {
            text.push_str(&chunk);
        }
    }
    text
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
