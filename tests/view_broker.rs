//! A view driven as the window will drive it: over a real socket, against the real binary and a real shell.
//!
//! Everything a view is sent is proven without a socket in `view_frames.rs` and `view_model.rs`. What is
//! proven here is the part that needs one: that a connection greeting as a view is served, that its keys
//! reach a program and the program's output comes back as rows, and that a view coming, going or
//! misbehaving costs the session nothing.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role};
use apack_terminal::view::wire::{Frame, Incoming, Position, Row, Span, ToBroker, ToView, read_message};

/// How long a case waits for something a working broker does at once. A bound, so a broken one fails
/// rather than hangs; generous, so a busy machine does not.
const PATIENCE: Duration = Duration::from_secs(20);

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
    Broker { process, socket, _directory: directory }
}

fn connect(broker: &Broker) -> Arc<Stream> {
    let name = broker.socket.to_str().expect("a utf-8 path").to_fs_name::<GenericFilePath>().expect("a socket name");
    Arc::new(Stream::connect(name).expect("the broker accepts"))
}

fn say(stream: &Arc<Stream>, request: &Request) {
    let mut line = serde_json::to_string(request).expect("a request serializes");
    line.push('\n');
    (&**stream).write_all(line.as_bytes()).expect("the broker reads");
}

/// The app's own connection: it asks for sessions and hears what became of them.
struct Client {
    stream: Arc<Stream>,
    events: Receiver<Event>,
}

impl Client {
    fn greeting(broker: &Broker) -> Self {
        let stream = connect(broker);
        say(&stream, &Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client });
        let (sender, events) = channel();
        std::thread::spawn({
            let stream = Arc::clone(&stream);
            move || {
                for line in BufReader::new(&*stream).lines() {
                    let Ok(line) = line else { return };
                    if let Ok(event) = serde_json::from_str::<Event>(&line)
                        && sender.send(event).is_err()
                    {
                        return;
                    }
                }
            }
        });
        Self { stream, events }
    }

    /// The next event that is not about how the stream is flowing.
    fn event(&self) -> Event {
        self.events.recv_timeout(PATIENCE).expect("the broker answers")
    }

    /// A shell whose bytes nobody reads: it is there to be viewed.
    fn shell(&self) -> String {
        self.shell_taking(false)
    }

    /// The same, saying whether a program in it may place images.
    fn shell_taking(&self, images: bool) -> String {
        say(
            &self.stream,
            &Request::Spawn {
                cols: 60,
                rows: 8,
                shell: Some("/bin/sh".to_owned()),
                args: Vec::new(),
                // A prompt that is the same on every machine, and no line editor to redraw it
                env: vec![("PS1".to_owned(), "$ ".to_owned()), ("ENV".to_owned(), String::new())],
                unset: Vec::new(),
                cwd: None,
                colors: None,
                output: false,
                record: None,
                restore: false,
                images,
            },
        );
        loop {
            match self.event() {
                Event::Hello { .. } => {}
                Event::Spawned { session, .. } => return session,
                other => panic!("expected a session, was told {other:?}"),
            }
        }
    }
}

/// A view's connection, and what the view holds.
struct View {
    stream: Arc<Stream>,
    incoming: Receiver<ToView>,
    rows: BTreeMap<u64, Row>,
    last: Option<Frame>,
    ended: bool,
}

impl View {
    fn greeting(broker: &Broker) -> Self {
        let stream = connect(broker);
        say(&stream, &Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View });
        let (sender, incoming) = channel();
        std::thread::spawn({
            let stream = Arc::clone(&stream);
            move || {
                let mut reader = BufReader::new(&*stream);
                // The handshake's answer is the one JSON line this connection is ever sent
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
        Self { stream, incoming, rows: BTreeMap::new(), last: None, ended: false }
    }

    fn send(&self, message: &ToBroker) {
        (&*self.stream).write_all(&message.encode()).expect("the broker reads");
    }

    fn attached(broker: &Broker, session: &str) -> Self {
        let view = Self::greeting(broker);
        view.send(&ToBroker::Attach { channel: 1, session: session.to_owned() });
        view
    }

    /// Takes the next message, applying a frame and acknowledging it as a view that has drawn it would.
    fn step(&mut self) -> ToView {
        let message = self.incoming.recv_timeout(PATIENCE).expect("the broker sends");
        match &message {
            ToView::Frame(frame) => {
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
                self.last = Some(frame.clone());
            }
            ToView::Ended { .. } => self.ended = true,
            _ => {}
        }
        message
    }

    fn screen(&self) -> String {
        let Some(frame) = &self.last else { return String::new() };
        (frame.first_active..frame.first_active + u64::from(frame.screen_rows))
            .map(|line| self.rows.get(&line).map(text).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Frames until the screen shows `expected`.
    fn until_shows(&mut self, expected: &str) {
        while !self.screen().contains(expected) {
            assert!(!self.ended, "the session ended without showing {expected:?}:\n{}", self.screen());
            self.step();
        }
    }

    /// Messages until one is the answer being waited for.
    fn answer<T>(&mut self, mut pick: impl FnMut(&ToView) -> Option<T>) -> T {
        loop {
            let message = self.step();
            if let Some(found) = pick(&message) {
                return found;
            }
        }
    }

    fn key(&self, code: &str, typed: &str) {
        let unshifted = typed.chars().next().map_or(0, u32::from);
        self.send(&ToBroker::Key {
            channel: 1,
            action: 0,
            modifiers: 0,
            code: code.to_owned(),
            text: typed.to_owned(),
            unshifted,
        });
    }

    /// Types a line a key at a time, as a keyboard does, and presses Enter.
    fn type_line(&self, line: &str) {
        for character in line.chars() {
            self.key("", &character.to_string());
        }
        self.key("Enter", "");
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

#[test]
fn a_view_draws_a_shell_and_its_keys_reach_it() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);

    view.until_shows("$ ");
    view.type_line("echo vi$((40+2))ew");
    view.until_shows("vi42ew");

    // And none of it crossed the app's own connection as bytes: this session was spawned to be viewed
    let leaked: Vec<Event> = client.events.try_iter().filter(|event| matches!(event, Event::Output { .. })).collect();
    assert!(leaked.is_empty(), "the client was sent {leaked:?}");
}

#[test]
fn history_is_fetched_by_line_and_text_is_read_and_searched() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    view.type_line("i=0; while [ $i -lt 120 ]; do echo line-$i; i=$((i+1)); done; echo fin$((1+1))ished");
    // Arithmetic, so what is waited for is the output and not the echo of the command that asks for it
    view.until_shows("fin2ished");

    // The search finds what scrolled off long ago, newest first, and its numbers are lines to fetch
    view.send(&ToBroker::Search { channel: 1, request: 5, needle: "line-7".to_owned() });
    let spans = view.answer(|message| match message {
        ToView::Spans { request: 5, spans, .. } => Some(spans.clone()),
        _ => None,
    });
    let oldest = spans.last().expect("the search found something").start;
    view.send(&ToBroker::Fetch { channel: 1, request: 6, epoch: 0, first: oldest.line, count: 3 });
    let fetched = view.answer(|message| match message {
        ToView::Rows { request: 6, block, .. } => Some(block.rows.iter().map(text).collect::<Vec<_>>()),
        _ => None,
    });
    assert_eq!(fetched, vec!["line-7", "line-8", "line-9"]);

    view.send(&ToBroker::WordAt { channel: 1, request: 7, epoch: 0, at: Position { line: oldest.line, column: 2 } });
    let word = view.answer(|message| match message {
        ToView::Span { request: 7, span, .. } => Some(*span),
        _ => None,
    });
    let word = word.expect("there is a word there");
    assert_eq!((word.start.column, word.end.column), (0, 5));

    let across = Span { start: word.start, end: Position { line: oldest.line + 1, column: 5 } };
    view.send(&ToBroker::TextOf { channel: 1, request: 8, epoch: 0, span: across });
    let read = view.answer(|message| match message {
        ToView::Text { request: 8, text, .. } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(read, "line-7\nline-8");
}

#[test]
fn a_paste_that_could_run_a_command_is_refused_until_the_person_allows_it() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");

    let dangerous = "echo PA$((1+1))STE\n".to_owned();
    view.send(&ToBroker::Paste { channel: 1, request: 3, allow_unsafe: false, text: dangerous.clone() });
    let outcome = view.answer(|message| match message {
        ToView::Pasted { request: 3, outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(outcome, 1, "refused: `sh` does not bracket, so the line ending is the Enter key");

    view.send(&ToBroker::Paste { channel: 1, request: 4, allow_unsafe: true, text: dangerous });
    let outcome = view.answer(|message| match message {
        ToView::Pasted { request: 4, outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(outcome, 0);
    view.until_shows("PA2STE");
}

#[test]
fn a_view_is_told_when_its_session_ends_and_sent_the_screen_as_it_ended() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");

    view.type_line("echo last-wo$((1+1))rds; exit");
    while !view.ended {
        view.step();
    }
    assert!(view.screen().contains("last-wo2rds"), "the screen as it ended:\n{}", view.screen());

    // Attaching to a session that is gone is answered the same way, not left waiting
    let mut late = View::attached(&broker, &session);
    assert!(matches!(late.step(), ToView::Ended { channel: 1 }));
}

/// A view that stops drawing is sent nothing more, and costs nobody else anything.
#[test]
fn a_view_that_stops_acknowledging_does_not_hold_up_another() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let stuck = View::attached(&broker, &session);
    let mut live = View::attached(&broker, &session);
    live.until_shows("$ ");

    live.type_line("echo sec$((1+1))ond");
    live.until_shows("sec2ond");

    // The stuck view was sent its first frame and, never having acknowledged it, nothing since
    let sent: Vec<ToView> = stuck.incoming.try_iter().collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
}

#[test]
fn a_view_leaving_or_sending_nonsense_leaves_the_session_running() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    {
        let mut first = View::attached(&broker, &session);
        first.until_shows("$ ");
    }
    // Not the protocol: a length, a kind no message has, and then anything
    let vandal = View::greeting(&broker);
    (&*vandal.stream).write_all(&[3, 0, 0, 0, 250, 1, 2]).expect("the broker reads");
    let mut closed = [0u8; 1];
    let _ = (&*vandal.stream).read(&mut closed);

    let mut second = View::attached(&broker, &session);
    second.until_shows("$ ");
    second.type_line("echo sti$((1+1))ll");
    second.until_shows("sti2ll");
}

/// What a program asks of the world outside its screen reaches the app's own connection, as it happens —
/// from a session whose bytes are not sent at all.
#[test]
fn a_programs_title_clipboard_and_commands_are_told_to_the_client() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let write = |text: &str| say(&client.stream, &Request::Write { session: session.clone(), text: text.to_owned() });
    write("printf '\\033]0;a title\\007\\033]52;c;aGk=\\007\\033]633;E;make\\007\\033]633;C\\007\\033]633;D;2\\007'\n");

    let mut told = Vec::new();
    while told.len() < 4 {
        match client.event() {
            Event::Title { title, .. } => told.push(format!("title {title}")),
            Event::Clipboard { text, .. } => told.push(format!("clipboard {text}")),
            Event::CommandStarted { command, .. } => told.push(format!("started {command:?}")),
            Event::CommandFinished { exit_status, .. } => told.push(format!("finished {exit_status:?}")),
            Event::Output { .. } => panic!("this session's bytes are not sent"),
            _ => {}
        }
    }
    assert_eq!(told, ["title a title", "clipboard hi", "started Some(\"make\")", "finished Some(2)"]);
}

/// A shell that marks its commands is remembered: a view is told when there is something new to ask for and
/// is answered with each command and the lines that are its, and the app's own connection can read what
/// one printed.
#[test]
fn the_commands_a_shell_marks_are_listed_with_their_lines_and_their_output_is_read() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    let before = view.last.as_ref().expect("a frame arrived").commands;

    // The marks a shell's integration writes around a command, written here by hand: its prompt, the
    // start of its output and its end with a status
    view.type_line("printf '\\033]133;A\\007\\033]133;C\\007first\\nsecond\\n\\033]133;D;3\\007'; echo mar$((1+1))ked");
    view.until_shows("mar2ked");
    assert_ne!(view.last.as_ref().expect("a frame arrived").commands, before, "a view was not told to ask again");

    view.send(&ToBroker::Commands { channel: 1, request: 21 });
    let commands = view.answer(|message| match message {
        ToView::Commands { request: 21, commands, .. } => Some(commands.clone()),
        _ => None,
    });
    let [command] = commands.as_slice() else { panic!("expected one command, was told {commands:?}") };
    assert_eq!(command.exit_status, Some(3));
    let (output, end) = (command.output.expect("its output has a line"), command.end.expect("its end has a line"));
    assert_eq!(end - output, 2, "two lines of output between where it began and where it ended");

    view.send(&ToBroker::Fetch { channel: 1, request: 22, epoch: 0, first: output, count: 2 });
    let printed = view.answer(|message| match message {
        ToView::Rows { request: 22, block, .. } => Some(block.rows.iter().map(text).collect::<Vec<_>>()),
        _ => None,
    });
    assert_eq!(printed, vec!["first", "second"]);

    // And the same command, asked for by the connection that owns the session
    say(&client.stream, &Request::Commands { session: session.clone() });
    let id = loop {
        if let Event::Commands { commands, .. } = client.event() {
            let [record] = commands.as_slice() else { panic!("expected one command, was told {commands:?}") };
            assert_eq!(record.exit_status, Some(3));
            break record.id;
        }
    };
    say(&client.stream, &Request::CommandOutput { session: session.clone(), command: id });
    let text = loop {
        if let Event::CommandOutput { text, .. } = client.event() {
            break text;
        }
    };
    assert_eq!(text.as_deref(), Some("first\nsecond"));
}

/// The line under a cell, asked for over the connection: all of what wrapped, from its first row to its
/// last.
#[test]
fn the_line_at_a_cell_is_answered_to_the_view_that_asked() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    // Sixty columns: seventy characters wrap onto a second row
    view.type_line(&format!("echo st$((1+1))art-{}", "w".repeat(70)));
    view.until_shows("st2art-");

    view.send(&ToBroker::Search { channel: 1, request: 5, needle: "st2art-".to_owned() });
    let found = view.answer(|message| match message {
        ToView::Spans { request: 5, spans, .. } => Some(spans.clone()),
        _ => None,
    });
    let start = found.last().expect("the search found the line").start;
    let below = Position { line: start.line + 1, column: 3 };
    view.send(&ToBroker::LineAt { channel: 1, request: 6, epoch: 0, at: below });
    let line = view
        .answer(|message| match message {
            ToView::Span { request: 6, span, .. } => Some(*span),
            _ => None,
        })
        .expect("there is a line there");
    assert_eq!((line.start.line, line.start.column), (start.line, 0));
    assert_eq!((line.end.line, line.end.column), (start.line + 1, 16));
}

/// An image a program places is told to a view that asks where the images are, and its pixels to one that
/// asks for them — in a session that was started taking images, with cells whose size is known.
#[test]
fn a_placed_image_and_its_pixels_are_answered_to_the_view_that_asked() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell_taking(true);
    say(
        &client.stream,
        &Request::Resize { session: session.clone(), cols: 60, rows: 8, cell_width: 10, cell_height: 20 },
    );
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    // One red pixel, sent whole and shown where the cursor is
    view.type_line("printf '\\033_Ga=T,f=32,i=7,s=1,v=1,q=2;/wAA/w==\\033\\\\'; echo sh$((1+1))own");
    view.until_shows("sh2own");

    view.send(&ToBroker::Images { channel: 1, request: 9 });
    let (cell, placed) = view.answer(|message| match message {
        ToView::Images { request: 9, cell_width, cell_height, placements, .. } => {
            Some(((*cell_width, *cell_height), placements.clone()))
        }
        _ => None,
    });
    assert_eq!(cell, (10, 20));
    assert_eq!(placed.iter().map(|placement| placement.image).collect::<Vec<_>>(), [7]);

    view.send(&ToBroker::Image { channel: 1, request: 10, image: 7, offset: 0 });
    let pixels = view.answer(|message| match message {
        ToView::Image { request: 10, width, height, channels, total, bytes, .. } => {
            Some((*width, *height, *channels, *total, bytes.clone()))
        }
        _ => None,
    });
    assert_eq!(pixels, (1, 1, 4, 4, vec![255, 0, 0, 255]));
}
