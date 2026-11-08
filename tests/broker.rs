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
    let socket = socket_in(directory.path());
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

/// Where a broker listens, which is **a name rather than a path, and the spelling differs**.
///
/// `interprocess` maps a filesystem path to a Unix socket on POSIX; on Windows it refuses one, the local
/// namespace there being named pipes — *"expected a named pipe path"*, which is what the first Windows run
/// of this suite got instead of a broker. The caller chooses the spelling and the broker takes what it is
/// handed, so this is the only thing the two platforms disagree about.
#[cfg(unix)]
fn socket_in(directory: &std::path::Path) -> std::path::PathBuf {
    directory.join("broker.sock")
}

#[cfg(windows)]
fn socket_in(_directory: &std::path::Path) -> std::path::PathBuf {
    // One namespace for the whole machine, so the name carries this process and a counter rather than
    // leaning on the private directory a path would have given it
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::path::PathBuf::from(format!(
        r"\\.\pipe\apack-terminal-test-{}-{nth}",
        std::process::id()
    ))
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
fn connect(broker: &Broker) -> (Events, Wire) {
    let name = broker
        .socket
        .to_str()
        .expect("a utf-8 socket path")
        .to_fs_name::<GenericFilePath>()
        .expect("a socket name");
    let stream = Stream::connect(name).expect("the broker accepts");
    let wire = Wire(std::sync::Arc::new(stream));
    (Events::reading(wire.clone()), wire)
}

/// A connection's events, read on a thread of their own so that **every wait is bounded**.
///
/// A case waiting for something that will never come has to fail rather than hang — a mutation that stopped
/// resizes reaching the pty turned a blocking read into a ten-minute wait, which is why this file bounds
/// them at all. It used to ask the socket, with `set_recv_timeout`; `interprocess` refuses that for a
/// Windows named pipe, and the first Windows run of this suite hung for want of it.
///
/// So the bound moved off the socket: a reader thread and a channel do it the same way on both platforms,
/// which is one mechanism rather than one per platform and a guard that cannot quietly not apply.
struct Events(std::sync::mpsc::Receiver<String>);

impl Events {
    fn reading(wire: Wire) -> Self {
        let (lines, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(wire).lines() {
                // A send that fails means the case has finished with this connection
                if line.is_err() || lines.send(line.unwrap_or_default()).is_err() {
                    return;
                }
            }
        });
        Self(events)
    }
}

/// Ends a connection, the way a client process going away ends one.
///
/// **Dropping the handles is not enough, and the case that relied on it passed by luck.** `Events` moved a
/// clone of the socket into its reader thread, which is blocked in `read` and so cannot notice that its
/// channel is gone — it holds the socket open until something wakes it, which is pending output and
/// nothing else. A case that drained the output first then dropped would wait for ever for a shell the
/// broker never ended, and read as the lifetime rule being wrong.
///
/// So this wakes it on purpose: the receiver goes first, then one malformed frame the broker is certain to
/// answer. The reader thread takes that answer, fails to send it, returns, and drops the last reference —
/// which is when the socket actually closes and the broker sees the client leave.
fn disconnect(reading: Events, mut writing: Wire) {
    drop(reading);
    let _ = writing.write_all(b"not a frame\n");
    let _ = writing.flush();
    drop(writing);
    // The broker's own side of the close is what the caller is about to assert on
    std::thread::sleep(std::time::Duration::from_millis(100));
}

/// A spawn frame: the default shell at a given size, with no launch arguments and nothing added to the
/// environment. **One builder**, so a field added to `Request::Spawn` is one edit here and not ten.
fn spawn_frame(cols: u16, rows: u16, shell: Option<&str>) -> String {
    serde_json::json!({
        "kind": "spawn",
        "cols": cols,
        "rows": rows,
        "shell": shell,
        "args": [],
        "env": [],
        "cwd": null,
    })
    .to_string()
}

fn ask(writing: &mut impl Write, frame: &str) {
    writing
        .write_all(frame.as_bytes())
        .expect("the request is written");
    writing.write_all(b"\n").expect("the frame ends");
    writing.flush().expect("the request is flushed");
}

/// One event, or a failure saying none came. Bounded by the reader thread's channel.
fn next(reading: &mut Events) -> serde_json::Value {
    match reading.0.recv_timeout(std::time::Duration::from_secs(10)) {
        // A guard that hangs is worse than one that fails: a mutation that stopped resizes reaching the pty
        // left `less` with nothing to redraw, and a blocking read turned that into a ten-minute hang
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("no event arrived within 10s"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the broker closed the connection")
        }
        Ok(line) => serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}")),
    }
}

/// Reads until an event of `kind` arrives, returning it with everything seen on the way.
fn until(reading: &mut Events, kind: &str) -> (serde_json::Value, Vec<serde_json::Value>) {
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

    ask(&mut writing, &spawn_frame(80, 24, None));
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
    once_the_shell_is_listening(&mut reading);
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":{}}}"#,
            serde_json::to_string(&format!("echo skeleton-probe{SUBMIT}")).expect("a JSON string")
        ),
    );
    let mut output = String::new();
    for _ in 0..200 {
        let event = next(&mut reading);
        if event["kind"] == "output" {
            output.push_str(event["data"].as_str().unwrap_or_default());
            if output.matches("skeleton-probe").count() >= 2 {
                break;
            }
        }
    }
    // **Twice**: once as the console echoes what was typed, once as the command's own output. Asserting it
    // appears at all is a case a Windows console passes without running anything, which is how `\n` looked
    // like working input (X6)
    assert!(
        output.matches("skeleton-probe").count() >= 2,
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
    ask(&mut writing, &spawn_frame(80, 24, None));
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

    ask(&mut writing, &spawn_frame(80, 24, None));
    assert_eq!(next(&mut reading)["code"], "HANDSHAKE");
}

#[test]
fn a_shell_that_exits_on_its_own_is_reported_without_anything_polling_for_it() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session")
        .to_owned();

    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":{}}}"#,
            serde_json::to_string(&format!("exit{SUBMIT}")).expect("a JSON string")
        ),
    );
    let (exited, _) = until(&mut reading, "exited");
    assert_eq!(exited["session"].as_str(), Some(session.as_str()));
}

#[test]
fn a_client_that_disconnects_leaves_no_shell_running() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let pid = next(&mut reading)["pid"].as_u64().expect("a pid") as i32;
    // What an app that quit, crashed or was killed looks like from here
    disconnect(reading, writing);

    // The app cannot end its sessions on the way out — a pack's `onShutdown` is synchronous and runs in
    // `process.on('exit')`, where no socket write completes — so the broker has to be the backstop.
    assert!(
        reaped(pid),
        "the shell for pid {pid} was still running after its client disconnected"
    );
}

/// Whether `pid` is still a live process.
///
/// **Two questions with one name, because the platforms answer it differently.** POSIX has signal 0, which
/// asks without touching the process. Windows has no signal at all, and `OpenProcess` would be a dependency
/// for one call, so the OS's own list is asked instead — `tasklist` is on every Windows Server image and its
/// filter answers with the row or with "no tasks".
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn alive(pid: i32) -> bool {
    let listed = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output();
    match listed {
        // A filter that matches nothing prints "INFO: No tasks are running..." rather than failing, so the
        // pid's own digits are what says it is there
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")),
        Err(_) => false,
    }
}

/// The shell this platform's default session runs, where a case needs to name one.
///
/// `portable-pty`'s default program is the login shell on POSIX and `ComSpec` on Windows — which is
/// `cmd.exe`, not PowerShell, and typing one's syntax into the other is how a case can look like a broken
/// pty when it is only a bad command.
#[cfg(unix)]
const DEFAULT_SHELL: &str = "/bin/sh";
#[cfg(windows)]
const DEFAULT_SHELL: &str = "cmd.exe";

/// **What submits a line, which is not the same two bytes on both platforms** — X6's subject, measured
/// rather than assumed.
///
/// Typed into a Windows console: `\n` is echoed and never runs, `\r` alone does not get that far, and
/// `\r\n` both echoes and runs. So the app's run-script path sending `\n` leaves the command on the screen
/// looking typed and never executed, which is exactly what the finding describes.
///
/// It is the control characters themselves, so anything embedding them in a frame serialises the frame
/// rather than pasting into one — a raw newline inside a JSON string is not JSON.
#[cfg(unix)]
const SUBMIT: &str = "\n";
#[cfg(windows)]
const SUBMIT: &str = "\r\n";

/// Ten numbered lines, which a case counts rather than reads.
#[cfg(unix)]
fn ten_lines() -> String {
    format!("for i in 1 2 3 4 5 6 7 8 9 10; do printf 'line-%s\\n' $i; done{SUBMIT}")
}
#[cfg(windows)]
fn ten_lines() -> String {
    format!("for /L %i in (1,1,10) do @echo line-%i{SUBMIT}")
}

/// Waits until the shell has said something, which is the only sign a pty gives that it is listening.
///
/// **Input sent before that is not kept on every platform.** A POSIX pty's line discipline holds what was
/// typed until the shell reads it, so a case could write immediately and be right by accident; ConPTY
/// discards it, and PowerShell takes seconds to reach a prompt. The first Windows run of the tree-kill case
/// failed here — its command never ran at all.
fn once_the_shell_is_listening(reading: &mut Events) {
    for _ in 0..400 {
        if next(reading)["kind"] == "output" {
            return;
        }
    }
    panic!("the shell said nothing, so nothing could be typed into it");
}

/// What to type to start a process that outlives the shell, and where it writes its pid.
///
/// **The pid comes from a file rather than from the screen.** Reading it out of the session's output means
/// parsing what a shell echoed back through a pty — a prompt, the command coming back, and on Windows a
/// stream of cursor positioning around both — which is a different problem on each platform and none of it
/// the subject here. A file is the same on both.
#[cfg(unix)]
fn long_lived_grandchild(pid_file: &std::path::Path) -> String {
    format!("sleep 300 & echo $! > {}\n", pid_file.display())
}

#[cfg(windows)]
fn long_lived_grandchild(pid_file: &std::path::Path) -> String {
    // **A file to run, not a line to type, and `cmd` is what runs it.** `portable-pty`'s default program on
    // Windows is `ComSpec`, so a session spawned with no shell is `cmd.exe` — PowerShell syntax typed into
    // it is just a bad command. One short `powershell -File` line is what cmd understands, and it keeps the
    // quoting out of the pty, which the VM guide recommends for the same reason.
    let script = pid_file.with_extension("ps1");
    std::fs::write(
        &script,
        format!(
            "$p = Start-Process powershell -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 300' -PassThru -WindowStyle Hidden\r\nSet-Content -Path '{}' -Value $p.Id\r\n",
            pid_file.display()
        ),
    )
    .expect("a script for the grandchild");
    format!(
        "powershell -NoProfile -File \"{}\"{SUBMIT}",
        script.display()
    )
}

/// The pid that process wrote, waiting up to ~10s for the shell to get that far.
fn pid_written_to(pid_file: &std::path::Path) -> i32 {
    for _ in 0..400 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        let Ok(text) = std::fs::read_to_string(pid_file) else {
            continue;
        };
        if let Ok(pid) = text.trim().parse::<i32>() {
            return pid;
        }
    }
    panic!(
        "the shell never wrote its background job's pid to {}",
        pid_file.display()
    );
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
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session")
        .to_owned();

    // A grandchild that outlives its parent unless something owns the tree: this is X1 and X2's subject,
    // and Phase B's acceptance says portable-pty is validated for containment rather than assumed.
    once_the_shell_is_listening(&mut reading);
    let notes = tempfile::tempdir().expect("a directory for the grandchild's pid");
    let pid_file = notes.path().join("grandchild.pid");
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":{}}}"#,
            serde_json::to_string(&long_lived_grandchild(&pid_file)).expect("a JSON string")
        ),
    );
    let grandchild = pid_written_to(&pid_file);
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
    ask(&mut writing, &spawn_frame(80, 24, None));
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

/// **POSIX only, and `tests/parser.rs` is why that is not a gap.** An alternate-screen app has to be one the
/// OS ships — this drives `less` for that reason — and Windows ships no pager that uses the alternate
/// screen, so there is nothing here to drive. What the case is *about* is the VT core's handling of a resize
/// under the alternate screen, which `an_alternate_screen_survives_a_resize` covers as a parser, on every
/// platform and without a shell.
#[cfg(unix)]
#[test]
fn an_alternate_screen_app_survives_a_resize_and_keeps_answering() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
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

    ask(&mut writing, &spawn_frame(40, 6, Some(DEFAULT_SHELL)));
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session id")
        .to_owned();

    // Enough lines to push some off a six-row viewport, so the history is doing work
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":{}}}"#,
            serde_json::to_string(&ten_lines()).expect("a JSON string")
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

/// Writing to and resizing a session whose shell has just exited is refused, not a crash — which is **X9**.
///
/// ConPTY throws for about a second after a console's process ends, where a POSIX pty write to a closed
/// master is an ordinary `EIO`. Either way the session is gone and the answer the app needs is the same one:
/// a refusal naming the session, in time for the window that asked to be told. What this pins is that the
/// broker produces one rather than dying, on whichever platform is running it.
#[test]
fn writing_to_a_session_whose_shell_has_gone_is_refused() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session")
        .to_owned();

    once_the_shell_is_listening(&mut reading);
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":{}}}"#,
            serde_json::to_string(&format!("exit{SUBMIT}")).expect("a JSON string")
        ),
    );
    until(&mut reading, "exited");

    // Straight after the exit, which is the window ConPTY is documented to be unhappy in
    ask(
        &mut writing,
        &format!(r#"{{"kind":"write","session":"{session}","data":"x"}}"#),
    );
    let refused = until(&mut reading, "failed").0;
    assert_eq!(refused["code"], "NO_SUCH_SESSION");

    ask(
        &mut writing,
        &format!(r#"{{"kind":"resize","session":"{session}","cols":100,"rows":30}}"#),
    );
    let refused = until(&mut reading, "failed").0;
    assert_eq!(refused["code"], "NO_SUCH_SESSION");

    // And the broker is still there to answer the next thing asked of it
    ask(
        &mut writing,
        r#"{"kind":"screen","session":"s-404","history":0}"#,
    );
    assert_eq!(until(&mut reading, "failed").0["code"], "NO_SUCH_SESSION");
}

/// A shell is started with the arguments and environment it was asked for — which is what shell integration
/// is arranged out of (D3).
///
/// The markers a terminal reads facts from come from code the shell runs at startup, and every shell takes
/// that a different way: bash wants `--init-file` and PowerShell `-NoExit`, which are arguments; zsh wants
/// `ZDOTDIR` and fish `XDG_DATA_DIRS`, which are environment. The broker chooses neither — what each shell
/// needs is the app's to know, because the rc files it points at are the app's to ship — so what this pins
/// is only that both arrive.
#[cfg(unix)]
#[test]
fn a_shell_is_started_with_the_arguments_and_environment_it_was_asked_for() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    next(&mut reading);

    // `-c` runs a command and exits, which is an argument doing something observable; the variable it
    // prints is the other half
    let frame = serde_json::json!({
        "kind": "spawn",
        "cols": 80,
        "rows": 24,
        "shell": "/bin/sh",
        "args": ["-c", "printf 'marker=%s\\n' \"$APACK_PROBE\""],
        "env": [["APACK_PROBE", "arranged"]],
        "cwd": null,
    })
    .to_string();
    ask(&mut writing, &frame);
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session")
        .to_owned();

    let mut seen = String::new();
    for _ in 0..200 {
        let event = next(&mut reading);
        if event["kind"] == "output" && event["session"] == session.as_str() {
            seen.push_str(event["data"].as_str().unwrap_or_default());
        }
        if seen.contains("marker=") {
            break;
        }
    }

    assert!(
        seen.contains("marker=arranged"),
        "the argument ran and the variable reached it: {seen:?}"
    );
}

/// A command that makes the shell *print* an escape sequence, so the marker reaches the screen the way a
/// shell with integration arranged emits it — and **does not appear in the echo of what was typed**, which
/// is what makes these cases about the shell's report rather than about the console's echo. The typed text
/// carries `\033`, three ordinary characters; only `printf`'s output carries the escape byte.
///
/// **POSIX only, deliberately.** `cmd.exe` has no `printf` and no way to write an escape byte that is not
/// also echoed, so the Windows form of this case would assert the echo. PowerShell can, and arranging these
/// markers there is X4 — which this goal admits only on the strength of a Windows run.
#[cfg(unix)]
fn prints(sequences: &str) -> String {
    // `READ%s` in the typed text, `READY` in the output: a sentinel a case can wait for without matching
    // the echo of the line that produces it
    format!("printf '{sequences}READ%s\\n' Y{SUBMIT}")
}

/// Reads until the shell has printed the sentinel. **Seeing it means every marker before it is parsed**,
/// because the screen is fed before the bytes leave for the client — the one direction in which the
/// authority can never be behind.
#[cfg(unix)]
fn once_the_sentinel_is_printed(reading: &mut Events) {
    let mut output = String::new();
    for _ in 0..400 {
        let event = next(reading);
        if event["kind"] == "output" {
            output.push_str(event["data"].as_str().unwrap_or_default());
            if output.contains("READY") {
                return;
            }
        }
    }
    panic!("the sentinel never arrived: {output:?}");
}

/// A session with a shell running, ready to be typed into.
#[cfg(unix)]
fn a_session(broker: &Broker) -> (Events, Wire, String) {
    let (mut reading, mut writing) = connect(broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    assert_eq!(next(&mut reading)["kind"], "hello");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let session = next(&mut reading)["session"]
        .as_str()
        .expect("a session id")
        .to_owned();
    once_the_shell_is_listening(&mut reading);
    (reading, writing, session)
}

#[cfg(unix)]
fn types(writing: &mut impl Write, session: &str, data: &str) {
    ask(
        writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","data":{}}}"#,
            serde_json::to_string(data).expect("a JSON string")
        ),
    );
}

/// **The read Phase A exists for: a session nobody is drawing answers what its shell reported.**
///
/// No window, no attachment, no view — the facts come from the broker's own parse of the markers, which is
/// the whole gain of the VT core living here. Every field is a claim the shell made, and the case asserts
/// they are reported as made: a status of 3 is 3, not "it failed".
#[cfg(unix)]
#[test]
fn a_session_nobody_is_drawing_answers_what_its_shell_reported() {
    let broker = start();
    let (mut reading, mut writing, session) = a_session(&broker);

    types(
        &mut writing,
        &session,
        &prints("\\033]633;E;ls -la\\007\\033]633;C\\007\\033]633;P;Cwd=/tmp\\007\\033]633;D;3\\007"),
    );
    once_the_sentinel_is_printed(&mut reading);

    ask(&mut writing, &format!(r#"{{"kind":"facts","session":"{session}"}}"#));
    let (facts, _) = until(&mut reading, "facts");
    assert_eq!(facts["cwd"], "/tmp", "the directory the shell reported");
    assert_eq!(facts["command"], "ls -la", "the command line it reported");
    assert_eq!(facts["exit_status"], 3, "the status it reported, as reported");
    assert_eq!(facts["running"], false, "the command finished");
}

/// A bounded search over a hidden session's history, which is the other read without a view.
#[cfg(unix)]
#[test]
fn a_search_finds_text_in_a_hidden_sessions_history() {
    let broker = start();
    let (mut reading, mut writing, session) = a_session(&broker);

    types(&mut writing, &session, &ten_lines());
    types(&mut writing, &session, &prints(""));
    once_the_sentinel_is_printed(&mut reading);

    ask(
        &mut writing,
        &format!(r#"{{"kind":"search","session":"{session}","text":"line-7","history":1000}}"#),
    );
    let (found, _) = until(&mut reading, "search");
    let matches = found["matches"].as_array().expect("matches");
    assert!(
        matches.iter().any(|m| m["text"] == "line-7"),
        "the line it was asked for: {matches:?}"
    );
    // The whole line comes back, so a caller sees its context without a second read
    assert!(
        matches[0]["column"].as_u64().is_some(),
        "with where in the line it starts"
    );

    // Text nothing wrote is not found, which is the half that fails if a search answers everything
    ask(
        &mut writing,
        &format!(r#"{{"kind":"search","session":"{session}","text":"line-11","history":1000}}"#),
    );
    let (none, _) = until(&mut reading, "search");
    assert_eq!(
        none["matches"].as_array().expect("matches").len(),
        0,
        "nothing wrote it"
    );
}

/// **A wait is answered when the state arrives, not on a timer.**
///
/// The shell reports a command starting and the session is busy; the wait registered then must not answer.
/// It answers when the shell reports the command finished — from the bytes arriving, which is the whole
/// point of checking the condition where they land.
#[cfg(unix)]
#[test]
fn a_wait_for_idle_is_answered_when_the_command_finishes() {
    let broker = start();
    let (mut reading, mut writing, session) = a_session(&broker);

    // Busy: a command started and nothing has finished it
    types(&mut writing, &session, &prints("\\033]633;C\\007"));
    once_the_sentinel_is_printed(&mut reading);

    ask(
        &mut writing,
        &format!(r#"{{"kind":"wait","session":"{session}","until":{{"kind":"idle"}},"timeout_ms":10000}}"#),
    );
    // Nothing has finished the command, so nothing may answer yet
    for _ in 0..3 {
        if let Ok(line) = reading.0.recv_timeout(std::time::Duration::from_millis(300)) {
            let event: serde_json::Value = serde_json::from_str(&line).expect("an event");
            assert_ne!(event["kind"], "waited", "the session is still busy: {event}");
        }
    }

    types(&mut writing, &session, &prints("\\033]633;D;0\\007"));
    let (waited, _) = until(&mut reading, "waited");
    assert_eq!(waited["outcome"], "MET", "the shell reported it finished");
}

/// A wait for something that never comes answers its deadline rather than lasting for ever.
#[cfg(unix)]
#[test]
fn a_wait_that_is_never_met_answers_its_deadline() {
    let broker = start();
    let (mut reading, mut writing, session) = a_session(&broker);

    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"wait","session":"{session}","until":{{"kind":"text","text":"nothing writes this"}},"timeout_ms":400}}"#
        ),
    );
    let (waited, _) = until(&mut reading, "waited");
    assert_eq!(waited["outcome"], "TIMEOUT");
}

/// **A session ending settles every wait on it, and each is answered exactly once.**
///
/// An `exit` wait has reached what it asked for; an `idle` wait has not and never will, which is neither
/// `MET` nor a deadline that has not passed. Without this a caller waiting on a shell the user just exited
/// waits out its whole timeout.
#[cfg(unix)]
#[test]
fn a_session_ending_settles_both_kinds_of_wait() {
    let broker = start();
    let (mut reading, mut writing, session) = a_session(&broker);

    // Busy, so the idle wait cannot be answered by the state it is in
    types(&mut writing, &session, &prints("\\033]633;C\\007"));
    once_the_sentinel_is_printed(&mut reading);

    ask(
        &mut writing,
        &format!(r#"{{"kind":"wait","session":"{session}","until":{{"kind":"idle"}},"timeout_ms":30000}}"#),
    );
    ask(
        &mut writing,
        &format!(r#"{{"kind":"wait","session":"{session}","until":{{"kind":"exit"}},"timeout_ms":30000}}"#),
    );
    types(&mut writing, &session, &format!("exit{SUBMIT}"));

    let mut outcomes = Vec::new();
    for _ in 0..400 {
        let event = next(&mut reading);
        if event["kind"] == "waited" {
            outcomes.push(event["outcome"].as_str().unwrap_or_default().to_owned());
        }
        if outcomes.len() == 2 {
            break;
        }
    }
    outcomes.sort();
    assert_eq!(
        outcomes,
        vec!["EXITED".to_string(), "MET".to_string()],
        "the exit wait is met and the idle wait is told the session ended"
    );
}

/// Greets as a given role, answering the connection and its id.
fn greet(broker: &Broker, role: &str) -> (Events, Wire) {
    let (mut reading, mut writing) = connect(broker);
    ask(&mut writing, &format!(r#"{{"kind":"hello","protocol":1,"role":"{role}"}}"#));
    assert_eq!(next(&mut reading)["kind"], "hello", "greeted as {role}");
    (reading, writing)
}

/// **A session outlives the client that opened it, while an owner holds the broker.**
///
/// This is what the whole lifetime rule is for: restarting the service that opened a session must not end
/// it. The client here goes away entirely — its socket is dropped — and the shell is still there to answer
/// a second client afterwards.
#[cfg(unix)]
#[test]
fn a_session_outlives_its_client_while_an_owner_holds_the_broker() {
    let broker = start();
    let (_owner_events, _owner) = greet(&broker, "lifetime");

    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let spawned = next(&mut reading);
    let session = spawned["session"].as_str().expect("a session id").to_owned();
    let pid = spawned["pid"].as_u64().expect("a pid") as i32;
    once_the_shell_is_listening(&mut reading);

    // The client is gone, socket and all
    disconnect(reading, writing);
    assert!(alive(pid), "the shell outlives the client that asked for it");

    // And a client that comes back can still use it, which is what "still running" has to mean
    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &format!(r#"{{"kind":"facts","session":"{session}"}}"#));
    let (facts, _) = until(&mut reading, "facts");
    assert_eq!(facts["session"], session.as_str(), "the same session answers the new client");
}

/// **The owner going away ends them**, which is what stops a quit leaving shells behind.
#[cfg(unix)]
#[test]
fn the_owner_leaving_ends_every_session() {
    let broker = start();
    let (owner_events, owner) = greet(&broker, "lifetime");

    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let pid = next(&mut reading)["pid"].as_u64().expect("a pid") as i32;
    once_the_shell_is_listening(&mut reading);
    assert!(alive(pid));

    disconnect(owner_events, owner);
    for _ in 0..40 {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the shell outlived the owner of the broker");
}

/// **A client reconnecting receives the feed again**, which is the other half of surviving a restart: the
/// session is useless if its output still goes to a socket nobody holds.
#[cfg(unix)]
#[test]
fn output_follows_the_client_that_came_back() {
    let broker = start();
    let (_owner_events, _owner) = greet(&broker, "lifetime");

    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let session = next(&mut reading)["session"].as_str().expect("a session id").to_owned();
    once_the_shell_is_listening(&mut reading);
    disconnect(reading, writing);

    let (mut reading, mut writing) = greet(&broker, "client");
    types(&mut writing, &session, &format!("echo back-on-the-feed{SUBMIT}"));
    let mut output = String::new();
    for _ in 0..400 {
        let event = next(&mut reading);
        if event["kind"] == "output" {
            output.push_str(event["data"].as_str().unwrap_or_default());
            if output.contains("back-on-the-feed") {
                return;
            }
        }
    }
    panic!("the new client never received the session's output: {output:?}");
}

/// A caller that names no role is a client, so nothing that spoke the old handshake changes behaviour.
#[cfg(unix)]
#[test]
fn a_connection_that_names_no_role_is_a_client() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, r#"{"kind":"hello","protocol":1}"#);
    assert_eq!(next(&mut reading)["kind"], "hello");

    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let pid = next(&mut reading)["pid"].as_u64().expect("a pid") as i32;
    once_the_shell_is_listening(&mut reading);

    // No owner, so the old rule stands: this client leaving ends it
    disconnect(reading, writing);
    for _ in 0..40 {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("a client with no owner left its shell running");
}

/// **An owner asks for nothing and is sent nothing**, which is what makes holding the broker's lifetime
/// free. The order here is the one that matters: a client is already attached when the owner arrives, so an
/// owner that installed itself as the feed would take the output away from a terminal someone is using. The
/// cheap reading — owner first, client second — hides it, because the client's own greeting takes the feed
/// back.
#[cfg(unix)]
#[test]
fn an_owner_arriving_does_not_take_the_feed_from_a_client() {
    let broker = start();
    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let session = next(&mut reading)["session"].as_str().expect("a session id").to_owned();
    once_the_shell_is_listening(&mut reading);

    // The app takes its hold *after* the terminal is already running
    let (_owner_events, _owner) = greet(&broker, "lifetime");

    types(&mut writing, &session, &format!("echo still-mine{SUBMIT}"));
    let mut output = String::new();
    for _ in 0..400 {
        let event = next(&mut reading);
        if event["kind"] == "output" {
            output.push_str(event["data"].as_str().unwrap_or_default());
            if output.contains("still-mine") {
                return;
            }
        }
    }
    panic!("the owner took the feed from the client: {output:?}");
}
