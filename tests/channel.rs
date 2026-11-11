//! The presentation channel: bounded, coalesced, and with capacity an answer can always use.
//!
//! These are the firing cases apack's Phase B asks for, and one thing it asks for that has no case.
//!
//! **Coalescing and the overload report are guarded here**, each failing under its own mutation. **The
//! credit split is not**, and the attempt to guard it is recorded in `src/output.rs`: three mutations that
//! gave answers output's credit passed every case written for them, because in this design an answer is
//! never waiting on anything. What deadlocks is a shared *bounded* queue, which this is not and which no
//! small mutation can make it.
//!
//! **POSIX only, and what that leaves unproven is stated rather than implied.** Every case here needs a
//! shell that can produce a megabyte as fast as the pty will take it, which is `head -c /dev/zero | tr` —
//! the subject is the broker's own coalescer and credit, and the shell is only a byte source. Windows ships
//! no equivalent one-liner, and a `for /L` loop producing that much output is slower than the channel it is
//! meant to saturate, so the cases would be measuring cmd.exe.
//!
//! What that leaves open there: the bound's arithmetic includes *"whatever the socket's buffers absorb"*,
//! and a named pipe's buffering is not a Unix socket's. The coalescing and the credit are the broker's own
//! code and platform-independent; how much output it takes to fill the pipe is not. The transport itself is
//! covered on Windows by `tests/broker.rs`, which passes there in full.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use apack_terminal::output::{OUTPUT_CAP, OUTPUT_WINDOW};
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

struct Broker {
    process: Child,
    socket: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn start() -> Broker {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg(&socket)
        .stdout(Stdio::piped())
        .spawn()
        .expect("the broker starts");
    let mut ready = String::new();
    BufReader::new(process.stdout.as_mut().expect("piped stdout")).read_line(&mut ready).expect("a ready line");
    Broker { process, socket, _directory: directory }
}

#[derive(Clone)]
struct Wire(std::sync::Arc<Stream>);

impl std::io::Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut &*self.0, buffer)
    }
}

impl Write for Wire {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

fn greeted(broker: &Broker) -> (BufReader<Wire>, Wire) {
    let name = broker.socket.to_str().expect("utf-8").to_fs_name::<GenericFilePath>().expect("a name");
    let wire = Wire(std::sync::Arc::new(Stream::connect(name).expect("the broker accepts")));
    let (mut reading, mut writing) = (BufReader::new(wire.clone()), wire);
    ask(&mut writing, &format!(r#"{{"kind":"hello","protocol":{}}}"#, apack_terminal::protocol::PROTOCOL_VERSION));
    let mut line = String::new();
    reading.read_line(&mut line).expect("the greeting");
    (reading, writing)
}

fn ask(writing: &mut impl Write, frame: &str) {
    writing.write_all(frame.as_bytes()).expect("written");
    writing.write_all(b"\n").expect("framed");
    writing.flush().expect("flushed");
}

fn next(reading: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reading.read_line(&mut line).expect("an event");
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
}

/// Opens a session and runs `command` in it, returning the session id
fn run(reading: &mut impl BufRead, writing: &mut impl Write, command: &str) -> String {
    ask(
        writing,
        &serde_json::json!({
            "kind": "spawn", "cols": 80, "rows": 24, "shell": null,
            "args": [], "env": [], "unset": [], "cwd": null,
        })
        .to_string(),
    );
    let session = next(reading)["session"].as_str().expect("a session").to_owned();
    ask(
        writing,
        &format!(r#"{{"kind":"write","session":"{session}","text":{}}}"#, serde_json::to_string(command).unwrap()),
    );
    session
}

/// A megabyte, written as fast as the shell can
const SATURATE: &str = "head -c 1048576 /dev/zero | tr '\\0' x\n";

/// Enough to fill the channel's credit while nobody is reading, which a megabyte is not.
///
/// **The arithmetic is the point, and getting it wrong is how the overload case came to prove nothing.**
/// Credit runs out only when `OUTPUT_CREDIT` coalesced messages are in flight, and a message holds up to
/// `OUTPUT_CAP`, so the bound is about 4 MiB — on top of whatever the socket's own buffers absorb, which on
/// macOS auto-tune into the megabytes. A megabyte of output arrives as sixteen messages and never touches
/// any of it. The case passed anyway, because `take` was timing the mutex rather than the condvar and so
/// reported every flush as throttled; with that fixed it reported nothing, which is what sent this number
/// up. 64 MiB is past every buffer in the path.
const OVERLOAD: &str = "head -c 67108864 /dev/zero | tr '\\0' x\n";

/// **A flush reaches the cap, which is the one thing the case below cannot say.**
///
/// That case derives its budget *from* `OUTPUT_CAP`, deliberately, so that a slower machine is allowed more
/// windows and the budget stays load-independent. The cost is that it cannot notice the cap itself moving:
/// shrink it sixtyfold and the expected message count shrinks with it, and the case still passes — which a
/// mutation run found, `64 * 1024` surviving as `64 + 1024`.
///
/// So this asserts a size rather than a count, against a literal, and the direction is what makes it safe:
/// coalescing can only put *more* into a message than the window alone would, never less, and nothing above
/// the cap is ever held back. A busy machine therefore pushes this measurement up. A cap of 1,088 bytes caps
/// every message at 1,088 and fails here; the real cap clears the floor sixteenfold.
#[test]
fn output_is_coalesced_up_to_the_cap_rather_than_to_some_smaller_size() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, SATURATE);

    /// A sixteenth of the real cap: past any plausible window's worth at this rate, and far under 64 KiB
    const FLOOR: usize = 4 * 1024;

    let mut largest = 0usize;
    let mut bytes = 0usize;
    while bytes < 1_048_576 {
        let event = next(&mut reading);
        if event["kind"] == "output" && event["session"] == session.as_str() {
            let held = event["text"].as_str().unwrap_or_default().len();
            bytes += held;
            largest = largest.max(held);
        }
    }

    assert!(
        largest > FLOOR,
        "a megabyte arrived in messages of at most {largest} bytes, under the {FLOOR} a cap of \
         {OUTPUT_CAP} has to clear: the cap is not what is bounding a flush"
    );
}

#[test]
fn a_megabyte_of_output_is_coalesced_into_far_fewer_messages_than_chunks() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, SATURATE);

    let started = std::time::Instant::now();
    let mut messages = 0usize;
    let mut bytes = 0usize;
    while bytes < 1_048_576 {
        let event = next(&mut reading);
        if event["kind"] == "output" && event["session"] == session.as_str() {
            messages += 1;
            bytes += event["text"].as_str().unwrap_or_default().len();
        }
    }
    let took = started.elapsed();

    // **The ceiling is derived from the two bounds, not chosen.** A message leaves when the window closes or
    // the cap fills, so a run of this length cannot have produced more than one per window plus one per cap,
    // and one more for whatever was held when the shell exited. That makes the budget load-independent: a
    // slower machine takes more windows and is allowed more messages, where a round number would have been
    // either slack on this machine or flaky on a busy one.
    //
    // apack's Phase G measured the before, one bus event per pty chunk: 115 messages for 64 KiB, about 1,840
    // per MB (F4). The same corpus in the app now costs 6 messages for 64 KiB.
    let budget = took.as_millis().div_ceil(OUTPUT_WINDOW.as_millis()) as usize + bytes.div_ceil(OUTPUT_CAP) + 1;
    assert!(
        messages <= budget,
        "a megabyte arrived as {messages} messages carrying {bytes} bytes, past the {budget} that \
         {took:?} of windows and {OUTPUT_CAP}-byte caps allow"
    );
    assert!(bytes >= 1_048_576, "every byte arrived: {bytes}");
}

/// A producer nobody is holding up reports no throttling at all — the half the saturating case cannot say.
///
/// **This is the one the app found.** `take` timed the mutex it holds to reach the credit rather than the
/// condvar it waits on, so taking an uncontended lock — a few hundred nanoseconds — counted as waiting, and
/// every flush reported itself throttled. Driving `less` put a line in the app's log for each one, at a level
/// that reads as an error, saying it had waited `0ms`: a report firing constantly whose own number said
/// nothing had happened. Saturating the channel cannot catch it, because then the waits are real.
#[test]
fn a_producer_nobody_is_holding_up_reports_no_throttling() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, SATURATE);

    let mut bytes = 0usize;
    let mut throttles: Vec<u64> = Vec::new();
    while bytes < 1_048_576 {
        let event = next(&mut reading);
        match event["kind"].as_str() {
            Some("output") if event["session"] == session.as_str() => {
                bytes += event["text"].as_str().unwrap_or_default().len();
            }
            Some("throttled") => throttles.push(event["waited_us"].as_u64().unwrap_or_default()),
            _ => {}
        }
    }

    assert!(
        throttles.is_empty(),
        "a megabyte with a reader keeping up is not an overload, and reporting one makes the signal noise: {throttles:?}"
    );
}

#[test]
fn a_saturating_producer_reports_that_it_was_throttled_rather_than_losing_output() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, OVERLOAD);

    // **A deliberately slow reader, not a reader that races the producer.** Saturation used to depend on the
    // shell outrunning this thread, which a loaded machine reverses: the reader drains faster than the pty
    // fills, nothing is ever throttled, and the case fails for the machine's reasons rather than the code's —
    // which it did, once, with four other brokers of this file's running beside it. One event per 20 ms is
    // slower than any shell writing to a pty, whatever else the machine is doing.
    let mut bytes = 0usize;
    let mut waits: Vec<u64> = Vec::new();
    for _ in 0..400 {
        let event = next(&mut reading);
        match event["kind"].as_str() {
            Some("output") if event["session"] == session.as_str() => {
                bytes += event["text"].as_str().unwrap_or_default().len();
            }
            Some("throttled") => waits.push(event["waited_us"].as_u64().expect("a wait in microseconds")),
            _ => {}
        }
        if !waits.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // The overload is reported rather than silent, which is what the goal requires of the bound
    assert!(!waits.is_empty(), "a saturating producer reported no throttling after {bytes} bytes");
    // **And the report carries the wait it measured.** `as_millis` truncated a few hundred microseconds to
    // `0`, so every report read as "no wait" — a number that says nothing is worse than no number, because
    // whoever reads it concludes the mechanism misfired. That the waits are real is the other half of the
    // same fix: `take` now times the condvar rather than the mutex it holds to reach it
    assert!(
        waits.iter().all(|wait| *wait > 0),
        "a throttle reported a wait of zero, so its unit threw the measurement away: {waits:?}"
    );

    // **And it is a wait rather than a loss**, read after the throttle rather than in a case of its own: once
    // this reader stops sleeping it is faster than the shell, so a second run would not be throttled again and
    // the no-loss assertion would be the one that went flaky. The throttle above has already happened.
    while bytes < 67_108_864 {
        let event = next(&mut reading);
        if event["kind"] == "output" && event["session"] == session.as_str() {
            bytes += event["text"].as_str().unwrap_or_default().len();
        }
    }
    // At least, not exactly: the shell's prompt and its echo of the command are output too
    assert!(bytes >= 67_108_864, "output was dropped: {bytes} of 67108864 bytes");
}

#[test]
fn an_answer_arrives_while_a_producer_is_saturating_the_channel() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let flooding = run(&mut reading, &mut writing, SATURATE);

    // Let the channel fill, then ask a question whose answer must not queue behind a megabyte of output
    std::thread::sleep(std::time::Duration::from_millis(400));
    ask(&mut writing, r#"{"kind":"write","session":"s-404","text":"x"}"#);

    // This shows an answer arriving during saturation, which is worth having — but it does **not** by
    // itself prove the credit split: an answer that spent output's credit was measured arriving just as
    // promptly, because the writer returns credit as fast as it writes. What distinguishes the two is the
    // next case, where nobody is reading.
    let mut outputs_before_the_answer = 0usize;
    for _ in 0..4000 {
        let event = next(&mut reading);
        match event["kind"].as_str() {
            Some("failed") => {
                assert_eq!(event["code"], "NO_SUCH_SESSION");
                assert!(
                    outputs_before_the_answer < 64,
                    "the answer waited behind {outputs_before_the_answer} output messages, which is the whole credit"
                );
                return;
            }
            Some("output") if event["session"] == flooding.as_str() => outputs_before_the_answer += 1,
            _ => {}
        }
    }
    panic!("the answer never arrived while output was saturating the channel");
}

#[test]
fn a_request_is_still_carried_out_while_output_has_filled_the_channel() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, SATURATE);

    // **Nobody reads**, so output backs up behind the socket's own buffer and the coalescer waits on credit.
    // What this holds is that the request loop does not: the app can still stop a runaway terminal. It does
    // not distinguish the credit split — see the module comment — but it does catch a request loop that
    // comes to depend on output draining.
    std::thread::sleep(std::time::Duration::from_millis(500));
    ask(&mut writing, &format!(r#"{{"kind":"kill","session":"{session}"}}"#));

    // Now drain. The kill must already have been carried out while the channel was full
    for _ in 0..8000 {
        let event = next(&mut reading);
        if event["kind"] == "exited" && event["session"] == session.as_str() {
            return;
        }
    }
    panic!("the kill was never carried out: the request loop was blocked by output");
}
