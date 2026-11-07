//! The presentation channel: bounded, coalesced, and with capacity an answer can always use.
//!
//! These are the firing cases apack's Phase B asks for, and one thing it asks for that has no case.
//!
//! **Coalescing and the overload report are guarded here**, each failing under its own mutation. **The
//! credit split is not**, and the attempt to guard it is recorded in `src/output.rs`: three mutations that
//! gave answers output's credit passed every case written for them, because in this design an answer is
//! never waiting on anything. What deadlocks is a shared *bounded* queue, which this is not and which no
//! small mutation can make it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

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
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
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
    ask(writing, r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#);
    let session = next(reading)["session"].as_str().expect("a session").to_owned();
    ask(writing, &format!(r#"{{"kind":"write","session":"{session}","data":{}}}"#, serde_json::to_string(command).unwrap()));
    session
}

/// A megabyte, written as fast as the shell can
const SATURATE: &str = "head -c 1048576 /dev/zero | tr '\\0' x\n";

#[test]
fn a_megabyte_of_output_is_coalesced_into_far_fewer_messages_than_chunks() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, SATURATE);

    let mut messages = 0usize;
    let mut bytes = 0usize;
    while bytes < 1_048_576 {
        let event = next(&mut reading);
        if event["kind"] == "output" && event["session"] == session.as_str() {
            messages += 1;
            bytes += event["data"].as_str().unwrap_or_default().len();
        }
    }

    // apack's Phase G measured one bus event per pty chunk: 115 messages for 64 KiB, so about 1,840 per MB
    // (F4). The budget this holds is the ceiling the coalescer buys, not a figure chosen for comfort.
    assert!(messages < 200, "a megabyte arrived as {messages} messages, carrying {bytes} bytes");
    assert!(bytes >= 1_048_576, "every byte arrived: {bytes}");
}

#[test]
fn a_saturating_producer_reports_that_it_was_throttled_rather_than_losing_output() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let session = run(&mut reading, &mut writing, SATURATE);

    // The reader deliberately does nothing for a moment, so the channel's credit runs out
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut bytes = 0usize;
    let mut throttled = 0usize;
    while bytes < 1_048_576 {
        let event = next(&mut reading);
        match event["kind"].as_str() {
            Some("output") if event["session"] == session.as_str() => {
                bytes += event["data"].as_str().unwrap_or_default().len();
            }
            Some("throttled") => throttled += 1,
            _ => {}
        }
    }

    // The overload is reported, and it is a wait rather than a loss: every byte still arrives
    assert!(throttled > 0, "a saturating producer reported no throttling");
    assert!(bytes >= 1_048_576, "output was dropped: {bytes} of 1048576 bytes");
}

#[test]
fn an_answer_arrives_while_a_producer_is_saturating_the_channel() {
    let broker = start();
    let (mut reading, mut writing) = greeted(&broker);
    let flooding = run(&mut reading, &mut writing, SATURATE);

    // Let the channel fill, then ask a question whose answer must not queue behind a megabyte of output
    std::thread::sleep(std::time::Duration::from_millis(400));
    ask(&mut writing, r#"{"kind":"write","session":"s-404","data":"x"}"#);

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
