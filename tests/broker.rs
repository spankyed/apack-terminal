//! The broker driven as the app will drive it: over a real socket, in the real binary.
//!
//! This is the walking skeleton's own half of Phase S — a keystroke reaching a pty and its output coming
//! back through the protocol — proven here before any of it is wired into the app, so that the protocol is
//! derived from a client that exists rather than proposed to one that does not.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use apack_terminal::protocol::PROTOCOL_VERSION;

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
    start_with_env(|_| {})
}

/// The same, with the broker's **own** environment arranged first.
///
/// For a case whose subject is what the broker passes on rather than what it happened to inherit. Without
/// it such a case cannot fail where it matters: a developer's shell exports `TERM`, so a test that asserts
/// a spawned shell has one passes whether the broker set it or the runner did — which is the same asymmetry
/// as a check that is green locally and red on a runner, read the other way round.
fn start_with_env(arrange: impl FnOnce(&mut Command)) -> Broker {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
    command.arg(&socket).stdout(Stdio::piped());
    // **A session of its own, so a signal from the broker cannot leave this test.** The broker under test
    // signals process groups, and without this it inherits the group of `cargo nextest`, which inherits the
    // developer's shell: a broker that aimed at its group — which a mutation of the aiming can make it do —
    // would reach the test runner and everything beside it. It bounds a group signal and nothing else;
    // `kill(-1)` is bounded by the type a target is built through (`SignalTarget`), which is where that
    // belongs.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: runs in the child between fork and exec, and `setsid` touches nothing this process owns
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
    }
    arrange(&mut command);
    let mut process = command.spawn().expect("the broker starts");

    let mut ready = String::new();
    BufReader::new(process.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut ready)
        .expect("the broker says where it is listening");
    assert!(ready.starts_with("listening "), "{ready:?}");

    Broker { process, socket, _directory: directory }
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
    std::path::PathBuf::from(format!(r"\\.\pipe\apack-terminal-test-{}-{nth}", std::process::id()))
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
    let name =
        broker.socket.to_str().expect("a utf-8 socket path").to_fs_name::<GenericFilePath>().expect("a socket name");
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
///
/// **Its receiver is private, and that is load-bearing rather than tidy.** Reaching it is how a case comes
/// to match a pattern against one `output` event, which the protocol promises nothing about: an escape
/// sequence splits across two whenever a flush falls inside it (`tests/coalescing.rs`). Everything here
/// reads through `output_until`, which accumulates first, or through `event`, which is for the one-answer
/// events — `spawned`, `exited`, `facts` — where a single frame *is* the unit.
struct Events(std::sync::mpsc::Receiver<String>);

impl Events {
    /// The next line, waiting at most `step`. The one accessor, so the receiver stays private.
    fn line(&mut self, step: std::time::Duration) -> Result<String, std::sync::mpsc::RecvTimeoutError> {
        self.0.recv_timeout(step)
    }
}

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

/// The greeting every client sends first, **built from the constant rather than written out**.
///
/// One place, for `spawn_frame`'s reason: a protocol bump is an edit here and not in twenty cases, none of
/// which is about the version. The one case that *is* about it names its own number on purpose.
fn hello(role: Option<&str>) -> String {
    match role {
        Some(role) => format!(r#"{{"kind":"hello","protocol":{PROTOCOL_VERSION},"role":"{role}"}}"#),
        None => format!(r#"{{"kind":"hello","protocol":{PROTOCOL_VERSION}}}"#),
    }
}

/// A spawn frame: the default shell at a given size, with no launch arguments and nothing added to the
/// environment. **One builder**, so a field added to `Request::Spawn` is one edit here and not ten.
fn spawn_frame(cols: u16, rows: u16, shell: Option<&str>) -> String {
    spawn_frame_with_env(cols, rows, shell, &[])
}

/// The same, with environment the shell needs. A case that drives a program which reads terminfo declares
/// `TERM` here rather than inheriting whatever ran it — see the alternate-screen case.
fn spawn_frame_with_env(cols: u16, rows: u16, shell: Option<&str>, env: &[(&str, &str)]) -> String {
    serde_json::json!({
        "kind": "spawn",
        "cols": cols,
        "rows": rows,
        "shell": shell,
        "args": [],
        "env": env.iter().map(|(name, value)| [*name, *value]).collect::<Vec<_>>(),
        "unset": [],
        "cwd": null,
    })
    .to_string()
}

fn ask(writing: &mut impl Write, frame: &str) {
    writing.write_all(frame.as_bytes()).expect("the request is written");
    writing.write_all(b"\n").expect("the frame ends");
    writing.flush().expect("the request is flushed");
}

/// How long the broker may say **nothing at all** before a read gives up.
///
/// **A window of silence, not a budget for a read.** Every event resets it, so a slow machine — a CI runner
/// with three cores and another case's pty on each — makes a read take longer and never makes it fail. What
/// spends this is a broker that has stopped answering, which is the thing worth failing over, and a guard
/// that hangs is worse than one that fails: a mutation that stopped resizes reaching the pty left `less`
/// with nothing to redraw, and a blocking read turned that into a ten-minute wait.
const QUIET: std::time::Duration = std::time::Duration::from_secs(10);

/// A command whose *output* is `token` while the line the shell echoes back is not.
///
/// **A case cannot count the echo.** The shell writes the typed line back at the terminal's width, so on a
/// host whose prompt is long — a CI runner's is its hostname — it wraps mid-word, and `still-attached`
/// arrives as `still-att \rached`, which no `matches` finds. Splitting the token with an empty-string quote
/// leaves the echo unmatched and the command's own output, on a line of its own, as the one occurrence a
/// case waits for. Counting both was the assumption, and it held only where the prompt was short.
///
/// It also keeps what the count was *for*: a case that matches the echo passes on a console that ran
/// nothing, which is how `\n` once looked like working input. Here the only thing that can produce the
/// token is the command.
///
/// **POSIX shells only** — the empty-string quote is how `sh`, `bash` and `zsh` read it, and `cmd.exe`
/// prints it back. The cross-platform case above counts occurrences for that reason.
fn probe(token: &str) -> String {
    let (first, rest) = token.split_at(1);
    format!("echo {first}''{rest}{SUBMIT}")
}

/// The next event, or `None` once the broker has said nothing for `QUIET`.
fn event(reading: &mut Events) -> Option<serde_json::Value> {
    match reading.line(QUIET) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the broker closed the connection")
        }
        Ok(line) => Some(serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))),
    }
}

/// One event, or a failure saying none came.
fn next(reading: &mut Events) -> serde_json::Value {
    event(reading).unwrap_or_else(|| panic!("no event arrived within {QUIET:?}"))
}

/// **A session's output, read until `enough` is true of all of it**, or until the broker goes quiet.
///
/// Accumulating before deciding is the point, and not a convenience. Output arrives in whatever sizes a
/// `read` returned, so an escape sequence is routinely delivered in pieces — which is the whole subject of
/// `tests/parser.rs`. A case that asked whether *one* event contained `\x1b[?1049h` was therefore asking
/// whether the sequence happened to arrive whole, and on a loaded machine, where reads fragment, it does
/// not: the case then drained the stream looking for a match that could no longer appear and died on the
/// silence window with a message about a clock rather than its own.
///
/// It answers with everything it read either way, so the caller's own assertion is what fails and says
/// what came instead.
fn output_until(reading: &mut Events, session: Option<&str>, enough: impl Fn(&str) -> bool) -> String {
    let mut seen = String::new();
    while let Some(event) = event(reading) {
        if event["kind"] != "output" {
            continue;
        }
        if session.is_some_and(|wanted| event["session"] != wanted) {
            continue;
        }
        seen.push_str(event["text"].as_str().unwrap_or_default());
        if enough(&seen) {
            break;
        }
    }
    seen
}

/// Reads until an event of `kind` arrives, returning it with everything seen on the way.
fn until(reading: &mut Events, kind: &str) -> (serde_json::Value, Vec<serde_json::Value>) {
    let mut seen = Vec::new();
    while let Some(event) = event(reading) {
        if event["kind"] == kind {
            return (event, seen);
        }
        seen.push(event);
    }
    panic!("no {kind} arrived; saw {} events: {seen:?}", seen.len());
}

#[test]
fn a_keystroke_reaches_a_pty_and_its_output_comes_back() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, &hello(None));
    assert_eq!(next(&mut reading)["kind"], "hello", "the handshake is answered before anything else");

    // **A named shell, because this case is about the round trip and not about shell discovery.** Spawning
    // the platform's default made it depend on whatever shell the machine's account points at and whatever
    // that shell does on a profile it has never seen — a first-run wizard swallows the keystroke, and the
    // case times out waiting for an echo that was never coming. It failed on macOS CI and on no developer's
    // machine. `a_shell_is_started_with_the_arguments_and_environment_it_was_asked_for` is the case whose
    // subject *is* the shell.
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let spawned = next(&mut reading);
    assert_eq!(spawned["kind"], "spawned");
    let session = spawned["session"].as_str().expect("a session id").to_owned();
    assert!(spawned["pid"].as_u64().expect("a pid") > 0, "the broker owns a real process");

    // The keystroke, and the output it causes
    once_the_shell_is_listening(&mut reading);
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&format!("echo skeleton-probe{SUBMIT}")).expect("a JSON string")
        ),
    );
    let output = output_until(&mut reading, None, |seen| seen.matches("skeleton-probe").count() >= 2);
    // **Twice**: once as the console echoes what was typed, once as the command's own output. Asserting it
    // appears at all is a case a Windows console passes without running anything, which is how `\n` looked
    // like working input (X6)
    assert!(output.matches("skeleton-probe").count() >= 2, "the pty's output came back: {output:?}");

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
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"write","session":"s-404","text":"x"}"#);
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
    ask(&mut writing, &hello(None));
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
    assert!(failed["message"].as_str().unwrap_or_default().contains("999"), "{failed}");
}

/// **The handshake answers the one scrollback number**, which is the broker's because the broker is what
/// has to answer for it: a consumer's emulator holds whatever it was sent while attached, and this holds
/// every session's whether or not anyone is drawing one. Two independent constants is what this replaced —
/// 10,000 in the window against 1,000 here, so closing and reopening a window silently lost nine tenths of
/// the scrollback. Asserted against `HISTORY_LINES` so the wire cannot drift from what the screen keeps.
#[test]
fn the_handshake_says_how_much_history_the_broker_keeps() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, &hello(None));
    let greeting = next(&mut reading);

    assert_eq!(greeting["kind"], "hello");
    assert_eq!(
        greeting["history"].as_u64(),
        Some(apack_terminal::screen::HISTORY_LINES as u64),
        "the handshake's history is not what the screen keeps: {greeting}"
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
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();

    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
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
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let pid = next(&mut reading)["pid"].as_u64().expect("a pid") as i32;
    // What an app that quit, crashed or was killed looks like from here
    disconnect(reading, writing);

    // The app cannot end its sessions on the way out — a pack's `onShutdown` is synchronous and runs in
    // `process.on('exit')`, where no socket write completes — so the broker has to be the backstop.
    assert!(reaped(pid), "the shell for pid {pid} was still running after its client disconnected");
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
    let listed =
        std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output();
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
    format!("powershell -NoProfile -File \"{}\"{SUBMIT}", script.display())
}

/// The pid that process wrote, **waited for by the session's own output** rather than by a clock.
///
/// The shell has to start, read a line and run it before that file exists, and on Windows that line starts
/// another PowerShell — work whose duration is the machine's to decide, not this case's. What says it is
/// still happening is output arriving on the session, so that is the bound: the file is checked often,
/// which costs nothing, and the wait gives up only after `QUIET` with no output *and* no file. A fixed ten
/// seconds fails correct code on a busy box, which is the whole defect this shape avoids. A shell that
/// talks forever without writing the file is ended by `.config/nextest.toml`'s deadline, as any test that
/// has stopped ending is.
fn pid_written_to(reading: &mut Events, pid_file: &std::path::Path) -> i32 {
    const STEP: std::time::Duration = std::time::Duration::from_millis(25);
    let mut silence = std::time::Duration::ZERO;
    loop {
        if let Ok(text) = std::fs::read_to_string(pid_file)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return pid;
        }
        match reading.line(STEP) {
            // The shell is still working, so the wait starts over: progress, not elapsed time
            Ok(_) => silence = std::time::Duration::ZERO,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                silence += STEP;
                assert!(
                    silence < QUIET,
                    "the shell said nothing for {QUIET:?} and never wrote its background job's pid to {}",
                    pid_file.display()
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the broker closed the connection before the pid arrived")
            }
        }
    }
}

/// Whether `pid` went away, within a grace of about two hundred probes.
///
/// **A kill grace, and deliberately not progress-bounded**, because there is no progress to read: a process
/// being torn down by the OS reports nothing, and the only observable is its absence. That is the one class
/// of wait this repository keeps on a count rather than converting, and saying so is the point — the
/// neighbouring `pid_written_to` waits on another process's *work*, which does have a signal, and is bound
/// by it.
///
/// What is counted is probes rather than seconds, which matters on Windows: `alive` there runs `tasklist`,
/// so each probe costs what the machine charges for one and the grace stretches with load by itself. On
/// POSIX the probe is a signal-0 and free, so this is about five seconds of them.
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
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();

    // A grandchild that outlives its parent unless something owns the tree: this is X1 and X2's subject,
    // and Phase B's acceptance says portable-pty is validated for containment rather than assumed.
    once_the_shell_is_listening(&mut reading);
    let notes = tempfile::tempdir().expect("a directory for the grandchild's pid");
    let pid_file = notes.path().join("grandchild.pid");
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&long_lived_grandchild(&pid_file)).expect("a JSON string")
        ),
    );
    let grandchild = pid_written_to(&mut reading, &pid_file);
    assert!(alive(grandchild), "the grandchild {grandchild} is running");

    ask(&mut writing, &format!(r#"{{"kind":"kill","session":"{session}"}}"#));
    until(&mut reading, "exited");

    assert!(reaped(grandchild), "the grandchild {grandchild} outlived the session it belonged to");
}

#[test]
fn a_session_is_cancellable_while_its_shell_is_busy() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
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
            r#"{{"kind":"write","session":"{session}","text":"trap '' INT TERM; while true; do sleep 1; done\n"}}"#
        ),
    );
    std::thread::sleep(std::time::Duration::from_millis(400));

    ask(&mut writing, &format!(r#"{{"kind":"kill","session":"{session}"}}"#));
    assert!(reaped(shell), "the shell {shell} survived a kill while it was busy and trapping signals");
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
    ask(&mut writing, &hello(None));
    next(&mut reading);
    // **`TERM` is declared rather than inherited, and that is the whole of what made this case
    // environment-dependent.** `less` reads terminfo to decide whether it may use the alternate screen at
    // all: with `TERM` unset or `dumb` it stays on the primary one and emits no `[?1049h`, so the case
    // times out waiting for a switch that was never going to happen. A developer's run inherited their
    // terminal's `TERM` and passed; CI has no terminal, and this failed on the first run there. The app
    // declares the same variable for the same reason (X13).
    ask(&mut writing, &spawn_frame_with_env(80, 24, Some(DEFAULT_SHELL), &[("TERM", "xterm-256color")]));
    let spawned = next(&mut reading);
    let session = spawned["session"].as_str().expect("a session").to_owned();
    let shell = spawned["pid"].as_u64().expect("a pid") as i32;

    // `less` on the alternate screen: Phase B names this as where resize mistakes surface, because an app
    // that owns the whole screen redraws it on SIGWINCH rather than carrying on at a prompt. `vim` is the
    // other case and needs an installed vim; `less` ships with the OS.
    ask(&mut writing, &format!(r#"{{"kind":"write","session":"{session}","text":"seq 1 500 | less\n"}}"#));
    // `\x1b[?1049h` is the alternate screen; `\x1b[?47h` is the older form some builds use
    let entered = output_until(&mut reading, None, |seen| seen.contains("[?1049h") || seen.contains("[?47h"));
    assert!(
        entered.contains("[?1049h") || entered.contains("[?47h"),
        "less never switched to the alternate screen: {entered:?}"
    );

    // Resize while it owns the screen. A pty has one size, so this is one message, and the app must redraw
    for (cols, rows) in [(100u16, 30u16), (60, 20), (120, 40)] {
        ask(&mut writing, &format!(r#"{{"kind":"resize","session":"{session}","cols":{cols},"rows":{rows}}}"#));
    }

    // It redrew rather than dying: output arrives after the resizes, and the shell is still there
    let redrew = output_until(&mut reading, None, |seen| !seen.is_empty());
    assert!(!redrew.is_empty(), "nothing was drawn after the resize");
    assert!(alive(shell), "the shell did not survive a resize under an alternate-screen app");

    // And it is still taking input: `q` leaves less, which ends the shell's foreground job
    ask(&mut writing, &format!(r#"{{"kind":"write","session":"{session}","text":"q"}}"#));
    let left = output_until(&mut reading, None, |seen| seen.contains("[?1049l") || seen.contains("[?47l"));
    assert!(
        left.contains("[?1049l") || left.contains("[?47l"),
        "less never left the alternate screen, so it had stopped reading input: {left:?}"
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
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, &spawn_frame(40, 6, Some(DEFAULT_SHELL)));
    let session = next(&mut reading)["session"].as_str().expect("a session id").to_owned();

    // Enough lines to push some off a six-row viewport, so the history is doing work
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&ten_lines()).expect("a JSON string")
        ),
    );
    let mut seen = String::new();
    for _ in 0..400 {
        let event = next(&mut reading);
        if event["kind"] == "output" {
            seen.push_str(event["text"].as_str().unwrap_or_default());
            if seen.contains("line-10") {
                break;
            }
        }
    }
    assert!(seen.contains("line-10"), "the output arrived: {seen:?}");

    // The viewport alone: six rows, and the last lines written are among them
    ask(&mut writing, &format!(r#"{{"kind":"screen","session":"{session}","history":0}}"#));
    let (screen, _) = until(&mut reading, "screen");
    let viewport = screen["lines"].as_array().expect("lines").clone();
    assert_eq!(viewport.len(), 6, "the viewport is the size the session was spawned with");
    let text =
        |lines: &[serde_json::Value]| lines.iter().filter_map(|line| line.as_str()).collect::<Vec<_>>().join("\n");
    assert!(text(&viewport).contains("line-10"), "the viewport holds the last output: {:?}", text(&viewport));
    assert_eq!(screen["alt_screen"], false, "a shell prompt is the primary screen");

    // And with history, the lines that scrolled off — which only the broker still has
    ask(&mut writing, &format!(r#"{{"kind":"screen","session":"{session}","history":50}}"#));
    let (with_history, _) = until(&mut reading, "screen");
    let all = with_history["lines"].as_array().expect("lines").clone();
    assert!(all.len() > viewport.len(), "history came with it: {} lines", all.len());
    assert!(text(&all).contains("line-1\n"), "a line that scrolled off the viewport: {:?}", text(&all));

    // A ceiling, not a promise: asking for more history than there is answers with what there is
    assert!(all.len() <= 6 + 50, "history is bounded by what was asked for");

    ask(&mut writing, &format!(r#"{{"kind":"kill","session":"{session}"}}"#));
    until(&mut reading, "exited");
}

/// The screen a session no longer has is a refusal by code, like every other request naming one.
#[test]
fn a_screen_read_for_a_session_that_does_not_exist_is_refused_by_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"screen","session":"s-404","history":10}"#);
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
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();

    once_the_shell_is_listening(&mut reading);
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&format!("exit{SUBMIT}")).expect("a JSON string")
        ),
    );
    until(&mut reading, "exited");

    // Straight after the exit, which is the window ConPTY is documented to be unhappy in
    ask(&mut writing, &format!(r#"{{"kind":"write","session":"{session}","text":"x"}}"#));
    let refused = until(&mut reading, "failed").0;
    assert_eq!(refused["code"], "NO_SUCH_SESSION");

    ask(&mut writing, &format!(r#"{{"kind":"resize","session":"{session}","cols":100,"rows":30}}"#));
    let refused = until(&mut reading, "failed").0;
    assert_eq!(refused["code"], "NO_SUCH_SESSION");

    // And the broker is still there to answer the next thing asked of it
    ask(&mut writing, r#"{"kind":"screen","session":"s-404","history":0}"#);
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
    ask(&mut writing, &hello(None));
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
        "unset": [],
        "cwd": null,
    })
    .to_string();
    ask(&mut writing, &frame);
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();

    let seen = output_until(&mut reading, Some(&session), |seen| seen.contains("marker="));

    assert!(seen.contains("marker=arranged"), "the argument ran and the variable reached it: {seen:?}");
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
            output.push_str(event["text"].as_str().unwrap_or_default());
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
    ask(&mut writing, &hello(None));
    assert_eq!(next(&mut reading)["kind"], "hello");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    let session = next(&mut reading)["session"].as_str().expect("a session id").to_owned();
    once_the_shell_is_listening(&mut reading);
    (reading, writing, session)
}

fn types(writing: &mut impl Write, session: &str, data: &str) {
    ask(
        writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
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
    assert_eq!(facts["finished"], 1, "and it is counted, for a caller that asks after it ended");
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

    ask(&mut writing, &format!(r#"{{"kind":"search","session":"{session}","text":"line-7","history":1000}}"#));
    let (found, _) = until(&mut reading, "search");
    let matches = found["matches"].as_array().expect("matches");
    assert!(matches.iter().any(|m| m["text"] == "line-7"), "the line it was asked for: {matches:?}");
    // The whole line comes back, so a caller sees its context without a second read
    assert!(matches[0]["column"].as_u64().is_some(), "with where in the line it starts");

    // Text nothing wrote is not found, which is the half that fails if a search answers everything
    ask(&mut writing, &format!(r#"{{"kind":"search","session":"{session}","text":"line-11","history":1000}}"#));
    let (none, _) = until(&mut reading, "search");
    assert_eq!(none["matches"].as_array().expect("matches").len(), 0, "nothing wrote it");
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
        if let Ok(line) = reading.line(std::time::Duration::from_millis(300)) {
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
    ask(&mut writing, &hello(Some(role)));
    assert_eq!(next(&mut reading)["kind"], "hello", "greeted as {role}");
    (reading, writing)
}

/// **A session outlives the client that opened it, while an owner holds the broker.**
///
/// This is what the whole lifetime rule is for: restarting the service that opened a session must not end
/// it. The client here goes away entirely — its socket is dropped — and the shell is still there to answer
/// a second client afterwards.
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
            output.push_str(event["text"].as_str().unwrap_or_default());
            if output.contains("back-on-the-feed") {
                return;
            }
        }
    }
    panic!("the new client never received the session's output: {output:?}");
}

/// A caller that names no role is a client, so nothing that spoke the old handshake changes behaviour.
#[test]
fn a_connection_that_names_no_role_is_a_client() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
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
            output.push_str(event["text"].as_str().unwrap_or_default());
            if output.contains("still-mine") {
                return;
            }
        }
    }
    panic!("the owner took the feed from the client: {output:?}");
}

/// **A broker nobody is connected to exits**, which is the difference between a process per app launch and
/// none. It used to listen for ever: its sessions were ended, so nothing leaked that a user could feel, and
/// one process accumulated every time the app started.
#[test]
fn a_broker_nobody_is_connected_to_exits() {
    let mut broker = start();
    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    next(&mut reading);
    once_the_shell_is_listening(&mut reading);

    disconnect(reading, writing);

    for _ in 0..100 {
        if let Ok(Some(_)) = broker.process.try_wait() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the broker went on listening with nobody connected");
}

/// The other direction, so the case above cannot be satisfied by a broker that exits whenever it feels
/// like it: while an owner holds it, a client coming and going leaves it listening.
#[test]
fn a_broker_an_owner_holds_keeps_listening_between_clients() {
    let mut broker = start();
    let (_owner_events, _owner) = greet(&broker, "lifetime");

    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    next(&mut reading);
    once_the_shell_is_listening(&mut reading);
    disconnect(reading, writing);

    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(matches!(broker.process.try_wait(), Ok(None)), "the broker exited while the app still held it");
    // And it still answers, which is what "listening" has to mean
    let (mut reading, mut writing) = greet(&broker, "client");
    ask(&mut writing, &spawn_frame(80, 24, Some(DEFAULT_SHELL)));
    assert_eq!(next(&mut reading)["kind"], "spawned");
    let _ = &mut writing;
}

/// **X4: a Windows shell reports itself when it is started pointing at an integration file.**
///
/// Shell integration on Windows was inert — `integrationFor` answered nothing there, so the markers the
/// working directory and the command status come from were never emitted whatever the setting said. The
/// question this answers is the one that cannot be answered from a Mac: does PowerShell, launched the way
/// the app would launch it, emit OSC 633, and does the broker read it?
///
/// It is written here rather than against the app because the broker is where the markers are parsed, and
/// because this is the layer a Windows run can reach without an Electron build.
#[cfg(windows)]
#[test]
fn a_windows_shell_reports_itself_when_started_with_an_integration_file() {
    let directory = tempfile::tempdir().expect("a directory for the integration file");
    let script = directory.path().join("apack.ps1");
    // The prompt is the one hook every PowerShell has: it runs after each command and before the next line
    // is read, which is exactly the boundary the markers describe
    std::fs::write(
        &script,
        "$Global:__apackPrompt = $function:prompt\r\n\
         function Global:prompt {\r\n\
         $status = $LASTEXITCODE\r\n\
         if ($null -eq $status) { $status = 0 }\r\n\
         $e = [char]27\r\n\
         $bel = [char]7\r\n\
         $theirs = if ($Global:__apackPrompt) { & $Global:__apackPrompt } else { \"PS> \" }\r\n\
         \"$e]633;D;$status$bel\" + \"$e]633;P;Cwd=$($PWD.Path)$bel\" + \"$e]633;A$bel\" + $theirs + \"$e]633;B$bel\"\r\n\
         }\r\n",
    )
    .expect("the integration file is written");

    let broker = start();
    let (mut reading, mut writing) = greet(&broker, "client");
    let frame = serde_json::json!({
        "kind": "spawn", "cols": 120, "rows": 30, "shell": "powershell.exe",
        "args": ["-NoExit", "-Command", format!(". '{}'", script.display())],
        "env": [], "unset": [], "cwd": null,
    })
    .to_string();
    ask(&mut writing, &frame);
    let session = next(&mut reading)["session"].as_str().expect("a session id").to_owned();

    // The first prompt is where the markers are drawn, so nothing is typed until one has been
    let mut output = output_until(&mut reading, None, |seen| seen.contains("\u{1b}]633;B"));
    assert!(output.contains("\u{1b}]633;P;Cwd="), "PowerShell emitted no working-directory marker: {output:?}");

    // **`\r` alone, not `\r\n`.** A trailing newline opens PowerShell's continuation prompt — the command
    // is taken and then the parser waits for more, so nothing runs. Measured here, not assumed.
    types(&mut writing, &session, "cd C:\\Windows\r");
    output.push_str(&output_until(&mut reading, None, |seen| seen.matches("\u{1b}]633;P;Cwd=").count() >= 1));

    ask(&mut writing, &format!(r#"{{"kind":"facts","session":"{session}"}}"#));
    let (facts, _) = until(&mut reading, "facts");
    assert_eq!(facts["cwd"], "C:\\Windows", "the shell moved and said so, and the broker read it: {output:?}");
}

/// **A frame is read before it is understood, so what may be read has to be bounded.**
///
/// `lines()` grows its buffer until a newline arrives. A client that writes and never sends one is therefore
/// unbounded by construction — and this process owns every terminal in the app, so being killable by a
/// sender that declines to finish a sentence is every session gone at once. The cap is the answer, and what
/// makes it a cap rather than a comment is that the broker is still there afterwards.
#[test]
fn a_frame_without_end_is_refused_and_the_broker_survives() {
    let broker = start();
    // The app's own hold, so that the client below leaving does not end the broker for the ordinary reason:
    // what is under test is a frame closing one connection, not the lifetime rule
    let (_owner_events, _owner) = greet(&broker, "lifetime");
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, &hello(None));
    assert_eq!(next(&mut reading)["kind"], "hello");

    // A megabyte and one byte, with no newline anywhere in it
    writing.write_all(&vec![b'x'; (1 << 20) + 1]).expect("the broker takes the bytes");
    writing.flush().expect("flushed");

    let refusal = next(&mut reading);
    assert_eq!(refusal["kind"], "failed", "an unbounded frame was not refused: {refusal}");
    assert_eq!(refusal["code"], "MALFORMED", "{refusal}");
    // Named, because `serde_json` refuses a megabyte of `x` with `MALFORMED` too: what this case is about
    // is the cap answering first, and only its own message says so
    assert!(
        refusal["message"].as_str().is_some_and(|said| said.contains("at most")),
        "the refusal was the parser's rather than the cap's: {refusal}"
    );

    // The half that makes this a bound rather than a crash: another client is still served
    let (mut again, mut writing_again) = connect(&broker);
    ask(&mut writing_again, &hello(None));
    assert_eq!(next(&mut again)["kind"], "hello", "the broker did not survive the oversized frame");
}

/// **A frame of exactly the cap is inside it**, which is the boundary the case above cannot see.
///
/// That one sends a byte past the cap, so it passes whether the comparison is `>` or `>=` — a cap one byte
/// tight refuses a legal frame and nothing would have said so. Found by mutating the comparison, which is
/// the only way an off-by-one on a bound ever announces itself.
#[test]
fn a_frame_of_exactly_the_cap_is_taken() {
    let broker = start();
    let (_owner_events, _owner) = greet(&broker, "lifetime");
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, &hello(None));
    assert_eq!(next(&mut reading)["kind"], "hello");

    // A valid frame for a session that does not exist, padded so the line — the frame and the newline
    // `ask` adds — is exactly the cap. Its refusal names the session, not the cap, which is the assertion.
    const CAP: usize = 1 << 20;
    let around = r#"{"kind":"write","session":"nope","text":""}"#.len() + 1;
    let padded = "x".repeat(CAP - around);
    let frame = format!(r#"{{"kind":"write","session":"nope","text":"{padded}"}}"#);
    assert_eq!(frame.len() + 1, CAP, "the frame is not exactly the cap");
    ask(&mut writing, &frame);

    let refusal = next(&mut reading);
    assert_eq!(refusal["kind"], "failed", "{refusal}");
    assert!(
        !refusal["message"].as_str().is_some_and(|said| said.contains("at most")),
        "a frame of exactly the cap was refused by the cap: {refusal}"
    );
}

/// **A grid nothing could draw is answered rather than attempted.**
///
/// `cols` and `rows` are `u16`, so a frame may ask for 65535 by 65535 — four billion cells, allocated per
/// session by a process that owns every terminal in the app. This is the firing case for the cap: without
/// it the broker attempts the allocation and dies, taking every other session with it, and the suite sees
/// its connection close rather than an answer.
#[test]
fn a_grid_nothing_could_draw_is_capped_rather_than_attempted() {
    let broker = start();
    let (_owner_events, _owner) = greet(&broker, "lifetime");
    let (mut reading, mut writing) = connect(&broker);

    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(
        &mut writing,
        r#"{"kind":"spawn","cols":65535,"rows":65535,"shell":null,"args":[],"env":[],"unset":[],"cwd":null}"#,
    );

    let spawned = next(&mut reading);
    assert_eq!(spawned["kind"], "spawned", "an impossible grid was refused rather than capped: {spawned}");
}

/// What prints `TERM` and nothing else, per platform. The shell differs; the question does not.
#[cfg(unix)]
const PRINTS_TERM: (&str, [&str; 2]) = ("/bin/sh", ["-c", "printf 'term=%s\\n' \"$TERM\""]);
/// `cmd.exe` rather than PowerShell: `%TERM%` expands in one pass with no profile to load and no quoting
/// rules to get wrong, and this case is about the variable rather than about any shell's syntax.
#[cfg(windows)]
const PRINTS_TERM: (&str, [&str; 2]) = ("cmd.exe", ["/c", "echo term=%TERM%"]);

/// **A shell is told what terminal it is in, whoever launched the app** (X13).
///
/// Nothing in the request names `TERM` here, so what the shell prints is what the broker put there. An app
/// started from the Dock or the Start menu has none to pass on, and a shell with no `TERM` reads no terminfo
/// — which is how this suite's own alternate-screen case came to wait ten seconds on a CI runner.
///
/// **It runs on both platforms**, because the claim is the broker's and not a POSIX one: the broker sets
/// `TERM` in `spawn` with no `cfg` around it, so a case that only ever ran on a Mac left the half that
/// matters most unasserted — a Windows app is the one most likely to be launched from a shortcut with no
/// environment at all.
#[test]
fn a_shell_is_told_what_terminal_it_is_in() {
    // No `TERM` for the broker to pass on, so what the shell prints is what the broker decided
    let broker = start_with_env(|command| {
        command.env_remove("TERM");
    });
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);

    let (shell, args) = PRINTS_TERM;
    let frame = serde_json::json!({
        "kind": "spawn",
        "cols": 80,
        "rows": 24,
        "shell": shell,
        "args": args,
        "env": [],
        "unset": [],
        "cwd": null,
    })
    .to_string();
    ask(&mut writing, &frame);
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();

    let seen = output_until(&mut reading, Some(&session), |seen| seen.contains("term=xterm-256color"));
    // `cmd.exe` echoes the command before running it, so the value has to be matched rather than the label:
    // `echo term=%TERM%` appears in the output whatever TERM is, including when it is unset
    assert!(seen.contains("term=xterm-256color"), "the shell was given no terminal to be: {seen:?}");
}

/// **What is running in a session is measured, not asked of the shell** (`Foreground`).
///
/// A shell with no integration installed emits no markers, so every claim in `Facts` is empty for it and a
/// caller cannot tell a session sitting at a prompt from one three minutes into a build. The pty's
/// foreground process group needs no cooperation from whatever is running in it, which is why this is the
/// field a tab title and a confirmation before closing can actually be built on.
///
/// **`cat` rather than a shell, and its own echo as the sync point.** A shell's reported name is not
/// portable — macOS answers `bash` for `/bin/sh` — and asking straight after `spawn` would read a process
/// that may still be between `fork` and `execve`, which is a case that passes on a quiet machine. Writing a
/// line and waiting for it back proves the program is running before anything is measured.
#[cfg(unix)]
#[test]
fn what_is_running_in_a_session_is_measured_rather_than_claimed() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);

    let frame = serde_json::json!({
        "kind": "spawn",
        "cols": 80,
        "rows": 24,
        "shell": "/bin/cat",
        "args": [],
        "env": [],
        "unset": [],
        "cwd": null,
    })
    .to_string();
    ask(&mut writing, &frame);
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    ask(&mut writing, &serde_json::json!({ "kind": "write", "session": session, "text": "ready\n" }).to_string());

    let seen = output_until(&mut reading, Some(&session), |seen| seen.contains("ready"));
    assert!(seen.contains("ready"), "the program is running: {seen:?}");

    ask(&mut writing, &serde_json::json!({ "kind": "facts", "session": session }).to_string());
    let facts = loop {
        let event = next(&mut reading);
        if event["kind"] == "facts" {
            break event;
        }
    };

    assert_eq!(facts["command"], serde_json::Value::Null, "this program claimed nothing, which is the case");
    assert_eq!(facts["foreground"]["name"], "cat", "so the kernel was asked instead");
    assert!(facts["foreground"]["pid"].as_i64().unwrap_or_default() > 0, "and it named a live process");
}

/// **The last client leaving ends its sessions even while something else holds the broker open.**
///
/// A connection that has not greeted yet is counted as a connection and not as a client, so it keeps the
/// process alive while the client count falls to zero — which is the one window in which that count decides
/// on its own, and the window a probe, a health check or a reconnect racing a quit actually opens. Without
/// it the only thing left to end a user's shells is the broker exiting, and here it does not.
#[test]
fn the_last_client_leaving_ends_its_sessions_while_another_connection_holds_the_broker() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let (spawned, _) = until(&mut reading, "spawned");
    let pid = spawned["pid"].as_u64().expect("a pid") as i32;

    // Connected and silent. The broker is not exiting after this disconnect, so ending the sessions is the
    // client count's own work rather than something the process teardown would have done anyway
    let holding = connect(&broker);

    disconnect(reading, writing);

    assert!(reaped(pid), "the shell for pid {pid} outlived every client of it");
    drop(holding);
}

/// Two sessions are two sessions. The id is how every later request names one, so a second shell answering
/// to the first one's id puts `write`, `resize` and `kill` on whichever the map happens to be holding — and
/// a keystroke meant for one shell reaches another.
#[test]
fn two_sessions_are_told_apart() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, &spawn_frame(80, 24, None));
    let (first, _) = until(&mut reading, "spawned");
    ask(&mut writing, &spawn_frame(80, 24, None));
    let (second, _) = until(&mut reading, "spawned");

    assert_ne!(
        first["session"].as_str().expect("a session"),
        second["session"].as_str().expect("a session"),
        "the second shell took the first one's id"
    );
}

/// **Flow control, which is a second class from the credit that bounds the channel.**
///
/// Credit bounds how much output may be in flight for every session at once, because one socket carries
/// them all. These cases are about one session being stopped because *its* consumer asked — an image
/// transport that has to drain before the next frame — which a consumer cannot get by reading its socket
/// more slowly without stalling every other terminal in the app.
///
/// **A read already outstanding still lands.** The reader is parked before a `read`, and when the pause
/// arrives it is usually inside one — so the chunk that read returns is delivered and the parking happens
/// after it. That is `node-pty`'s `pause()` too, and what a consumer handling a chunk means by pausing.
#[cfg(unix)]
#[test]
fn a_paused_session_stops_being_read_and_a_resume_delivers_what_it_missed() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    once_the_shell_is_listening(&mut reading);

    ask(&mut writing, &format!(r#"{{"kind":"pause","session":"{session}"}}"#));
    // More than one `read` of output, so the whole of it cannot arrive in the one chunk a pause may still
    // deliver: four thousand lines against an 8 KiB buffer
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&format!("seq 1 4000 | sed 's/^/probe-/'{SUBMIT}")).expect("a JSON string")
        ),
    );

    // What a feed that was *not* paused would certainly have delivered by now. A slow machine can only make
    // this read less, which cannot fail the case falsely — it is the tail arriving *after* the resume that
    // the assertion is about
    let mut before = String::new();
    while let Ok(line) = reading.line(std::time::Duration::from_millis(500)) {
        let event: serde_json::Value = serde_json::from_str(&line).expect("an event");
        if event["kind"] == "output" {
            before.push_str(event["text"].as_str().unwrap_or_default());
        }
    }
    assert!(!before.contains("probe-4000"), "a paused session went on being read: {} bytes arrived", before.len());

    ask(&mut writing, &format!(r#"{{"kind":"resume","session":"{session}"}}"#));
    let after = output_until(&mut reading, Some(&session), |seen| seen.contains("probe-4000"));
    assert!(after.contains("probe-4000"), "the resumed session never delivered its tail: {after:?}");
    // Held in the kernel's buffer rather than dropped, which is what makes a pause backpressure: the lines
    // written while nobody was reading are still there to be read
    let whole = format!("{before}{after}");
    assert!(whole.contains("probe-1\r\n"), "the start of the output was lost over the pause");
    assert!(whole.contains("probe-2000"), "the middle of the output was lost over the pause");
}

/// A pause belongs to the client that asked for one: the next client never did, and has no way to find out.
#[cfg(unix)]
#[test]
fn a_pause_does_not_outlive_the_client_that_asked_for_it() {
    let broker = start();
    // An owner, so the session survives the client leaving and there is something left to be stuck
    let (mut owning, mut owner) = connect(&broker);
    ask(&mut owner, &hello(Some("lifetime")));
    next(&mut owning);

    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    once_the_shell_is_listening(&mut reading);
    ask(&mut writing, &format!(r#"{{"kind":"pause","session":"{session}"}}"#));
    disconnect(reading, writing);

    let (mut again, mut writing_again) = connect(&broker);
    ask(&mut writing_again, &hello(None));
    next(&mut again);
    ask(
        &mut writing_again,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&probe("inherited-probe")).expect("a JSON string")
        ),
    );

    let output = output_until(&mut again, Some(&session), |seen| seen.contains("inherited-probe"));
    assert!(output.contains("inherited-probe"), "the new client inherited a pause it never asked for: {output:?}");
}

/// **An exit and a signal are different answers.** Reported as one they are the same `1`, so a window
/// cannot tell the shutdown the app asked for from a crash it should draw a seam for.
#[cfg(unix)]
#[test]
fn a_session_killed_by_a_signal_names_it_and_reports_no_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    once_the_shell_is_listening(&mut reading);

    // SIGKILL rather than SIGTERM: an interactive shell ignores termination, and what this case is about is
    // the answer rather than the shell's manners
    ask(
        &mut writing,
        &format!(r#"{{"kind":"signal","session":"{session}","signal":"SIGKILL","scope":{{"kind":"leader"}}}}"#),
    );

    let (exited, _) = until(&mut reading, "exited");
    assert_eq!(exited["signal"], "SIGKILL", "the signal is named: {exited:?}");
    assert!(exited["code"].is_null(), "a signal is not an exit code: {exited:?}");
}

#[cfg(unix)]
#[test]
fn a_session_that_exits_of_its_own_accord_reports_a_code_and_no_signal() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    once_the_shell_is_listening(&mut reading);

    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&format!("exit 7{SUBMIT}")).expect("a JSON string")
        ),
    );

    let (exited, _) = until(&mut reading, "exited");
    assert_eq!(exited["code"], 7, "the code the shell exited with: {exited:?}");
    assert!(exited["signal"].is_null(), "nothing signalled it: {exited:?}");
}

/// **The pid scope is what a consumer driving one program needs, and the check is what makes it safe to
/// offer.** A pid is a number any caller can name, so without the tree check this request is `kill(2)` over
/// a socket.
#[cfg(unix)]
#[test]
fn a_signal_reaches_a_process_inside_the_session_s_tree_and_nothing_outside_it() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    once_the_shell_is_listening(&mut reading);
    let notes = tempfile::tempdir().expect("a directory for the grandchild's pid");
    let pid_file = notes.path().join("grandchild.pid");
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&long_lived_grandchild(&pid_file)).expect("a JSON string")
        ),
    );
    let grandchild = pid_written_to(&mut reading, &pid_file);

    // **This process, which is the safest pid to name and not only a convenient one.** It is the broker's
    // parent, so it is certainly outside every session's tree — and if a mutation of the tree walk ever
    // admits it, what the broker then signals is this test's own process and nothing else. Naming any other
    // real process would make a mutant's blast radius the user's.
    let outside = std::process::id();
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"signal","session":"{session}","signal":"SIGKILL","scope":{{"kind":"pid","pid":{outside}}}}}"#
        ),
    );
    let (failed, _) = until(&mut reading, "failed");
    assert_eq!(failed["code"], "OUTSIDE_SESSION", "a pid outside the tree was signalled: {failed:?}");
    assert!(alive(outside as i32), "the broker signalled a process it does not own");

    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"signal","session":"{session}","signal":"SIGTERM","scope":{{"kind":"pid","pid":{grandchild}}}}}"#
        ),
    );
    assert!(reaped(grandchild), "the grandchild {grandchild} was not reached by a signal to its own pid");

    // **The answer, and not only the effect.** A mutation of the syscall's success test leaves the signal
    // delivered and reports a failure for it, which `reaped` cannot see. A `screen` request is what makes
    // the absence of one observable without waiting out the silence window: `until` hands back everything
    // the broker said before the answer it waits for, so a spurious `failed` arrives inside it.
    ask(&mut writing, &format!(r#"{{"kind":"screen","session":"{session}","history":0}}"#));
    let (_, before) = until(&mut reading, "screen");
    assert!(
        before.iter().all(|event| event["kind"] != "failed"),
        "a signal that was delivered was reported as a failure: {before:?}"
    );
}

/// **A child in the leader's own process group**, which is what `kill(-pid)` reaches.
///
/// `set +m` turns the shell's job monitor off, so the background job stays in the shell's process group
/// instead of being given one of its own. That is deliberate and is the subject: with job control on, a
/// group signal does *not* reach a job, which is why `Scope::Group` says so and why `Kill` is what ends a
/// whole tree. A consumer driving one CLI is in this case rather than that one.
#[cfg(unix)]
fn grandchild_in_the_leaders_group(pid_file: &std::path::Path) -> String {
    format!("set +m; sleep 300 & echo $! > {}\n", pid_file.display())
}

/// The group is what reaches whatever the leader started in it, which is the scope a shutdown escalates to.
#[cfg(unix)]
#[test]
fn a_signal_to_the_group_reaches_what_the_shell_started() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let session = next(&mut reading)["session"].as_str().expect("a session").to_owned();
    once_the_shell_is_listening(&mut reading);
    let notes = tempfile::tempdir().expect("a directory for the grandchild's pid");
    let pid_file = notes.path().join("grandchild.pid");
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&grandchild_in_the_leaders_group(&pid_file)).expect("a JSON string")
        ),
    );
    let grandchild = pid_written_to(&mut reading, &pid_file);
    assert!(alive(grandchild), "the grandchild {grandchild} is running");

    ask(
        &mut writing,
        &format!(r#"{{"kind":"signal","session":"{session}","signal":"SIGKILL","scope":{{"kind":"group"}}}}"#),
    );

    assert!(reaped(grandchild), "a signal to the group left the grandchild {grandchild} running");
}

#[test]
fn pausing_a_session_that_does_not_exist_is_refused_by_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"pause","session":"s-404"}"#);
    let failed = next(&mut reading);
    assert_eq!(failed["code"], "NO_SUCH_SESSION", "{failed:?}");
}

/// **A client leaving takes the feed away only if it had it**, which is what "last greeting wins" means
/// from the other side.
///
/// The departing one is not the attached one here, and what it must not do is unhook the client that is —
/// nor lift a pause that client is relying on, now that a departure resumes what it paused.
#[cfg(unix)]
#[test]
fn a_client_that_is_not_the_attached_one_leaving_takes_nothing_with_it() {
    let broker = start();
    let (first, mut opening) = connect(&broker);
    ask(&mut opening, &hello(None));
    let mut first = first;
    next(&mut first);
    ask(&mut opening, &spawn_frame(80, 24, None));
    let session = next(&mut first)["session"].as_str().expect("a session").to_owned();

    // Last greeting wins the feed, so this one is now the attached client
    let (mut attached, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut attached);

    disconnect(first, opening);

    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&probe("still-attached")).expect("a JSON string")
        ),
    );
    let output = output_until(&mut attached, Some(&session), |seen| seen.contains("still-attached"));
    assert!(
        output.contains("still-attached"),
        "another client leaving took the feed from the attached one: {output:?}"
    );
}

/// **The question the `Pid` scope's refusal answers by acting**, asked without acting.
///
/// A consumer admits a process that claims to be its own — *"I am yours, pid N"* over a socket, where the
/// pid is whatever that process chose to write. Walking the parent chain from outside is a second
/// implementation of what the broker checks before it signals, and two implementations of one invariant can
/// disagree. These cases are that both come from the same walk.
#[cfg(unix)]
#[test]
fn a_session_says_which_pids_are_inside_its_own_tree() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let spawned = next(&mut reading);
    let session = spawned["session"].as_str().expect("a session").to_owned();
    let leader = spawned["pid"].as_u64().expect("a pid");
    once_the_shell_is_listening(&mut reading);
    let notes = tempfile::tempdir().expect("a directory for the grandchild's pid");
    let pid_file = notes.path().join("grandchild.pid");
    ask(
        &mut writing,
        &format!(
            r#"{{"kind":"write","session":"{session}","text":{}}}"#,
            serde_json::to_string(&long_lived_grandchild(&pid_file)).expect("a JSON string")
        ),
    );
    let grandchild = pid_written_to(&mut reading, &pid_file);

    // The process it started, however the shell arranged it
    ask(&mut writing, &format!(r#"{{"kind":"owns","session":"{session}","pid":{grandchild}}}"#));
    let (answer, _) = until(&mut reading, "owns");
    assert_eq!(answer["pid"].as_u64(), Some(grandchild as u64), "the answer names the pid it is about");
    assert_eq!(answer["owned"], true, "a process the session started is its own: {answer:?}");

    // Its own process, which is the edge a walk gets wrong by starting one hop up
    ask(&mut writing, &format!(r#"{{"kind":"owns","session":"{session}","pid":{leader}}}"#));
    let (own, _) = until(&mut reading, "owns");
    assert_eq!(own["owned"], true, "the session's own process is inside its tree: {own:?}");

    // This process is the broker's parent, so it is outside every session's tree
    let outside = std::process::id();
    ask(&mut writing, &format!(r#"{{"kind":"owns","session":"{session}","pid":{outside}}}"#));
    let (foreign, _) = until(&mut reading, "owns");
    assert_eq!(foreign["pid"].as_u64(), Some(outside as u64));
    assert_eq!(foreign["owned"], false, "a process outside the tree is not owned: {foreign:?}");
}

/// **The pid is the correlation**, as the session is everywhere else: a consumer may have several claims to
/// admit at once, and an answer taken for another pid's is one admitted or refused wrongly.
#[cfg(unix)]
#[test]
fn two_questions_about_one_session_are_each_answered_about_their_own_pid() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    ask(&mut writing, &spawn_frame(80, 24, None));
    let spawned = next(&mut reading);
    let session = spawned["session"].as_str().expect("a session").to_owned();
    let leader = spawned["pid"].as_u64().expect("a pid");
    let outside = std::process::id() as u64;

    ask(&mut writing, &format!(r#"{{"kind":"owns","session":"{session}","pid":{leader}}}"#));
    ask(&mut writing, &format!(r#"{{"kind":"owns","session":"{session}","pid":{outside}}}"#));

    let (first, _) = until(&mut reading, "owns");
    let (second, _) = until(&mut reading, "owns");
    let mut answers =
        [(first["pid"].as_u64(), first["owned"].as_bool()), (second["pid"].as_u64(), second["owned"].as_bool())];
    answers.sort();
    let mut wanted = [(Some(leader), Some(true)), (Some(outside), Some(false))];
    wanted.sort();
    assert_eq!(answers, wanted, "each answer carries the pid it is about");
}

#[test]
fn asking_what_a_session_that_does_not_exist_owns_is_refused_by_code() {
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);

    ask(&mut writing, r#"{"kind":"owns","session":"s-404","pid":2}"#);
    let failed = next(&mut reading);
    assert_eq!(failed["code"], "NO_SUCH_SESSION", "{failed:?}");
}

/// **A caller can keep a name out of its child, which adding entries cannot do** (`unset`).
///
/// A session inherits the broker's environment, so a consumer spawning a *program* rather than the user's
/// shell has no way to subtract from it — and what it is usually subtracting is a credential the app
/// carries and the program must not see: a CLI that found one would authenticate as the app rather than as
/// whoever the user signed in as.
///
/// The broker's own environment is arranged here on purpose. Without that the case passes whether or not
/// anything was removed, which is the asymmetry `start_with_env` exists for.
#[cfg(unix)]
#[test]
fn a_name_the_request_unsets_is_absent_from_the_child_even_though_the_broker_has_it() {
    const PRINTS_SECRET: (&str, [&str; 2]) = ("/bin/sh", ["-c", "printf 'secret=[%s]\\n' \"$A_CREDENTIAL\""]);
    let broker = start_with_env(|command| {
        command.env("A_CREDENTIAL", "the-app-s-own");
    });
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    let (shell, args) = PRINTS_SECRET;

    // Inherited, which is what makes the other half of this case mean anything
    let inherited = serde_json::json!({
        "kind": "spawn", "cols": 80, "rows": 24, "shell": shell, "args": args,
        "env": [], "unset": [], "cwd": null,
    })
    .to_string();
    ask(&mut writing, &inherited);
    // **`until` rather than `next`**: this program exits as soon as it has printed, so its `exited` frame is
    // in flight beside the next `spawned` — and `next` would hand back whichever arrived, whose `session`
    // field reads perfectly well and names a session that has already finished
    let (spawned, _) = until(&mut reading, "spawned");
    let first = spawned["session"].as_str().expect("a session").to_owned();
    let seen = output_until(&mut reading, Some(&first), |seen| seen.contains("secret=["));
    assert!(seen.contains("secret=[the-app-s-own]"), "the broker's own environment reaches a child: {seen:?}");

    let without = serde_json::json!({
        "kind": "spawn", "cols": 80, "rows": 24, "shell": shell, "args": args,
        "env": [], "unset": ["A_CREDENTIAL"], "cwd": null,
    })
    .to_string();
    ask(&mut writing, &without);
    let (second_spawned, _) = until(&mut reading, "spawned");
    let second = second_spawned["session"].as_str().expect("a session").to_owned();
    let gone = output_until(&mut reading, Some(&second), |seen| seen.contains("secret=["));
    assert!(gone.contains("secret=[]"), "the name was still in the child's environment: {gone:?}");
}

/// **Absence is what a caller gets when it asks for both**, because a name it asked to be rid of is one it
/// means to be rid of — an override that also named it would otherwise be the one place the guarantee broke.
#[cfg(unix)]
#[test]
fn unsetting_a_name_wins_over_setting_it() {
    const PRINTS_SECRET: (&str, [&str; 2]) = ("/bin/sh", ["-c", "printf 'secret=[%s]\\n' \"$A_CREDENTIAL\""]);
    let broker = start();
    let (mut reading, mut writing) = connect(&broker);
    ask(&mut writing, &hello(None));
    next(&mut reading);
    let (shell, args) = PRINTS_SECRET;

    let frame = serde_json::json!({
        "kind": "spawn", "cols": 80, "rows": 24, "shell": shell, "args": args,
        "env": [["A_CREDENTIAL", "set-by-the-caller"]], "unset": ["A_CREDENTIAL"], "cwd": null,
    })
    .to_string();
    ask(&mut writing, &frame);
    let (spawned, _) = until(&mut reading, "spawned");
    let session = spawned["session"].as_str().expect("a session").to_owned();

    let seen = output_until(&mut reading, Some(&session), |seen| seen.contains("secret=["));
    assert!(seen.contains("secret=[]"), "an override outlived the unset that named it: {seen:?}");
}
