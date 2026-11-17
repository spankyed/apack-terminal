//! The presentation channel: bounded, coalesced, and with capacity an answer can always use.
//!
//! These are the firing cases apack's Phase B asks for, and one thing it asks for that has no case.
//!
//! **The overload report is guarded here**, and how much a message holds is guarded in `tests/coalescing.rs`,
//! where the chunks' arrival is the case's to arrange rather than a busy machine's. **The
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

use std::io::Write;
use std::process::{Child, Command, Stdio};

use apack_terminal::output::{OUTPUT_CAP, OUTPUT_CREDIT};
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use crate::heard::Heard;

struct Broker {
    process: Child,
    socket: std::path::PathBuf,
    _directory: tempfile::TempDir,
    /// What stops the broker if this case is ended before it can (`tests/reaper.rs`)
    reaper: crate::reaper::Reaper,
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.reaper.stand_down();
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
    crate::heard::lines(process.stdout.take().expect("piped stdout")).next();
    let reaper = crate::reaper::watching(&process, env!("CARGO_BIN_EXE_apack-terminal"));
    Broker { process, socket, _directory: directory, reaper }
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

/// A connection that has been greeted, read as fast as the broker sends.
fn greeted(broker: &Broker) -> (Heard<serde_json::Value>, Wire) {
    greeted_as(broker, crate::heard::events)
}

/// The same, **read at the case's pace** (`Heard::paced`): for a case about what the broker does while its
/// client is not reading, which a reader running ahead would empty the channel under. Only those: pacing hands
/// over each event between two threads, and a case whose subject is a reader that keeps up is then one that
/// does not.
fn greeted_paced(broker: &Broker) -> (Heard<serde_json::Value>, Wire) {
    greeted_as(broker, crate::heard::paced_events)
}

fn greeted_as(broker: &Broker, hearing: fn(Wire) -> Heard<serde_json::Value>) -> (Heard<serde_json::Value>, Wire) {
    let name = broker.socket.to_str().expect("utf-8").to_fs_name::<GenericFilePath>().expect("a name");
    let wire = Wire(std::sync::Arc::new(Stream::connect(name).expect("the broker accepts")));
    let (reading, mut writing) = (hearing(wire.clone()), wire);
    ask(&mut writing, &format!(r#"{{"kind":"hello","protocol":{}}}"#, apack_terminal::protocol::PROTOCOL_VERSION));
    reading.next();
    (reading, writing)
}

fn ask(writing: &mut impl Write, frame: &str) {
    writing.write_all(frame.as_bytes()).expect("written");
    writing.write_all(b"\n").expect("framed");
    writing.flush().expect("flushed");
}

#[track_caller]
fn next(reading: &mut Heard<serde_json::Value>) -> serde_json::Value {
    reading.next()
}

/// Opens a session and runs `command` in it, returning the session id.
///
/// **The spawn names no program, and its `SHELL` names `/bin/sh`.** A spawn naming none is the default
/// program — `portable-pty` runs whatever the builder's `SHELL` says as a login shell — and that path is
/// what runs here. Left to inherit `SHELL`, it is the login shell of whoever ran the suite with all of
/// their configuration, and on a busy machine these cases waited out their deadline on somebody's prompt
/// rather than on the channel they are about.
fn run(reading: &mut Heard<serde_json::Value>, writing: &mut impl Write, command: &str) -> String {
    ask(
        writing,
        &serde_json::json!({
            "kind": "spawn", "cols": 80, "rows": 24, "shell": null,
            "args": [], "env": [["SHELL", "/bin/sh"]], "unset": [], "cwd": null,
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

/// A producer with no end, whose every line says where it is: `1`, `2`, `3`, … for as long as it is let run.
///
/// **No end, because the throttle has to come before the producer stops.** Credit runs out once
/// `OUTPUT_CREDIT` coalesced messages are in flight, on top of what the socket's buffers absorb — eight
/// kilobytes each way for a local socket on macOS (`net.local.stream.sendspace`), and on a reader as slow as
/// the one below it came after under a megabyte. A producer of a fixed size has to be sized past every buffer
/// on every platform, and then all of it has to be read to say none was lost, which made this case its
/// volume: sixty-four megabytes, several times the deadline on a busy machine.
///
/// **Numbered, because a loss is then a gap rather than a count.** What follows a throttle is read for gaps
/// for a credit's worth of output, which is everything that can have been waiting while it was reported.
///
/// **`awk` rather than `seq`**, whose BSD form prints through `%g`: past 999999 every line is `1e+06`.
const OVERLOAD: &str = "awk 'BEGIN { for (i = 1; ; i++) print i }'\n";

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
    let (mut reading, mut writing) = greeted_paced(&broker);
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
    //
    // A throttle is reported once the wait is over and ahead of what was held through it, so whatever a lossy
    // channel let go of is a gap within the credit's worth of output that follows it. A line cut between two
    // messages is joined before it is read, and the first is passed over, since its start came before the
    // throttle and was not kept.
    let past = OUTPUT_CREDIT * OUTPUT_CAP;
    let (mut text, mut after, mut last, mut joined) = (String::new(), 0usize, None::<u64>, false);
    while after < past {
        let event = next(&mut reading);
        if event["kind"] != "output" || event["session"] != session.as_str() {
            continue;
        }
        let arrived = event["text"].as_str().unwrap_or_default();
        after += arrived.len();
        text.push_str(arrived);
        if !joined {
            let Some(end) = text.find("\r\n") else { continue };
            text.drain(..end + 2);
            joined = true;
        }
        // Everything up to the last whole line; the rest waits for the message that ends it
        let whole = text.rfind("\r\n").map_or(0, |at| at + 2);
        // **A line may end in more than one carriage return.** The macOS tty, which turns a newline into `\r\n`,
        // sends `\r\r\n` for one now and then while this producer is held up — once in about one run in four
        // here. A terminal draws it as it draws `\r\n`, and no number is missing from around it
        for line in text[..whole].split("\r\n").map(|line| line.trim_end_matches('\r')).filter(|line| !line.is_empty())
        {
            // The shell's prompt and its echo of the command come before the first number, and nothing after it
            let Ok(number) = line.parse::<u64>() else {
                assert!(last.is_none(), "a line that is not the next number followed {last:?}: {line:?}");
                continue;
            };
            if let Some(before) = last {
                assert_eq!(number, before + 1, "output was dropped: {before} was followed by {number}");
            }
            last = Some(number);
        }
        text.drain(..whole);
    }
    assert!(last.is_some(), "no numbered line arrived after the throttle in {after} bytes");
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
    let (mut reading, mut writing) = greeted_paced(&broker);
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
