//! Sessions that outlast what started them: one kept past the app's hold on the broker, one whose screen
//! is picked up from its recording by the session that replaces it, and a recording looked back through.
//!
//! Against the real binary and a real shell, over connections that are hung up the way an app quitting
//! hangs them up.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role, SessionSummary};
use apack_terminal::view::wire::{Frame, Incoming, Row, ToBroker, ToView, read_message};

const PATIENCE: Duration = Duration::from_secs(20);

struct Broker {
    process: Child,
    socket: PathBuf,
    directory: tempfile::TempDir,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl Broker {
    fn start() -> Self {
        let directory = tempfile::tempdir().expect("a directory for the socket");
        let socket = directory.path().join("broker.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
        command.arg(&socket).stdout(Stdio::piped());
        // A session of its own, so a signal from the broker cannot leave this test (`tests/broker.rs`)
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: runs in the child between fork and exec, and `setsid` touches nothing this process owns
            unsafe {
                command.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) })
            };
        }
        let mut process = command.spawn().expect("the broker starts");
        let mut ready = String::new();
        BufReader::new(process.stdout.as_mut().expect("piped stdout")).read_line(&mut ready).expect("a ready line");
        assert!(ready.starts_with("listening "), "{ready:?}");
        Self { process, socket, directory }
    }

    /// Whether the broker's process ends within the time a working one takes to.
    fn exits(&mut self) -> bool {
        let waiting_since = Instant::now();
        while waiting_since.elapsed() < PATIENCE {
            if self.process.try_wait().expect("the broker can be waited for").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn recording(&self, name: &str) -> PathBuf {
        self.directory.path().join("recordings").join(name)
    }
}

/// One of the app's connections: the hold on the broker, or the one that asks for sessions.
struct Connection {
    stream: UnixStream,
    events: Receiver<Event>,
}

impl Connection {
    fn greeting(broker: &Broker, role: Role) -> Self {
        let stream = UnixStream::connect(&broker.socket).expect("the broker accepts");
        let (sender, events) = channel();
        std::thread::spawn({
            let stream = stream.try_clone().expect("a second handle");
            move || {
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else { return };
                    if let Ok(event) = serde_json::from_str::<Event>(&line)
                        && sender.send(event).is_err()
                    {
                        return;
                    }
                }
            }
        });
        let connection = Self { stream, events };
        connection.say(&Request::Hello { protocol: PROTOCOL_VERSION, role });
        assert!(matches!(connection.event(), Event::Hello { .. }));
        connection
    }

    fn say(&self, request: &Request) {
        let mut line = serde_json::to_string(request).expect("a request serializes");
        line.push('\n');
        (&self.stream).write_all(line.as_bytes()).expect("the broker reads");
    }

    fn event(&self) -> Event {
        self.events.recv_timeout(PATIENCE).expect("the broker answers")
    }

    /// The next event `pick` takes, past whatever else is told on the way.
    fn answer<T>(&self, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        loop {
            let event = self.event();
            if let Event::Failed { message, .. } = &event {
                panic!("the broker refused: {message}");
            }
            if let Some(found) = pick(event) {
                return found;
            }
        }
    }

    fn shell(&self, record: Option<&Path>, restore: bool) -> String {
        self.say(&Request::Spawn {
            cols: 60,
            rows: 8,
            shell: Some("/bin/sh".to_owned()),
            args: Vec::new(),
            env: vec![("PS1".to_owned(), "$ ".to_owned()), ("ENV".to_owned(), String::new())],
            unset: Vec::new(),
            cwd: None,
            colors: None,
            output: false,
            record: record.map(|path| path.to_str().expect("a utf-8 path").to_owned()),
            restore,
            images: false,
        });
        self.answer(|event| if let Event::Spawned { session, .. } = event { Some(session) } else { None })
    }

    fn sessions(&self) -> Vec<SessionSummary> {
        self.say(&Request::Sessions);
        self.answer(|event| if let Event::Sessions { sessions } = event { Some(sessions) } else { None })
    }

    /// Closes the connection, as its process ending would.
    fn hang_up(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// A view's connection, and the screen it has been sent.
struct View {
    stream: UnixStream,
    incoming: Receiver<ToView>,
    rows: BTreeMap<u64, Row>,
    last: Option<Frame>,
}

impl View {
    fn attached(broker: &Broker, session: &str) -> Self {
        let stream = UnixStream::connect(&broker.socket).expect("the broker accepts");
        let mut hello = serde_json::to_string(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View })
            .expect("a request serializes");
        hello.push('\n');
        (&stream).write_all(hello.as_bytes()).expect("the broker reads");
        let (sender, incoming) = channel();
        std::thread::spawn({
            let stream = stream.try_clone().expect("a second handle");
            move || {
                let mut reader = BufReader::new(stream);
                let mut hello = String::new();
                if reader.read_line(&mut hello).is_err() {
                    return;
                }
                while let Incoming::Message(body) = read_message(&mut reader) {
                    let Ok(message) = ToView::decode(&body) else { return };
                    if sender.send(message).is_err() {
                        return;
                    }
                }
            }
        });
        let view = Self { stream, incoming, rows: BTreeMap::new(), last: None };
        view.send(&ToBroker::Attach { channel: 1, session: session.to_owned() });
        view
    }

    fn send(&self, message: &ToBroker) {
        (&self.stream).write_all(&message.encode()).expect("the broker reads");
    }

    fn step(&mut self) {
        let message = self.incoming.recv_timeout(PATIENCE).expect("the broker sends");
        if let ToView::Frame(frame) = message {
            if self.last.as_ref().is_some_and(|last| last.epoch != frame.epoch) {
                self.rows.clear();
            }
            if let Some(stale) = frame.stale_from {
                self.rows.retain(|line, _| *line < stale || *line >= frame.first_active);
            }
            for row in &frame.block.rows {
                self.rows.insert(row.line, row.clone());
            }
            self.send(&ToBroker::Acknowledge { channel: frame.channel, sequence: frame.sequence });
            self.last = Some(frame);
        }
    }

    /// Every line this view holds, oldest first.
    fn held(&self) -> String {
        self.rows.values().map(text).collect::<Vec<_>>().join("\n")
    }

    fn screen(&self) -> String {
        let Some(frame) = &self.last else { return String::new() };
        (frame.first_active..frame.first_active + u64::from(frame.screen_rows))
            .map(|line| self.rows.get(&line).map(text).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn until(&mut self, what: &str, mut holds: impl FnMut(&Self) -> bool) {
        let waiting_since = Instant::now();
        while !holds(self) {
            assert!(waiting_since.elapsed() < PATIENCE, "never {what}:\n{}", self.held());
            self.step();
        }
    }

    fn until_shows(&mut self, expected: &str) {
        self.until(&format!("showed {expected:?}"), |view| view.screen().contains(expected));
    }

    fn type_line(&self, line: &str) {
        for character in line.chars() {
            self.send(&ToBroker::Key {
                channel: 1,
                action: 0,
                modifiers: 0,
                code: String::new(),
                text: character.to_string(),
                unshifted: u32::from(character),
            });
        }
        self.send(&ToBroker::Key {
            channel: 1,
            action: 0,
            modifiers: 0,
            code: "Enter".to_owned(),
            text: String::new(),
            unshifted: 0,
        });
    }

    fn hang_up(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn text(row: &Row) -> String {
    let mut text = String::new();
    for glyph in &row.glyphs {
        while text.chars().count() < usize::from(glyph.column) {
            text.push(' ');
        }
        text.push_str(&glyph.text);
    }
    text
}

/// The app quits: the session it asked to keep is still running and the one it did not is gone. The next
/// app to start finds the kept one, sees what it showed and types at it; and once that session ends with
/// nobody connected, so does the broker.
#[test]
fn a_kept_session_outlives_the_app_and_is_found_by_the_next() {
    let mut broker = Broker::start();
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);
    let kept = client.shell(None, false);
    let ended = client.shell(None, false);
    client.say(&Request::Keep { session: kept.clone(), kept: true });
    client.answer(|event| matches!(event, Event::Kept { kept: true, .. }).then_some(()));
    let mut view = View::attached(&broker, &kept);
    view.until_shows("$ ");
    view.type_line("echo be$((1+1))fore");
    view.until_shows("be2fore");

    view.hang_up();
    client.hang_up();
    hold.hang_up();

    // A new app: it holds the broker again and asks what is there
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);
    let found = client.sessions();
    assert_eq!(found.iter().map(|one| one.session.as_str()).collect::<Vec<_>>(), [kept.as_str()], "not {ended}");
    assert!(found[0].kept);
    assert_eq!((found[0].cols, found[0].rows), (60, 8));
    let mut view = View::attached(&broker, &kept);
    view.until_shows("be2fore");
    view.type_line("echo af$((1+2))ter");
    view.until_shows("af3ter");

    // No longer kept, it ends with the app like any other, and the broker with it
    client.say(&Request::Keep { session: kept, kept: false });
    client.answer(|event| matches!(event, Event::Kept { kept: false, .. }).then_some(()));
    view.hang_up();
    client.hang_up();
    hold.hang_up();
    assert!(broker.exits(), "the broker stayed with nothing to stay for");
}

/// A kept session's own process ending, with nobody connected, is the broker's last reason to stay.
#[test]
fn a_broker_holding_only_a_kept_session_ends_when_it_does() {
    let mut broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let kept = client.shell(None, false);
    client.say(&Request::Keep { session: kept.clone(), kept: true });
    client.answer(|event| matches!(event, Event::Kept { .. }).then_some(()));
    let mut view = View::attached(&broker, &kept);
    view.until_shows("$ ");
    // Asked to end itself a moment after everyone has gone
    view.type_line("sleep 1; exit");
    view.hang_up();
    client.hang_up();
    assert!(broker.exits(), "the broker stayed after its last session ended");
}

/// A terminal reopened where it left off: the new session begins with the screen and history the last
/// one's recording holds, its shell starts below them, and the recording carries on in the same file.
#[test]
fn a_session_begins_with_the_screen_its_recording_holds() {
    let broker = Broker::start();
    let recording = broker.recording("terminal-1");
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);
    let first = client.shell(Some(&recording), false);
    assert!(client.sessions()[0].recorded);
    let mut view = View::attached(&broker, &first);
    view.until_shows("$ ");
    view.type_line("for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo row-$i-of-the-first; done");
    view.until_shows("row-12-of-the-first");
    // Left inside an application on the alternate screen, with the mouse reported
    view.type_line("printf \"\\033[?1049h\\033[?1000hAPP-$((1+1))-SHOWN\"; sleep 100");
    view.until_shows("APP-2-SHOWN");
    view.hang_up();
    client.say(&Request::Kill { session: first });
    client.answer(|event| matches!(event, Event::Exited { .. }).then_some(()));

    let second = client.shell(Some(&recording), true);
    let mut view = View::attached(&broker, &second);
    view.until_shows("$ ");
    // The new shell's prompt is under what the first one printed, not inside its application
    assert!(!view.screen().contains("APP-2-SHOWN"), "{}", view.screen());
    assert!(view.screen().contains("row-12-of-the-first"), "{}", view.screen());
    assert!(!view.last.as_ref().expect("a frame").mouse);
    // And what scrolled off in the first session is history in the second
    let frame = view.last.clone().expect("a frame");
    assert!(frame.scrollback >= 6, "{} lines of history", frame.scrollback);
    view.send(&ToBroker::Fetch {
        channel: 1,
        request: 1,
        epoch: frame.epoch,
        first: frame.first_active - u64::from(frame.scrollback),
        count: frame.scrollback,
    });
    let fetched = loop {
        let message = view.incoming.recv_timeout(PATIENCE).expect("the broker sends");
        if let ToView::Rows { block, .. } = message {
            break block.rows.iter().map(text).collect::<Vec<_>>().join("\n");
        }
    };
    assert!(fetched.contains("row-1-of-the-first"), "{fetched}");

    // The second session is recorded after the first, in the one file: a third begins with both
    view.type_line("echo from-the-se$((1+1))cond");
    view.until_shows("from-the-se2cond");
    view.hang_up();
    client.say(&Request::Kill { session: second });
    client.answer(|event| matches!(event, Event::Exited { .. }).then_some(()));
    let third = client.shell(Some(&recording), true);
    let mut view = View::attached(&broker, &third);
    view.until_shows("$ ");
    assert!(view.screen().contains("from-the-se2cond"), "{}", view.screen());
    drop(hold);
}

/// A recording opened to look back through: it shows its end, then the moment asked for, to a view
/// attached like any other — and what that view types goes nowhere.
#[test]
fn a_recording_is_looked_back_through_as_a_session() {
    let broker = Broker::start();
    let recording = broker.recording("terminal-1");
    let client = Connection::greeting(&broker, Role::Client);
    let live = client.shell(Some(&recording), false);
    let mut view = View::attached(&broker, &live);
    view.until_shows("$ ");
    view.type_line("echo ear$((1+1))ly");
    view.until_shows("ear2ly");
    // Recordings are stamped in milliseconds: far enough apart that a moment between the two exists
    std::thread::sleep(Duration::from_millis(60));
    let between = apack_terminal::recording::now();
    std::thread::sleep(Duration::from_millis(60));
    view.type_line("echo la$((2+2))ter");
    view.until_shows("la4ter");

    client.say(&Request::Rewind { recording: recording.to_str().expect("a utf-8 path").to_owned(), cols: 60, rows: 8 });
    let (rewound, first, last) = client.answer(|event| {
        if let Event::Rewound { session, first, last } = event { Some((session, first, last)) } else { None }
    });
    assert!(first < between && between < last, "{first} {between} {last}");
    let mut looking = View::attached(&broker, &rewound);
    looking.until_shows("la4ter");

    client.say(&Request::Seek { session: rewound.clone(), at: between });
    client.answer(|event| matches!(event, Event::Sought { .. }).then_some(()));
    looking.until("showed the earlier moment", |view| {
        view.screen().contains("ear2ly") && !view.screen().contains("la4ter")
    });
    // Typed at, it does nothing, and the live session is not what it reached
    looking.type_line("echo no$((1+1))where");
    client.say(&Request::Seek { session: rewound.clone(), at: last });
    client.answer(|event| matches!(event, Event::Sought { .. }).then_some(()));
    looking.until_shows("la4ter");
    assert!(!looking.screen().contains("no2where"), "{}", looking.screen());
    assert!(!view.screen().contains("no2where"));

    // It is not a session with a process, and ending it ends no shell
    assert_eq!(client.sessions().iter().map(|one| one.session.as_str()).collect::<Vec<_>>(), [live.as_str()]);
    client.say(&Request::Kill { session: rewound });
    view.type_line("echo still-he$((1+1))re");
    view.until_shows("still-he2re");
}
