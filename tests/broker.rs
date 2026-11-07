//! The broker driven as the app will drive it: over a real socket, in the real binary.
//!
//! This is the walking skeleton's own half of Phase S — a keystroke reaching a pty and its output coming
//! back through the protocol — proven here before any of it is wired into the app, so that the protocol is
//! derived from a client that exists rather than proposed to one that does not.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

/// The broker, listening, with the socket it said it was listening on.
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

/// Starts the built binary and **waits for its ready line** rather than retrying a connect: the broker
/// prints where it is listening before its first accept, which is what makes a parent's start deterministic.
fn start() -> Broker {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg(&socket)
        .stdout(Stdio::piped())
        .spawn()
        .expect("the broker starts");

    let mut ready = String::new();
    BufReader::new(process.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut ready)
        .expect("the broker says where it is listening");
    assert!(ready.starts_with("listening "), "{ready:?}");

    Broker { process, socket, _directory: directory }
}

/// One connection, readable and writable from two places: `&Stream` implements both, so an `Arc` is what
/// the crate's deprecated `split` tells callers to use instead of halves.
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

/// A connection to the broker, which every client must then greet.
fn connect(broker: &Broker) -> (BufReader<Wire>, Wire) {
    let name = broker.socket.to_str().expect("a utf-8 socket path").to_fs_name::<GenericFilePath>().expect("a socket name");
    let wire = Wire(std::sync::Arc::new(Stream::connect(name).expect("the broker accepts")));
    (BufReader::new(wire.clone()), wire)
}

fn ask(writing: &mut impl Write, frame: &str) {
    writing.write_all(frame.as_bytes()).expect("the request is written");
    writing.write_all(b"\n").expect("the frame ends");
    writing.flush().expect("the request is flushed");
}

fn next(reading: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reading.read_line(&mut line).expect("an event");
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
}

/// Reads until an event of `kind` arrives, returning it with everything seen on the way.
fn until(reading: &mut impl BufRead, kind: &str) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut seen = Vec::new();
    for _ in 0..4000 {
        let event = next(reading);
        if event["kind"] == kind {
            return (event, seen);
        }
        seen.push(event);
    }
    panic!("no {kind} arrived; saw {} events", seen.len());
}

#[test]
fn a_keystroke_reaches_a_pty_and_its_output_comes_back() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    assert_eq!(next(&mut reading)["kind"], "hello", "the handshake is answered before anything else");

    ask(&mut writing, r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#);
    let spawned = next(&mut reading);
    assert_eq!(spawned["kind"], "spawned");
    let session = spawned["session"].as_str().expect("a session id").to_owned();
    assert!(spawned["pid"].as_u64().expect("a pid") > 0, "the broker owns a real process");

    // The keystroke, and the output it causes
    ask(&mut writing, &format!(r#"{{"kind":"write","session":"{session}","data":"printf skeleton-probe\n"}}"#));
    let mut output = String::new();
    for _ in 0..200 {
        let event = next(&mut reading);
        if event["kind"] == "output" {
            output.push_str(event["data"].as_str().unwrap_or_default());
            if output.contains("skeleton-probe") {
                break;
            }
        }
    }
    assert!(output.contains("skeleton-probe"), "the pty's output came back: {output:?}");

    // A resize is accepted; a pty has one size, so there is nothing to answer
    ask(&mut writing, &format!(r#"{{"kind":"resize","session":"{session}","cols":100,"rows":30}}"#));

    ask(&mut writing, &format!(r#"{{"kind":"kill","session":"{session}"}}"#));
    let (exited, _) = until(&mut reading, "exited");
    assert_eq!(exited["session"].as_str(), Some(session.as_str()), "one exit, reported once");
}

#[test]
fn a_request_for_a_session_that_does_not_exist_is_refused_by_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"write","session":"s-404","data":"x"}"#);
    let failed = next(&mut reading);
    assert_eq!(failed["kind"], "failed");
    // A code, not a message: a caller switches on something the compiler knows
    assert_eq!(failed["code"], "NO_SUCH_SESSION");
    assert_eq!(failed["session"], "s-404");
}

#[test]
fn a_frame_that_is_not_a_request_is_refused_and_the_connection_survives() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"nonsense"}"#);
    assert_eq!(next(&mut reading)["code"], "MALFORMED");

    // Still usable: one bad frame is not a broken connection
    ask(&mut writing, r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#);
    assert_eq!(next(&mut reading)["kind"], "spawned");
}

#[test]
fn a_mismatched_protocol_is_refused_at_the_handshake() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);

    // What a stale `.rev` pin looks like from here
    ask(&mut writing, r#"{"kind":"hello","protocol":999}"#);
    let failed = next(&mut reading);
    assert_eq!(failed["code"], "HANDSHAKE");
    assert!(failed["message"].as_str().unwrap_or_default().contains("999"), "{failed}");
}

#[test]
fn nothing_can_be_asked_before_the_handshake() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#);
    assert_eq!(next(&mut reading)["code"], "HANDSHAKE");
}

#[test]
fn a_shell_that_exits_on_its_own_is_reported_without_anything_polling_for_it() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#);
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();

    ask(&mut writing, &format!(r#"{{"kind":"write","session":"{session}","data":"exit\n"}}"#));
    let (exited, _) = until(&mut reading, "exited");
    assert_eq!(exited["session"].as_str(), Some(session.as_str()));
}

#[test]
fn a_client_that_disconnects_leaves_no_shell_running() {
    let broker = start();
    let pid = {
        let (mut reading, mut writing) = connect(&broker);
        ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
        next(&mut reading);
        ask(&mut writing, r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#);
        let spawned = next(&mut reading);
        spawned["pid"].as_u64().expect("a pid") as i32
        // The connection drops here, which is what an app that quit, crashed or was killed looks like
    };

    // The app cannot end its sessions on the way out — a pack's `onShutdown` is synchronous and runs in
    // `process.on('exit')`, where no socket write completes — so the broker has to be the backstop.
    let gone = (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(25));
        // Signal 0 asks whether the process is still there without touching it
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        !alive
    });
    assert!(gone, "the shell for pid {pid} was still running after its client disconnected");
}
