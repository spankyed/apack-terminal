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

    Broker {
        process,
        socket,
        _directory: directory,
    }
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
    let name = broker
        .socket
        .to_str()
        .expect("a utf-8 socket path")
        .to_fs_name::<GenericFilePath>()
        .expect("a socket name");
    let stream = Stream::connect(name).expect("the broker accepts");
    // **Every read is bounded**, so a case waiting for something that will never come fails instead of
    // hanging — which a mutation that stopped resizes reaching the pty turned into a ten-minute hang.
    stream
        .set_recv_timeout(Some(std::time::Duration::from_secs(10)))
        .expect("a receive timeout");
    let wire = Wire(std::sync::Arc::new(stream));
    (BufReader::new(wire.clone()), wire)
}

fn ask(writing: &mut impl Write, frame: &str) {
    writing
        .write_all(frame.as_bytes())
        .expect("the request is written");
    writing.write_all(b"\n").expect("the frame ends");
    writing.flush().expect("the request is flushed");
}

/// One event, or a failure saying none came. Bounded by the socket's own receive timeout, set on connect.
fn next(reading: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    match reading.read_line(&mut line) {
        Ok(0) => panic!("the broker closed the connection"),
        // A guard that hangs is worse than one that fails: a mutation that stopped resizes reaching the pty
        // left `less` with nothing to redraw, and a blocking read turned that into a ten-minute hang
        Err(error) => panic!("no event arrived within the socket's timeout ({error})"),
        Ok(_) => serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}")),
    }
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
    assert_eq!(
        next(&mut reading)["kind"],
        "hello",
        "the handshake is answered before anything else"
    );

    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
    let spawned = next(&mut reading);
    assert_eq!(spawned["kind"], "spawned");
    let session = spawned["session"]
        .as_str()
        .expect("a session id")
        .to_owned();
    assert!(
        spawned["pid"].as_u64().expect("a pid") > 0,
        "the broker owns a real process"
    );

    // The keystroke, and the output it causes
    ask(
        &mut writing,
        &format!(r#"{{"kind":"write","session":"{session}","data":"printf skeleton-probe\n"}}"#),
    );
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
    assert!(
        output.contains("skeleton-probe"),
        "the pty's output came back: {output:?}"
    );

    // A resize is accepted; a pty has one size, so there is nothing to answer
    ask(
        &mut writing,
        &format!(r#"{{"kind":"resize","session":"{session}","cols":100,"rows":30}}"#),
    );

    ask(
        &mut writing,
        &format!(r#"{{"kind":"kill","session":"{session}"}}"#),
    );
    let (exited, _) = until(&mut reading, "exited");
    assert_eq!(
        exited["session"].as_str(),
        Some(session.as_str()),
        "one exit, reported once"
    );
}

#[test]
fn a_request_for_a_session_that_does_not_exist_is_refused_by_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(
        &mut writing,
        r#"{"kind":"write","session":"s-404","data":"x"}"#,
    );
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
    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
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
    assert!(
        failed["message"]
            .as_str()
            .unwrap_or_default()
            .contains("999"),
        "{failed}"
    );
}

#[test]
fn nothing_can_be_asked_before_the_handshake() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);

    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
    assert_eq!(next(&mut reading)["code"], "HANDSHAKE");
}

#[test]
fn a_shell_that_exits_on_its_own_is_reported_without_anything_polling_for_it() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session")
        .to_owned();

    ask(
        &mut writing,
        &format!(r#"{{"kind":"write","session":"{session}","data":"exit\n"}}"#),
    );
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
        ask(
            &mut writing,
            r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
        );
        let spawned = next(&mut reading);
        spawned["pid"].as_u64().expect("a pid") as i32
        // The connection drops here, which is what an app that quit, crashed or was killed looks like
    };

    // The app cannot end its sessions on the way out — a pack's `onShutdown` is synchronous and runs in
    // `process.on('exit')`, where no socket write completes — so the broker has to be the backstop.
    assert!(
        reaped(pid),
        "the shell for pid {pid} was still running after its client disconnected"
    );
}

/// Whether `pid` is still a live process. Signal 0 asks without touching it.
fn alive(pid: i32) -> bool {
    let answer = unsafe { libc::kill(pid, 0) };
    answer == 0
}

/// Waits up to ~5s for `pid` to go away, answering whether it did.
fn reaped(pid: i32) -> bool {
    (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(25));
        !alive(pid)
    })
}

#[test]
fn killing_a_session_takes_the_whole_tree_with_it() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);
    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session")
        .to_owned();

    // A grandchild that outlives its parent unless something owns the tree: this is X1 and X2's subject,
    // and Phase B's acceptance says portable-pty is validated for containment rather than assumed.
    ask(
        &mut writing,
        &format!(r#"{{"kind":"write","session":"{session}","data":"sleep 300 & echo $!\n"}}"#),
    );
    let mut seen = String::new();
    let grandchild = (0..400)
        .find_map(|_| {
            let event = next(&mut reading);
            if event["kind"] != "output" {
                return None;
            }
            seen.push_str(event["data"].as_str().unwrap_or_default());
            // The shell echoes the job's pid on its own line, after the command it echoed back
            seen.lines()
                .filter_map(|line| line.trim().parse::<i32>().ok())
                .find(|pid| *pid > 1)
        })
        .unwrap_or_else(|| {
            panic!("the shell never reported its background job's pid; saw {seen:?}")
        });
    assert!(alive(grandchild), "the grandchild {grandchild} is running");

    ask(
        &mut writing,
        &format!(r#"{{"kind":"kill","session":"{session}"}}"#),
    );
    until(&mut reading, "exited");

    assert!(
        reaped(grandchild),
        "the grandchild {grandchild} outlived the session it belonged to"
    );
}

#[test]
fn a_session_is_cancellable_while_its_shell_is_busy() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);
    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
    let spawned = next(&mut reading);
    let session = spawned["session"].as_str().expect("a session").to_owned();
    let shell = spawned["pid"].as_u64().expect("a pid") as i32;

    // A shell that ignores interruption and never reads its input again: cancellation must not depend on
    // the process cooperating, which is what Phase B means by validating it rather than assuming it
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":"trap '' INT TERM; while true; do sleep 1; done\n"}}"#
        ),
    );
    std::thread::sleep(std::time::Duration::from_millis(400));

    ask(
        &mut writing,
        &format!(r#"{{"kind":"kill","session":"{session}"}}"#),
    );
    assert!(
        reaped(shell),
        "the shell {shell} survived a kill while it was busy and trapping signals"
    );
}

#[test]
fn an_alternate_screen_app_survives_a_resize_and_keeps_answering() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);
    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":80,"rows":24,"shell":null,"cwd":null}"#,
    );
    let spawned = next(&mut reading);
    let session = spawned["session"].as_str().expect("a session").to_owned();
    let shell = spawned["pid"].as_u64().expect("a pid") as i32;

    // `less` on the alternate screen: Phase B names this as where resize mistakes surface, because an app
    // that owns the whole screen redraws it on SIGWINCH rather than carrying on at a prompt. `vim` is the
    // other case and needs an installed vim; `less` ships with the OS.
    ask(
        &mut writing,
        &format!(r#"{{"kind":"write","session":"{session}","data":"seq 1 500 | less\n"}}"#),
    );
    let entered = (0..600).any(|_| {
        let event = next(&mut reading);
        // `\x1b[?1049h` is the alternate screen; `\x1b[?47h` is the older form some builds use
        event["kind"] == "output"
            && event["data"]
                .as_str()
                .is_some_and(|data| data.contains("[?1049h") || data.contains("[?47h"))
    });
    assert!(entered, "less never switched to the alternate screen");

    // Resize while it owns the screen. A pty has one size, so this is one message, and the app must redraw
    for (cols, rows) in [(100u16, 30u16), (60, 20), (120, 40)] {
        ask(
            &mut writing,
            &format!(r#"{{"kind":"resize","session":"{session}","cols":{cols},"rows":{rows}}}"#),
        );
    }

    // It redrew rather than dying: output arrives after the resizes, and the shell is still there
    let redrew = (0..600).any(|_| next(&mut reading)["kind"] == "output");
    assert!(redrew, "nothing was drawn after the resize");
    assert!(
        alive(shell),
        "the shell did not survive a resize under an alternate-screen app"
    );

    // And it is still taking input: `q` leaves less, which ends the shell's foreground job
    ask(
        &mut writing,
        &format!(r#"{{"kind":"write","session":"{session}","data":"q"}}"#),
    );
    let left = (0..600).any(|_| {
        let event = next(&mut reading);
        event["kind"] == "output"
            && event["data"]
                .as_str()
                .is_some_and(|data| data.contains("[?1049l") || data.contains("[?47l"))
    });
    assert!(
        left,
        "less never left the alternate screen, so it had stopped reading input"
    );
}

/// A session nobody is drawing still has a screen, which is the gain the whole refactor is for.
///
/// **No client holds any of this state.** The app asks for the screen after the output has arrived and the
/// broker answers from its own parse — so a window that never opened, or one that closed, changes nothing
/// about what a session knows about itself. Phase A's facts are read from this same screen.
#[test]
fn a_session_answers_a_bounded_screen_read() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":40,"rows":6,"shell":"/bin/sh","cwd":null}"#,
    );
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session id")
        .to_owned();

    // Enough lines to push some off a six-row viewport, so the history is doing work
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":"for i in 1 2 3 4 5 6 7 8 9 10; do printf 'line-%s\\n' $i; done\n"}}"#
        ),
    );
    let mut seen = String::new();
    for _ in 0..400 {
        let event = next(&mut reading);
        if event["kind"] == "output" {
            seen.push_str(event["data"].as_str().unwrap_or_default());
            if seen.contains("line-10") {
                break;
            }
        }
    }
    assert!(seen.contains("line-10"), "the output arrived: {seen:?}");

    // The viewport alone: six rows, and the last lines written are among them
    ask(
        &mut writing,
        &format!(r#"{{"kind":"screen","session":"{session}","history":0}}"#),
    );
    let (screen, _) = until(&mut reading, "screen");
    let viewport = screen["lines"].as_array().expect("lines").clone();
    assert_eq!(
        viewport.len(),
        6,
        "the viewport is the size the session was spawned with"
    );
    let text = |lines: &[serde_json::Value]| {
        lines
            .iter()
            .filter_map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        text(&viewport).contains("line-10"),
        "the viewport holds the last output: {:?}",
        text(&viewport)
    );
    assert_eq!(
        screen["alt_screen"], false,
        "a shell prompt is the primary screen"
    );

    // And with history, the lines that scrolled off — which only the broker still has
    ask(
        &mut writing,
        &format!(r#"{{"kind":"screen","session":"{session}","history":50}}"#),
    );
    let (with_history, _) = until(&mut reading, "screen");
    let all = with_history["lines"].as_array().expect("lines").clone();
    assert!(
        all.len() > viewport.len(),
        "history came with it: {} lines",
        all.len()
    );
    assert!(
        text(&all).contains("line-1\n"),
        "a line that scrolled off the viewport: {:?}",
        text(&all)
    );

    // A ceiling, not a promise: asking for more history than there is answers with what there is
    assert!(
        all.len() <= 6 + 50,
        "history is bounded by what was asked for"
    );

    ask(
        &mut writing,
        &format!(r#"{{"kind":"kill","session":"{session}"}}"#),
    );
    until(&mut reading, "exited");
}

/// The screen a session no longer has is a refusal by code, like every other request naming one.
#[test]
fn a_screen_read_for_a_session_that_does_not_exist_is_refused_by_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(
        &mut writing,
        r#"{"kind":"screen","session":"s-404","history":10}"#,
    );
    let failed = next(&mut reading);
    assert_eq!(failed["kind"], "failed");
    assert_eq!(failed["code"], "NO_SUCH_SESSION");
}
