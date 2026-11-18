//! `apack-terminal proxy`: a broker reached through a process's stdin and stdout.
//!
//! Driven as `ssh` or `docker exec` would carry it — a child whose pipes are the connection — against the
//! real binary: that a broker is started when there is none, that the pipe is the broker's protocol byte
//! for byte, and that a second proxy joins the first's broker rather than starting another.

use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use crate::heard::Heard;

use apack_terminal::listening::{HELD_EXIT, START_GRACE, START_GRACE_ENV, start_grace};
#[cfg(unix)]
use apack_terminal::listening::{Refused, hold};
use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role};
use apack_terminal::proxy::{KEPT_BYTES, LOOKS};

struct Proxy {
    process: Child,
    input: ChildStdin,
    events: Heard<Event>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn read_events(output: ChildStdout) -> Heard<Event> {
    crate::heard::events(output)
}

/// Where a broker listens: `name` in `directory` on POSIX, and on Windows a named pipe, where a path
/// is refused. The pipe's name carries this process and a count, there being one namespace for the
/// whole machine (`tests/broker.rs`, `socket_in`).
#[cfg(unix)]
fn socket_in(directory: &Path, name: &str) -> std::path::PathBuf {
    directory.join(name)
}

#[cfg(windows)]
fn socket_in(_directory: &Path, _name: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::path::PathBuf::from(format!(r"\\.\pipe\apack-terminal-proxy-test-{}-{nth}", std::process::id()))
}

/// The shell each platform is sure to have, and how a line typed at it is ended.
#[cfg(unix)]
const SHELL: (&str, &str) = ("/bin/sh", "\n");
#[cfg(windows)]
const SHELL: (&str, &str) = ("cmd.exe", "\r\n");

/// A line that prints `before` and `after` as one word that is nowhere in the line itself, so that
/// what a case waits for is the command's output and not the echo of what was typed.
fn echoing(before: &str, after: &str) -> String {
    let (_, submit) = SHELL;
    if cfg!(windows) { format!("echo {before}^{after}{submit}") } else { format!("echo {before}''{after}{submit}") }
}

fn proxy(socket: &std::path::Path) -> Proxy {
    let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
    command.arg("proxy").arg(socket).stdin(Stdio::piped()).stdout(Stdio::piped());
    // In the case's job when it holds an `Ending`, so the broker this starts is ended with the case
    let mut process = crate::reaper::spawn_contained(&mut command).expect("the proxy starts");
    let input = process.stdin.take().expect("piped stdin");
    let events = read_events(process.stdout.take().expect("piped stdout"));
    Proxy { process, input, events }
}

impl Proxy {
    fn say(&mut self, request: &Request) {
        let mut line = serde_json::to_string(request).expect("a request serializes");
        line.push('\n');
        self.input.write_all(line.as_bytes()).expect("the proxy reads");
    }

    #[track_caller]
    fn event(&self) -> Event {
        self.events.next()
    }

    fn greet(&mut self, role: Role) {
        self.say(&Request::Hello { protocol: PROTOCOL_VERSION, role, challenge: None });
        let answered = self.event();
        assert!(matches!(answered, Event::Hello { .. }), "a greeting was answered {answered:?}");
    }

    fn shell(&mut self) -> String {
        self.say(&Request::Spawn {
            cols: 80,
            rows: 24,
            shell: Some(SHELL.0.to_owned()),
            args: Vec::new(),
            env: vec![("PS1".to_owned(), "$ ".to_owned())],
            unset: Vec::new(),
            cwd: None,
            colors: None,
            output: true,
            record: None,
            restore: false,
            images: false,
            image_files: false,
            marks_key: None,
        });
        loop {
            if let Event::Spawned { session, .. } = self.event() {
                return session;
            }
        }
    }

    /// Output until it holds `expected`.
    fn output_with(&self, expected: &str) -> String {
        let mut seen = String::new();
        while !seen.contains(expected) {
            if let Event::Output { text, .. } = self.event() {
                seen.push_str(&text);
            }
        }
        seen
    }
}

#[test]
fn a_proxy_starts_a_broker_where_there_is_none_and_is_a_connection_to_it() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    // In a directory that does not exist yet, as a path on a machine nobody has set up would be
    let socket = socket_in(&directory.path().join("made-on-demand"), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let mut first = proxy(&socket);
    first.greet(Role::Client);
    let session = first.shell();
    first.say(&Request::Write { session: session.clone(), text: echoing("thr", "2ough") });
    assert!(first.output_with("thr2ough").contains("thr2ough"));
}

/// The first proxy's broker is the one every later proxy reaches: a session one started is there for the
/// other to ask about, which it would not be in a broker of its own.
#[test]
fn a_second_proxy_joins_the_broker_the_first_started() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let mut first = proxy(&socket);
    first.greet(Role::Lifetime);

    let mut second = proxy(&socket);
    second.greet(Role::Client);
    let session = second.shell();

    let mut third = proxy(&socket);
    third.greet(Role::Client);
    third.say(&Request::Facts { session: session.clone() });
    loop {
        match third.event() {
            Event::Facts { session: asked, .. } => {
                assert_eq!(asked, session);
                break;
            }
            Event::Failed { message, .. } => panic!("the third proxy reached another broker: {message}"),
            _ => {}
        }
    }
}

/// Proxies arriving at once on a socket nobody is listening on start one broker between them.
///
/// The broker sends its events to the client that greeted last, so each proxy greets and asks in its turn:
/// the first opens a session, and every other is asked about it — which only the broker that holds it can
/// answer.
#[test]
fn proxies_arriving_together_start_one_broker() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let mut arrived: Vec<Proxy> = (0..6).map(|_| proxy(&socket)).collect();
    let mut rest = arrived.split_off(1);
    let first = &mut arrived[0];
    first.greet(Role::Client);
    let session = first.shell();
    for (index, one) in rest.iter_mut().enumerate() {
        one.greet(Role::Client);
        one.say(&Request::Facts { session: session.clone() });
        loop {
            match one.event() {
                Event::Facts { session: asked, .. } => {
                    assert_eq!(asked, session);
                    break;
                }
                Event::Failed { message, .. } => panic!("proxy {} is on another broker: {message}", index + 1),
                _ => {}
            }
        }
    }
}

/// **A broker a proxy started does not outlive the case that had it started**, even one that is meant
/// to go on: a session asked to be kept holds its broker open after every connection has closed, which
/// is the one broker here that does not go by itself, and stands for one that cannot.
#[test]
fn a_broker_a_proxy_started_is_ended_with_the_case_that_had_it_started() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    {
        let mut through = proxy(&socket);
        through.greet(Role::Client);
        let session = through.shell();
        through.say(&Request::Keep { session, kept: true });
        loop {
            if let Event::Kept { kept: true, .. } = through.event() {
                break;
            }
        }
    }
    // Its one connection has closed and it is still there, holding what a broker holds
    let name = socket.to_str().expect("a utf-8 path");
    assert!(broker_holds(name), "the broker went with its last connection");

    drop(ending);
    // A pipe is let go a moment after its process has ended, so on Windows the wait is for it to go, counted
    let waiting_since = Instant::now();
    while cfg!(windows) && broker_holds(name) && waiting_since.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!broker_holds(name), "the broker is still holding its socket");
}

/// Whether a broker holds `socket`: on POSIX the lock beside its socket, which only a live broker keeps, and on
/// Windows a pipe that accepts a connection, the pipe being the hold there (`listening::hold` takes no part).
#[cfg(unix)]
fn broker_holds(socket: &str) -> bool {
    matches!(hold(socket), Err(Refused::Held))
}

#[cfg(windows)]
fn broker_holds(socket: &str) -> bool {
    use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};
    let name = socket.to_fs_name::<GenericFilePath>().expect("a socket name");
    Stream::connect(name).is_ok()
}

/// **A broker's hold is let go of, not only closed** (`listening::Held`). The lock is the open file's, and a
/// process forked from the broker holds a copy of it until it execs: here a child that is never let exec
/// holds that copy for as long as the case likes, and the socket is free to the next broker all the same.
#[cfg(unix)]
#[test]
fn a_hold_let_go_is_free_though_a_child_still_has_its_descriptor() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let name = socket.to_str().expect("a utf-8 path");
    let held = hold(name).expect("the socket is held");
    let mut ends = [0; 2];
    // SAFETY: `pipe` fills the two descriptors it is given
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "a pipe is made");
    // SAFETY: the child calls only async-signal-safe functions — `close`, `read` and `_exit` — before it ends
    let child = unsafe { libc::fork() };
    if child == 0 {
        // The child: holds every descriptor this process had, the hold's among them, until the pipe closes
        // SAFETY: `close`, `read` and `_exit` are async-signal-safe, the descriptors are the pipe's, and the
        // byte read into is this frame's
        unsafe {
            libc::close(ends[1]);
            let mut byte = 0u8;
            libc::read(ends[0], (&raw mut byte).cast(), 1);
            libc::_exit(0);
        }
    }
    assert!(child > 0, "a child is forked");
    // SAFETY: the read end is the child's to use; this process keeps the write end
    unsafe { libc::close(ends[0]) };

    drop(held);
    let again = hold(name);

    // The child is let go before anything is asserted, and waited for by the pid its fork returned
    // SAFETY: the write end is this process's, and the pid is the child it forked
    unsafe {
        libc::close(ends[1]);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    assert!(again.is_ok(), "a hold let go of was still held by a child that had its descriptor");
}

/// `proxy` is a word, not a position: two arguments of which the first is anything else are not a request
/// to be one, and are refused as any other mistyped invocation is.
#[test]
fn only_the_word_proxy_asks_for_one() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let status = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("serve")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the binary runs");
    assert_eq!(status.code(), Some(2));
    assert!(!socket.exists(), "a broker was started for an invocation that asked for none");
}

/// A broker started directly, as the app starts its own, with what it said first on its stdout: the line
/// that says it is listening, or the one that says why it is not.
struct Broker {
    process: Child,
    said: String,
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

fn broker(socket: &Path) -> Broker {
    broker_given(socket, None)
}

/// A broker told how long to wait for its first connection, or left to wait as long as it does in use.
fn broker_given(socket: &Path, grace: Option<Duration>) -> Broker {
    let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
    match grace {
        Some(grace) => command.env(START_GRACE_ENV, grace.as_millis().to_string()),
        // Whatever ran the suite may have set one
        None => command.env_remove(START_GRACE_ENV),
    };
    let watching = crate::reaper::watching(env!("CARGO_BIN_EXE_apack-terminal"));
    let mut process = command
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the broker starts");
    let reaper = watching.of(&process);
    let said = crate::heard::lines(process.stdout.take().expect("piped stdout")).next();
    Broker { process, said, reaper }
}

impl Broker {
    /// Waits for it to exit, answering its code and what it wrote to stderr.
    fn ended(&mut self) -> (Option<i32>, String) {
        let mut errors = String::new();
        self.process
            .stderr
            .take()
            .expect("piped stderr")
            .read_to_string(&mut errors)
            .expect("the broker's stderr reads");
        (self.process.wait().expect("the broker is waited for").code(), errors)
    }
}

/// Greets a broker over its socket as a client, answering the connection once the broker has answered.
fn greeted(socket: &Path) -> Arc<Stream> {
    let name = socket.to_str().expect("a utf-8 path").to_fs_name::<GenericFilePath>().expect("a socket name");
    let stream = Arc::new(Stream::connect(name).expect("the broker accepts"));
    let hello =
        serde_json::to_string(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client, challenge: None })
            .expect("hello");
    (&*stream).write_all(format!("{hello}\n").as_bytes()).expect("the broker reads");
    // A byte at a time and past the reader's buffer, so that nothing past the answer is taken off the
    // connection; read at the case's pace, so nothing more is read once it has been
    let answer = Heard::paced(Shared(Arc::clone(&stream)), |reader| {
        let mut answer = Vec::new();
        let mut byte = [0u8];
        while reader.get_mut().read(&mut byte).ok()? == 1 && byte[0] != b'\n' {
            answer.push(byte[0]);
        }
        Some(String::from_utf8_lossy(&answer).into_owned())
    })
    .next();
    assert!(matches!(serde_json::from_str::<Event>(&answer), Ok(Event::Hello { .. })), "{answer}");
    stream
}

/// A connection read on one thread and held on another
struct Shared(Arc<Stream>);

impl Read for Shared {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buffer)
    }
}

/// **One broker a socket.** A second broker asked to listen where one already is does not take the socket
/// from it — which would leave the first running with nothing able to reach it — but exits, with a code
/// of its own and the reason, and the first is still there.
#[test]
fn a_second_broker_on_a_socket_leaves_the_first_alone_and_says_why_it_left() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let first = broker(&socket);
    assert!(first.said.starts_with("listening "), "{}", first.said);
    let _held = greeted(&socket);

    let mut second = broker(&socket);
    assert_eq!(second.said.trim(), "failed another broker is already listening there");
    let (code, errors) = second.ended();
    assert_eq!(code, Some(HELD_EXIT));
    assert!(errors.contains("another broker is already listening there"), "{errors}");

    // And the first is still the one that answers there
    let _again = greeted(&socket);
}

/// A broker that was killed leaves its socket file behind, and nothing holding the socket: the next
/// broker replaces the file and listens.
// POSIX only: a named pipe leaves no file behind; it goes with the last handle to it.
#[cfg(unix)]
#[test]
fn a_socket_file_a_dead_broker_left_is_replaced() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let mut dead = broker(&socket);
    assert!(dead.said.starts_with("listening "), "{}", dead.said);
    dead.process.kill().expect("the broker is killed");
    dead.process.wait().expect("the broker is waited for");
    assert!(socket.exists(), "the socket file went with the broker, so this case shows nothing");

    let next = broker(&socket);
    assert!(next.said.starts_with("listening "), "{}", next.said);
    let _connection = greeted(&socket);
}

/// What crosses the socket is a user's terminals, so the socket is theirs alone to open, and a directory
/// made for it is theirs alone to look in.
// POSIX only: these are mode bits. A pipe's access is its descriptor, which `security.rs` builds and
// `tests/security.rs` reads back, and which refuses another user (`docs/architecture/windows-pipe-access.md`).
#[cfg(unix)]
#[test]
fn a_socket_and_a_directory_made_for_it_are_their_owners_alone() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let made = directory.path().join("made").join("for-it");
    let socket = made.join("broker.sock");
    let listening = broker(&socket);
    assert!(listening.said.starts_with("listening "), "{}", listening.said);
    let mode = |path: &Path| std::fs::metadata(path).expect("it is there").permissions().mode() & 0o777;
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(&made), 0o700);
    assert_eq!(mode(&directory.path().join("made")), 0o700);
}

/// A directory other users can write to is one where the socket can be swapped for theirs: a broker will
/// not listen in it, and a proxy will not look for one there.
// POSIX only: a pipe has no directory. A pipe another user made first is refused by whose it is
// (`pipe::owned_by_this_user`).
#[cfg(unix)]
#[test]
fn a_directory_others_can_write_to_is_refused() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let shared = directory.path().join("shared");
    std::fs::create_dir(&shared).expect("the directory is made");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).expect("the mode is set");
    let socket = shared.join("broker.sock");

    let mut refused = broker(&socket);
    assert!(refused.said.contains("can be written to by other users"), "{}", refused.said);
    let (code, _) = refused.ended();
    assert_eq!(code, Some(1));
    assert!(!socket.exists());

    let through = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("proxy")
        .arg(&socket)
        .stdin(Stdio::null())
        .output()
        .expect("the proxy runs");
    assert!(!through.status.success());
    assert!(String::from_utf8_lossy(&through.stderr).contains("can be written to by other users"));
}

/// A broker a proxy starts that cannot listen says why, and the proxy passes it on: the person at the far
/// end of an `ssh` reads the reason rather than that nothing was said.
#[test]
fn a_proxy_says_why_the_broker_it_started_could_not_listen() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    // A path that is already something no socket can replace
    let socket = directory.path().join("taken");
    std::fs::create_dir(&socket).expect("the directory is made");
    std::fs::write(socket.join("kept"), b"").expect("a file is put in it");

    let through = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("proxy")
        .arg(&socket)
        .stdin(Stdio::null())
        .output()
        .expect("the proxy runs");
    assert!(!through.status.success());
    let errors = String::from_utf8_lossy(&through.stderr);
    assert!(errors.contains("could not listen: "), "{errors}");
    assert!(socket.join("kept").exists(), "what was at the path was removed to make way");
}

/// **The far end finishing what it has to say is not the end of the answer.** A proxy whose input ends —
/// the whole of a conversation piped into it — goes on carrying what the broker says until the broker
/// has said it, so the last request is answered like the first.
///
/// On every platform: the caller says `done` when its input ends, and the broker answers what it owes
/// before it lets go.
#[test]
fn a_proxy_whose_input_has_ended_still_carries_the_answer_to_what_it_was_last_sent() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("proxy")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the proxy starts");
    let mut input = process.stdin.take().expect("piped stdin");
    let mut said = String::new();
    for request in [
        Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client, challenge: None },
        Request::Sessions,
        Request::Facts { session: "s-404".to_owned() },
    ] {
        said.push_str(&serde_json::to_string(&request).expect("a request serializes"));
        said.push('\n');
    }
    // Everything at once, and then nothing more ever
    input.write_all(said.as_bytes()).expect("the proxy reads");
    drop(input);

    let mut answered = String::new();
    process.stdout.take().expect("piped stdout").read_to_string(&mut answered).expect("the proxy's output reads");
    let events: Vec<Event> = answered.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    assert!(
        matches!(events.as_slice(), [Event::Hello { .. }, Event::Sessions { .. }, Event::Failed { .. }]),
        "{answered}"
    );
    assert!(process.wait().expect("the proxy is waited for").success());
}

/// **What a broker has written to a caller is the caller's to read, however long after it lets the
/// connection go.** A caller that says `done` and reads only once the broker has gone is the slowest a
/// proxy carrying the answers on can be; the answers are still there, the last one included.
///
/// On Windows, disconnecting a named pipe throws away whatever the far end has not yet read, so a
/// broker that let a caller go by disconnecting raced the caller's reads, and sometimes won.
#[test]
fn a_caller_that_reads_only_after_the_broker_let_it_go_still_reads_every_answer() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let mut broker = broker(&socket);
    assert!(broker.said.starts_with("listening "), "{}", broker.said);
    let name = socket.to_str().expect("a utf-8 path").to_fs_name::<GenericFilePath>().expect("a socket name");
    let stream = Stream::connect(name).expect("the broker accepts");
    let mut said = String::new();
    for request in [
        Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client, challenge: None },
        Request::Sessions,
        Request::Facts { session: "s-404".to_owned() },
        Request::Done,
    ] {
        said.push_str(&serde_json::to_string(&request).expect("a request serializes"));
        said.push('\n');
    }
    (&stream).write_all(said.as_bytes()).expect("the broker reads");

    // Its only connection let go and nothing running, the broker exits: nothing it does is still to come
    let (code, _) = broker.ended();
    assert_eq!(code, Some(0));

    let mut answered = String::new();
    let read = (&stream).read_to_string(&mut answered);
    let events: Vec<Event> = answered.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    assert!(
        matches!(events.as_slice(), [Event::Hello { .. }, Event::Sessions { .. }, Event::Failed { .. }]),
        "read {read:?}: {answered:?}"
    );
}

/// Stands in on `socket` for a broker on its way out, which takes a connection into its queue as it goes and
/// never says a word on it. It takes one connection for each of `readings`, reads that many bytes from it,
/// lets it go and hands over what it read; after the last it stops listening.
///
/// **Listening as a broker does** (`listening::listen`), so it is this user's to a proxy that asks: on Windows
/// a pipe made with the default descriptor is refused as somebody else's before a byte is carried.
fn leaving(socket: &Path, readings: Vec<usize>) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let listener = apack_terminal::listening::listen(socket.to_str().expect("a utf-8 path"))
        .unwrap_or_else(|refused| panic!("something listens on the socket: {refused}"));
    let (read, reads) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for reading in readings {
            let Ok(connection) = listener.accept() else { return };
            let mut bytes = vec![0; reading];
            if (&connection).read_exact(&mut bytes).is_err() || read.send(bytes).is_err() {
                return;
            }
        }
    });
    reads
}

/// What `leaving` read from the next connection it let go of, once it has.
#[track_caller]
fn let_go(reads: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
    reads
        .recv_timeout(crate::heard::QUIET)
        .unwrap_or_else(|why| panic!("no connection was made and let go of within {:?} ({why})", crate::heard::QUIET))
}

/// A greeting, as a line.
fn hello_line() -> String {
    let hello = Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client, challenge: None };
    format!("{}\n", serde_json::to_string(&hello).expect("a request serializes"))
}

/// **A broker that lets a proxy's connection go without a word never served it, and the proxy reaches
/// another.** A broker leaving — its last caller gone, or nothing having reached it within its grace —
/// takes a connection that arrives as it goes into its queue, and exits with it unread; the proxy that made
/// it went with it, saying nothing, and whoever was speaking through it found its input closed. Here
/// something listening stands for that broker twice: once before the caller has said anything, and once
/// after, and what was said is said again to the broker the proxy starts when nothing is listening.
#[test]
fn a_connection_let_go_of_without_a_word_is_made_again_and_what_was_said_is_said_again() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let hello = hello_line();
    let reads = leaving(&socket, vec![0, hello.len()]);
    let mut through = proxy(&socket);
    assert!(let_go(&reads).is_empty());

    // Said once the first connection has gone, so it is on the second, or kept and said again on it
    through.say(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client, challenge: None });
    assert_eq!(String::from_utf8_lossy(&let_go(&reads)), hello);
    let answered = through.event();
    assert!(matches!(answered, Event::Hello { .. }), "a greeting said again was answered {answered:?}");
}

/// **What was said before a word came back is said again whole, up to `KEPT_BYTES`, and past that the
/// proxy says why it cannot go on** rather than leaving its caller to find its input closed. A greeting
/// padded with blank lines, which a broker reads past, to the most that is kept and to one byte more.
#[test]
fn as_much_as_is_kept_is_said_again_and_a_caller_that_said_more_is_told_why_not() {
    for (padding, kept) in [(0, true), (1, false)] {
        let directory = tempfile::tempdir().expect("a directory for the socket");
        let socket = socket_in(directory.path(), "broker.sock");
        let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
        let mut said = hello_line();
        said.push_str(&"\n".repeat(KEPT_BYTES + padding - said.len()));
        let reads = leaving(&socket, vec![said.len()]);
        let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
            .arg("proxy")
            .arg(&socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the proxy starts");
        let mut input = process.stdin.take().expect("piped stdin");
        input.write_all(said.as_bytes()).expect("the proxy reads");
        assert_eq!(let_go(&reads).len(), said.len());
        let events = read_events(process.stdout.take().expect("piped stdout"));
        if kept {
            let answered = events.next();
            assert!(matches!(answered, Event::Hello { .. }), "{answered:?}");
            drop(input);
        } else {
            drop(input);
            let status = process.wait().expect("the proxy is waited for");
            let mut why = String::new();
            process.stderr.take().expect("piped stderr").read_to_string(&mut why).expect("its errors read");
            assert!(!status.success() && why.contains("more had been said"), "{status}: {why:?}");
        }
        let _ = process.kill();
        let _ = process.wait();
    }
}

/// **A socket where every connection is let go of without a word is given up on, saying so**, once as many
/// have been made as a broker is looked for: whatever is listening there is not a broker that answers, and
/// reaching it again for ever would be a proxy that never ends.
#[test]
fn a_proxy_whose_connections_are_all_let_go_of_without_a_word_gives_up_saying_why() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let reads = leaving(&socket, vec![0; LOOKS + 1]);
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("proxy")
        .arg(&socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the proxy starts");
    for _ in 0..LOOKS {
        let_go(&reads);
    }
    let status = process.wait().expect("the proxy is waited for");
    let mut why = String::new();
    process.stderr.take().expect("piped stderr").read_to_string(&mut why).expect("its errors read");
    assert!(!status.success() && why.contains(&format!("let {LOOKS} connections go")), "{status}: {why:?}");
    assert!(reads.try_recv().is_err(), "a connection was made after the proxy gave up");
}

/// A caller that says nothing and is done is answered by the connection being let go of, and the proxy
/// leaves with it: nothing was said for a broker to have left unanswered.
#[test]
fn a_proxy_whose_caller_said_nothing_leaves_once_it_is_done() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let _ending = crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("proxy")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the proxy starts");
    let mut said = String::new();
    process.stdout.take().expect("piped stdout").read_to_string(&mut said).expect("its output reads");
    assert!(said.is_empty(), "{said:?}");
    assert!(process.wait().expect("the proxy is waited for").success());
}

/// A broker is started to be connected to. One that nothing ever connects to — whatever started it died
/// first — has no connection closing to tell it to go, and goes of its own accord once the time it
/// allows for that first connection has passed.
#[test]
fn a_broker_nothing_ever_connects_to_exits() {
    // Long enough that leaving at once and leaving when it is over cannot be mistaken for each other
    let grace = Duration::from_millis(500);
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = socket_in(directory.path(), "broker.sock");
    let started = Instant::now();
    let mut alone = broker_given(&socket, Some(grace));
    assert!(alone.said.starts_with("listening "), "{}", alone.said);
    let (code, errors) = alone.ended();
    assert_eq!(code, Some(0));
    assert!(started.elapsed() >= grace, "it left after {:?}, before anything could have connected", started.elapsed());
    // What a starter that connects too late is left to read, in place of a refused connection alone
    assert!(errors.contains("nothing connected within 500ms of listening"), "{errors:?}");
}

/// A broker waits as long as it does in use unless it is told a number of milliseconds.
#[test]
fn the_grace_is_the_one_in_use_unless_a_number_of_milliseconds_is_given() {
    assert_eq!(start_grace(None), START_GRACE);
    assert_eq!(start_grace(Some("250")), Duration::from_millis(250));
    assert_eq!(start_grace(Some("soon")), START_GRACE);
    assert_eq!(start_grace(Some("")), START_GRACE);
}

/// A broker whose socket has been taken from under it cannot be connected to by anything, itself
/// included: with nothing ever having connected, it goes when the time it allows for a first connection
/// has passed, as one that could still be reached does.
// POSIX only: it takes the broker's socket file away, and a named pipe has no file to take.
#[cfg(unix)]
#[test]
fn a_broker_nothing_can_reach_exits() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    // Its socket is gone well inside this, so what it finds when the grace is over is nothing to reach
    let mut alone = broker_given(&socket, Some(Duration::from_secs(2)));
    assert!(alone.said.starts_with("listening "), "{}", alone.said);
    std::fs::remove_file(&socket).expect("the socket's file is removed");
    let (code, _) = alone.ended();
    assert_eq!(code, Some(0));
}

/// A socket named with no directory is in the directory the broker was started in, and that directory
/// is asked what any other is: one other users can write to is refused.
// POSIX only: a pipe has no directory.
#[cfg(unix)]
#[test]
fn a_socket_named_without_a_directory_is_checked_where_it_would_be_made() {
    let directory = tempfile::tempdir().expect("a directory");
    let shared = directory.path().join("shared");
    std::fs::create_dir(&shared).expect("the directory is made");
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).expect("the mode is set");
    let started = |within: &Path| {
        Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
            .arg("bare.sock")
            .current_dir(within)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the broker starts")
    };
    let said = |mut process: Child| {
        let line = crate::heard::lines(process.stdout.take().expect("piped stdout")).next();
        let _ = process.kill();
        let _ = process.wait();
        line
    };
    assert!(said(started(&shared)).contains("can be written to by other users"));
    assert!(!shared.join("bare.sock").exists());
    // And in one that is its user's alone, it listens
    assert!(said(started(directory.path())).starts_with("listening "));
}

/// **A proxy told no socket uses the one place a user is known to have**: under the runtime directory
/// the system gives them, or failing that under their home, in a directory made for them alone. So
/// `ssh host apack-terminal proxy` needs nothing set up on the host, and two proxies told nothing meet.
// POSIX only: the default socket and its directory are checked for Unix mode bits. On Windows the default is
// a pipe, and the case after this one is its own.
#[cfg(unix)]
#[test]
fn a_proxy_told_no_socket_uses_the_users_own() {
    let home = tempfile::tempdir().expect("a home directory");
    let runtime = tempfile::tempdir().expect("a runtime directory");
    let ending =
        |socket: std::path::PathBuf| crate::reaper::ending_with_the_case(&socket, env!("CARGO_BIN_EXE_apack-terminal"));
    let _endings = [
        ending(home.path().join(".apack-terminal").join("broker.sock")),
        ending(runtime.path().join("apack-terminal").join("broker.sock")),
    ];
    let through = |runtime_directory: Option<&Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
        command.arg("proxy").env("HOME", home.path()).stdin(Stdio::piped()).stdout(Stdio::piped());
        match runtime_directory {
            Some(directory) => command.env("XDG_RUNTIME_DIR", directory),
            None => command.env_remove("XDG_RUNTIME_DIR"),
        };
        let mut process = command.spawn().expect("the proxy starts");
        let input = process.stdin.take().expect("piped stdin");
        let events = read_events(process.stdout.take().expect("piped stdout"));
        Proxy { process, input, events }
    };
    let mode = |path: &Path| std::fs::metadata(path).expect("it is there").permissions().mode() & 0o777;

    let mut first = through(None);
    first.greet(Role::Client);
    let made = home.path().join(".apack-terminal");
    assert!(made.join("broker.sock").exists());
    assert_eq!(mode(&made), 0o700);
    // A second told nothing reaches the first's broker
    let session = first.shell();
    let mut second = through(None);
    second.greet(Role::Client);
    second.say(&Request::Facts { session: session.clone() });
    loop {
        match second.event() {
            Event::Facts { session: asked, .. } => break assert_eq!(asked, session),
            Event::Failed { message, .. } => panic!("the second proxy reached another broker: {message}"),
            _ => {}
        }
    }

    // Where the system gives the user a runtime directory, that is the place
    let mut third = through(Some(runtime.path()));
    third.greet(Role::Client);
    assert!(runtime.path().join("apack-terminal").join("broker.sock").exists());
    assert_eq!(mode(&runtime.path().join("apack-terminal")), 0o700);
}

/// **A proxy told no socket on Windows uses the pipe named for its user**, so `apack-terminal proxy` needs
/// nothing set up there either, and two proxies told nothing meet. A pipe's name is the whole machine's, so the
/// user's SID is what is in it (`listening::default_socket`). Whatever broker is already listening there is the
/// one reached, so the case's job ends only a broker this case started.
#[cfg(windows)]
#[test]
fn a_proxy_told_no_socket_uses_the_pipe_named_for_its_user() {
    let named = apack_terminal::listening::default_socket().expect("a user has a pipe of their own");
    let _ending = crate::reaper::ending_with_the_case(Path::new(&named), env!("CARGO_BIN_EXE_apack-terminal"));
    let told_nothing = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
        command.arg("proxy").stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut process = crate::reaper::spawn_contained(&mut command).expect("the proxy starts");
        let input = process.stdin.take().expect("piped stdin");
        let events = read_events(process.stdout.take().expect("piped stdout"));
        Proxy { process, input, events }
    };

    let mut first = told_nothing();
    first.greet(Role::Client);
    let session = first.shell();
    let mut second = told_nothing();
    second.greet(Role::Client);
    second.say(&Request::Facts { session: session.clone() });
    loop {
        match second.event() {
            Event::Facts { session: asked, .. } => break assert_eq!(asked, session),
            Event::Failed { message, .. } => panic!("the second proxy reached another broker: {message}"),
            _ => {}
        }
    }
}

/// **A connection whose far end has said all it will can still be hung up on.** A caller that has
/// finished speaking closes the side it sends on and may go on being owed answers; one that then stops
/// reading has whatever writes to it parked, and hanging up is what lets that go. The far end's half
/// being closed already is the state a hang-up most often finds a connection in.
// POSIX only: a named pipe cannot be closed one way, so its far end is never in this state.
#[cfg(unix)]
#[test]
fn a_connection_whose_far_end_has_stopped_sending_is_hung_up_on_and_a_write_to_it_ends() {
    use apack_terminal::listening::{hang_up, let_go};

    // Letting a connection go is hanging up on it where a hang-up leaves what was written, which a socket's does
    for (ending, end) in [("hung up on", hang_up as fn(&Stream) -> bool), ("let go", |stream| let_go(stream, || {}))] {
        ends_a_write_to_a_far_end_that_stopped_sending(ending, end);
    }
}

#[cfg(unix)]
fn ends_a_write_to_a_far_end_that_stopped_sending(ending: &str, end: fn(&Stream) -> bool) {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, ToFsName, prelude::*};

    let directory = tempfile::tempdir().expect("a directory for the socket");
    let path = directory.path().join("heard.sock");
    let name = path.to_str().expect("a utf-8 path").to_fs_name::<GenericFilePath>().expect("a socket name");
    let listener = ListenerOptions::new().name(name).create_sync().expect("a socket to listen on");
    let far = UnixStream::connect(&path).expect("the far end connects");
    let near = listener.accept().expect("and is accepted");

    far.shutdown(std::net::Shutdown::Write).expect("the far end closes the side it sends on");
    let mut heard = [0u8; 1];
    assert_eq!((&near).read(&mut heard).expect("the near end reads"), 0, "the near end hears that it finished");

    assert!(end(&near), "a connection half closed by its far end could not be {ending}");
    // Far more than a socket holds, to a far end that reads none of it: a write left open would park here
    let written = (&near).write_all(&vec![0u8; 8 << 20]);
    assert!(written.is_err(), "megabytes were written to a connection that had been {ending}");
    drop(far);
}
