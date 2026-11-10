//! `apack-terminal proxy`: a broker reached through a process's stdin and stdout.
//!
//! Driven as `ssh` or `docker exec` would carry it — a child whose pipes are the connection — against the
//! real binary: that a broker is started when there is none, that the pipe is the broker's protocol byte
//! for byte, and that a second proxy joins the first's broker rather than starting another.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role};

const PATIENCE: Duration = Duration::from_secs(20);

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
        assert!(matches!(self.event(), Event::Hello { .. }));
    }

    fn shell(&mut self) -> String {
        self.say(&Request::Spawn {
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".to_owned()),
            args: Vec::new(),
            env: vec![("PS1".to_owned(), "$ ".to_owned())],
            unset: Vec::new(),
            cwd: None,
            colors: None,
            output: true,
            record: None,
            restore: false,
            images: false,
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
    let socket = directory.path().join("made-on-demand").join("broker.sock");
    let mut first = proxy(&socket);
    first.greet(Role::Client);
    let session = first.shell();
    first.say(&Request::Write { session: session.clone(), text: "echo thr$((1+1))ough\n".to_owned() });
    assert!(first.output_with("thr2ough").contains("thr2ough"));
}

/// The first proxy's broker is the one every later proxy reaches: a session one started is there for the
/// other to ask about, which it would not be in a broker of its own.
#[test]
fn a_second_proxy_joins_the_broker_the_first_started() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
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
    let socket = directory.path().join("broker.sock");
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

/// A proxy that died while starting a broker leaves its claim behind, and nothing listening. The next
/// proxy waits for the broker that claim promises, and when none comes takes the claim over and starts
/// one — rather than waiting on a file nobody is going to remove.
#[test]
fn a_claim_left_by_a_proxy_that_died_is_taken_over() {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let claim = directory.path().join("broker.sock.starting");
    std::fs::write(&claim, b"").expect("the claim is left");
    let mut late = proxy(&socket);
    late.greet(Role::Client);
    assert!(!claim.exists(), "the claim outlived the broker it was for");
}
