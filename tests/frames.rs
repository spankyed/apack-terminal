//! The frame decoder driven as a decoder, which is the only part of the protocol a stranger reaches.
//!
//! Everything else in the broker runs on a `Request` that already parsed. These bytes are the ones an
//! unknown sender controls, and the process they reach owns every terminal the app has open — so the claim
//! is not that a bad frame is reported well, it is that **no byte string gets anything other than one of
//! five answers**. Hence proptest rather than a list: a hand-written case tests the malformed frame whoever
//! wrote it thought of.
//!
//! Three properties, in increasing order of how much they find:
//!
//! - **Arbitrary bytes are classified.** The cheap one, and the one that covers the reader: a cap, a
//!   newline that never comes, UTF-8 that is not UTF-8.
//! - **A valid frame with one byte changed is classified.** Random bytes are never nearly-valid JSON, so
//!   this is the half that reaches the deserializer with a tag it half-recognises, a number where a string
//!   belongs and a truncated string.
//! - **A request round-trips.** Serialize every variant and read it back: a field whose `Serialize` and
//!   `Deserialize` disagree is a frame the app can send and the broker cannot read, which no amount of
//!   fuzzing the broker's side would show — and the generated `node/protocol.ts` is written from the same
//!   derives, so it is the app's half of this seam too.

use std::io::BufReader;
use std::sync::atomic::{AtomicUsize, Ordering};

use apack_terminal::frames::{Frame, MAX_FRAME_BYTES, read_frame};
use apack_terminal::protocol::{Request, Role, Until};
use proptest::prelude::*;

/// Read one frame out of a buffer, as the broker's loop does.
fn frame_of(bytes: &[u8]) -> Frame {
    let mut reader = BufReader::new(bytes);
    let mut line = String::new();
    read_frame(&mut reader, &mut line)
}

/// Every frame the buffer holds, in order, until it ends.
fn frames_of(bytes: &[u8]) -> Vec<Frame> {
    let mut reader = BufReader::new(bytes);
    let mut line = String::new();
    let mut read = Vec::new();
    loop {
        match read_frame(&mut reader, &mut line) {
            Frame::Ended => return read,
            frame => read.push(frame),
        }
    }
}

/// What the decoder said, as one word, for the population assertions below.
fn kind_of(frame: &Frame) -> &'static str {
    match frame {
        Frame::Request(_) => "request",
        Frame::Blank => "blank",
        Frame::Malformed(_) => "malformed",
        Frame::TooLong(_) => "too long",
        Frame::Ended => "ended",
    }
}

/// A refusal that says nothing is a refusal nobody can act on, so this is checked wherever one is produced.
fn says_why(frame: &Frame) -> bool {
    match frame {
        Frame::Malformed(message) | Frame::TooLong(message) => !message.trim().is_empty(),
        _ => true,
    }
}

/// One of every request, so the round trip covers the whole message set rather than the easy variants.
fn requests() -> impl Strategy<Value = Request> {
    prop_oneof![
        (any::<u32>(), prop_oneof![Just(Role::Client), Just(Role::Lifetime)])
            .prop_map(|(protocol, role)| Request::Hello { protocol, role }),
        (
            any::<u16>(),
            any::<u16>(),
            proptest::option::of("[^\u{0}]{0,20}"),
            proptest::collection::vec("[^\u{0}]{0,10}", 0..4),
            proptest::collection::vec(("[A-Z_]{1,8}", "[^\u{0}]{0,10}"), 0..4),
            proptest::collection::vec("[A-Z_]{1,8}", 0..4),
            proptest::option::of("[^\u{0}]{0,20}"),
        )
            .prop_map(|(cols, rows, shell, args, env, unset, cwd)| Request::Spawn {
                cols,
                rows,
                shell,
                args,
                env,
                unset,
                cwd
            }),
        ("[^\u{0}]{0,40}", "(?s).{0,40}").prop_map(|(session, text)| Request::Write { session, text }),
        ("[^\u{0}]{0,40}", any::<u16>(), any::<u16>()).prop_map(|(session, cols, rows)| Request::Resize {
            session,
            cols,
            rows
        }),
        ("[^\u{0}]{0,40}", any::<u32>()).prop_map(|(session, history)| Request::Screen { session, history }),
        "[^\u{0}]{0,40}".prop_map(|session| Request::Facts { session }),
        ("[^\u{0}]{0,40}", "(?s).{0,20}", any::<u32>()).prop_map(|(session, text, history)| Request::Search {
            session,
            text,
            history
        }),
        (
            "[^\u{0}]{0,40}",
            prop_oneof![Just(Until::Idle), Just(Until::Exit), "(?s).{0,20}".prop_map(|text| Until::Text { text })],
            any::<u32>(),
        )
            .prop_map(|(session, until, timeout_ms)| Request::Wait { session, until, timeout_ms }),
        "[^\u{0}]{0,40}".prop_map(|session| Request::Kill { session }),
    ]
}

proptest! {
    /// **Arbitrary bytes get one of five answers.** The property is that this returns at all: a decoder
    /// that indexed into the line, unwrapped a parse or looked past the cap would panic here, and a panic
    /// in the broker's read loop ends every terminal the app has open.
    #[test]
    fn arbitrary_bytes_are_classified(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let frame = frame_of(&bytes);
        prop_assert!(says_why(&frame), "a refusal that says nothing: {frame:?}");
    }

    /// The same, for bytes that are text — so the deserializer is reached rather than the UTF-8 check.
    #[test]
    fn arbitrary_text_is_classified(text in "(?s).{0,2000}") {
        let frame = frame_of(format!("{text}\n").as_bytes());
        prop_assert!(says_why(&frame), "a refusal that says nothing: {frame:?}");
    }

    /// **A request the app can send is a request the broker can read.** A `Serialize` and a `Deserialize`
    /// that disagree — a renamed tag, an `Option` written as `null` and refused on the way back, a `u16`
    /// written as a string — is a frame that crosses the seam and cannot be read at the other end, and the
    /// generated TypeScript is written from these same derives.
    #[test]
    fn a_request_survives_the_round_trip(request in requests()) {
        let written = serde_json::to_string(&request).expect("a request serializes");
        prop_assert!(!written.contains('\n'), "a frame may not hold a newline: {written:?}");
        match frame_of(format!("{written}\n").as_bytes()) {
            Frame::Request(read) => {
                let again = serde_json::to_string(&read).expect("a request serializes");
                prop_assert_eq!(written, again);
            }
            other => prop_assert!(false, "{written} read back as {}", kind_of(&other)),
        }
    }
}

/// How many kinds the mutation property produced, which is what says it looked at anything.
static MUTATED_KINDS: AtomicUsize = AtomicUsize::new(0);
/// Bit per kind, so the assertion below names which were missing rather than counting.
const REQUEST_BIT: usize = 1;
const MALFORMED_BIT: usize = 2;
const ENDED_BIT: usize = 4;

proptest! {
    /// **A valid frame with one byte changed is classified.** This is the half that reaches the
    /// deserializer in a state it has to decide about, which random bytes never do: `"kind":"spaw"`,
    /// `"cols":"80"`, a string cut off mid-escape, a brace too few.
    ///
    /// Three outcomes are legitimate, and which one is not the point: a mutation can land in a `text` field
    /// and leave a valid frame, break the JSON, or put a byte in that is not UTF-8 and so is not text for
    /// the reader to hand on at all. So the property is the classification, and the population assertion
    /// after it is what says the strategy produced more than one of them.
    #[test]
    fn a_mutated_frame_is_classified(
        request in requests(),
        at in 0usize..4096,
        how in 0u8..3,
        byte in any::<u8>(),
    ) {
        let mut written = serde_json::to_string(&request).expect("a request serializes").into_bytes();
        prop_assume!(!written.is_empty());
        let at = at % written.len();
        match how {
            0 => written[at] = byte,
            1 => written.insert(at, byte),
            // At least one byte: truncating to nothing deletes the frame rather than mutating it
            _ => written.truncate(at.max(1)),
        }
        written.push(b'\n');
        let frame = frame_of(&written);
        prop_assert!(says_why(&frame), "a refusal that says nothing: {frame:?}");
        let bit = match frame {
            Frame::Request(_) => REQUEST_BIT,
            Frame::Malformed(_) => MALFORMED_BIT,
            // A byte mutation can leave the line invalid UTF-8, which the reader cannot hand on as text
            Frame::Ended => ENDED_BIT,
            // A mutation cannot reach the cap, and at least one byte is always kept
            other => return Err(TestCaseError::fail(format!("a mutated frame read as {}", kind_of(&other)))),
        };
        MUTATED_KINDS.fetch_or(bit, Ordering::Relaxed);
    }
}

/// The population check for the property above: a strategy that only ever produced one outcome would pass
/// it while testing half of what it claims to.
#[test]
fn the_mutations_reach_both_outcomes() {
    a_mutated_frame_is_classified();
    let seen = MUTATED_KINDS.load(Ordering::Relaxed);
    assert!(seen & REQUEST_BIT != 0, "no mutation left a readable frame; the strategy only breaks them");
    assert!(seen & MALFORMED_BIT != 0, "no mutation broke a frame; the strategy is not mutating anything");
}

/// **The cap is the frame's own length, and the byte past it is the refusal.** The reader takes one byte
/// more than the cap for exactly this: a frame at the limit is a frame, and a frame one byte longer is not.
/// Nothing asserted this boundary, so a `>=` here would have been a megabyte paste refused for being a
/// megabyte.
#[test]
fn a_frame_at_the_cap_is_read_and_one_byte_past_it_is_refused() {
    // The newline counts toward the cap, since what is measured is the line the reader produced
    let at_the_cap = write_frame_of(MAX_FRAME_BYTES - 1);
    assert_eq!(at_the_cap.len() + 1, MAX_FRAME_BYTES);
    assert!(
        matches!(frame_of(format!("{at_the_cap}\n").as_bytes()), Frame::Request(_)),
        "a frame exactly at the cap was refused"
    );

    let one_past = write_frame_of(MAX_FRAME_BYTES);
    assert!(
        matches!(frame_of(format!("{one_past}\n").as_bytes()), Frame::TooLong(_)),
        "a frame one byte past the cap was read"
    );
}

/// A `write` frame whose serialized length is exactly `bytes`, padded in its text.
fn write_frame_of(bytes: usize) -> String {
    let empty = serde_json::to_string(&Request::Write { session: "s".to_owned(), text: String::new() })
        .expect("a request serializes");
    let padding = bytes.checked_sub(empty.len()).expect("the frame fits");
    serde_json::to_string(&Request::Write { session: "s".to_owned(), text: "a".repeat(padding) })
        .expect("a request serializes")
}

/// **A sender that is never going to send a newline is cut off at the cap**, which is the reason the cap
/// exists at all: the broker's memory, and every session in it, must not be a stranger's to take by
/// declining to finish a sentence.
#[test]
fn a_frame_without_an_end_is_refused_rather_than_read_for_ever() {
    let endless = vec![b'a'; MAX_FRAME_BYTES * 2];
    assert!(matches!(frame_of(&endless), Frame::TooLong(_)), "an endless frame was read");
}

/// **One bad frame does not cost the next one.** A client holding live sessions sends a frame the broker
/// cannot read; the frames after it are still its own. This is why `Malformed` continues and only
/// `TooLong` stops: past the cap the next frame boundary is no longer known.
#[test]
fn a_refused_frame_does_not_cost_the_frames_after_it() {
    let buffer =
        "{\"kind\":\"facts\",\"session\":\"one\"}\nnot json at all\n\n{\"kind\":\"kill\",\"session\":\"two\"}\n";
    let kinds: Vec<_> = frames_of(buffer.as_bytes()).iter().map(kind_of).collect();
    assert_eq!(kinds, vec!["request", "malformed", "blank", "request"]);
}

/// A last frame with no trailing newline is still a frame: nothing in the protocol says a sender must send
/// one before closing, and a client that writes and exits is the ordinary case for tooling.
#[test]
fn a_last_frame_needs_no_newline() {
    assert!(matches!(frame_of(b"{\"kind\":\"facts\",\"session\":\"one\"}"), Frame::Request(_)));
}

/// **Bytes that are not UTF-8 end the connection rather than being answered.** There is nothing to answer:
/// a sender that is not writing text is not speaking this protocol, and a refusal would be read by nobody.
#[test]
fn bytes_that_are_not_text_end_the_connection() {
    assert!(matches!(frame_of(b"{\"kind\":\"facts\",\"session\":\"\xff\xfe\"}\n"), Frame::Ended));
}

/// **A `hello` with no role is a client**, which is the `#[serde(default)]` on that field and the whole of
/// what keeps a raw caller, a probe and a client written before roles existed working as they did.
#[test]
fn a_hello_without_a_role_is_a_client() {
    match frame_of(b"{\"kind\":\"hello\",\"protocol\":1}\n") {
        Frame::Request(Request::Hello { protocol, role }) => {
            assert_eq!(protocol, 1);
            assert_eq!(role, Role::Client);
        }
        other => panic!("a hello without a role read as {}", kind_of(&other)),
    }
}
