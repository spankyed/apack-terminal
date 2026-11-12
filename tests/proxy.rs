//! `apack-terminal proxy`: a broker reached through a process's stdin and stdout.
//!
//! Driven as `ssh` or `docker exec` would carry it — a child whose pipes are the connection — against the
//! real binary: that a broker is started when there is none, that the pipe is the broker's protocol byte
//! for byte, and that a second proxy joins the first's broker rather than starting another.

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use apack_terminal::listening::{HELD_EXIT, Refused, START_GRACE, START_GRACE_ENV, hold, start_grace};
use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role};

/// How long a wait in this file goes on with nothing arriving. **Shorter than the runner's deadline for a
/// case** (`.config/nextest.toml`), so a wait that runs out fails saying what it was waiting for, where
/// one as long as the deadline is ended by the runner saying nothing.
const PATIENCE: Duration = Duration::from_secs(15);

struct Proxy {
    process: Child,
    input: ChildStdin,
    events: Receiver<Event>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

fn read_events(output: ChildStdout) -> Receiver<Event> {
    let (sender, events) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { return };
            if let Ok(event) = serde_json::from_str::<Event>(&line)
                && sender.send(event).is_err()
            {
                return;
            }
        }
    });
    events
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
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg("proxy")
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the proxy starts");
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

    fn event(&self) -> Event {
        self.events.recv_timeout(PATIENCE).expect("the broker answers through the proxy")
    }

    fn greet(&mut self, role: Role) {
        self.say(&Request::Hello { protocol: PROTOCOL_VERSION, role });
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
#[cfg(unix)]
#[test]
fn a_broker_a_proxy_started_is_ended_with_the_case_that_had_it_started() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
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
    assert!(matches!(hold(name), Err(Refused::Held)), "the broker went with its last connection");

    drop(ending);
    assert!(hold(name).is_ok(), "the broker is still holding its socket");
}

/// `proxy` is a word, not a position: two arguments of which the first is anything else are not a request
/// to be one, and are refused as any other mistyped invocation is.
#[cfg(unix)]
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
    let mut process = command
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the broker starts");
    let mut said = String::new();
    BufReader::new(process.stdout.take().expect("piped stdout"))
        .read_line(&mut said)
        .expect("the broker's stdout reads");
    let reaper = crate::reaper::watching(&process, env!("CARGO_BIN_EXE_apack-terminal"));
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
fn greeted(socket: &Path) -> Stream {
    let name = socket.to_str().expect("a utf-8 path").to_fs_name::<GenericFilePath>().expect("a socket name");
    let stream = Stream::connect(name).expect("the broker accepts");
    let hello =
        serde_json::to_string(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client }).expect("hello");
    (&stream).write_all(format!("{hello}\n").as_bytes()).expect("the broker reads");
    // A byte at a time, so that nothing past the answer is taken off the connection
    let mut answer = Vec::new();
    let mut byte = [0u8];
    while (&stream).read(&mut byte).expect("the broker answers") == 1 && byte[0] != b'\n' {
        answer.push(byte[0]);
    }
    let answer = String::from_utf8_lossy(&answer);
    assert!(matches!(serde_json::from_str::<Event>(&answer), Ok(Event::Hello { .. })), "{answer}");
    stream
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
#[cfg(unix)]
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
/// **POSIX only.** A named pipe has one end to close, so a proxy on Windows whose input ends leaves at
/// once (`proxy::finish_sending`), and what the broker had still to say is not carried.
#[cfg(unix)]
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
        Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client },
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
    let (code, _) = alone.ended();
    assert_eq!(code, Some(0));
    assert!(started.elapsed() >= grace, "it left after {:?}, before anything could have connected", started.elapsed());
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
        let mut line = String::new();
        BufReader::new(process.stdout.take().expect("piped stdout")).read_line(&mut line).expect("stdout reads");
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

/// **A connection whose far end has said all it will can still be hung up on.** A caller that has
/// finished speaking closes the side it sends on and may go on being owed answers; one that then stops
/// reading has whatever writes to it parked, and hanging up is what lets that go. The far end's half
/// being closed already is the state a hang-up most often finds a connection in.
#[cfg(unix)]
#[test]
fn a_connection_whose_far_end_has_stopped_sending_is_hung_up_on_and_a_write_to_it_ends() {
    use apack_terminal::listening::hang_up;
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

    assert!(hang_up(&near), "a connection half closed by its far end could not be hung up on");
    // Far more than a socket holds, to a far end that reads none of it: a write left open would park here
    let written = (&near).write_all(&vec![0u8; 8 << 20]);
    assert!(written.is_err(), "megabytes were written to a connection that had been hung up on");
    drop(far);
}
