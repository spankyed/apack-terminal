//! A view driven as the window will drive it: over a real socket, against the real binary and a real shell.
//!
//! Everything a view is sent is proven without a socket in `view_frames.rs` and `view_model.rs`. What is
//! proven here is the part that needs one: that a connection greeting as a view is served, that its keys
//! reach a program and the program's output comes back as rows, and that a view coming, going or
//! misbehaving costs the session nothing.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role};
use apack_terminal::view::wire::{Frame, Incoming, Position, Row, Span, ToBroker, ToView, read_message};

/// How long a case waits for something a working broker does at once: generous, so a busy machine does
/// not fail one. **Shorter than the runner's deadline for a
/// case** (`.config/nextest.toml`), so a wait that runs out fails saying what it was waiting for, where
/// one as long as the deadline is ended by the runner saying nothing.
const PATIENCE: Duration = Duration::from_secs(15);

struct Broker {
    process: Child,
    socket: std::path::PathBuf,
    _directory: tempfile::TempDir,
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
    let reaper = crate::reaper::watching(&process, env!("CARGO_BIN_EXE_apack-terminal"));
    Broker { process, socket, _directory: directory, reaper }
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
        self.running("/bin/sh", &[], images)
    }

    /// A session running `program`, whose bytes nobody reads.
    fn running(&self, program: &str, arguments: &[&str], images: bool) -> String {
        say(
            &self.stream,
            &Request::Spawn {
                cols: 60,
                rows: 8,
                shell: Some(program.to_owned()),
                args: arguments.iter().map(|argument| (*argument).to_owned()).collect(),
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
            composed: false,
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

    // The stuck view was sent its first frame and, never having acknowledged it, nothing since. The
    // frame is waited for: it was sent before the live view's, and is read by a thread of its own
    let first = stuck.incoming.recv_timeout(PATIENCE).expect("the stuck view's first frame");
    assert!(matches!(first, ToView::Frame { .. }), "{first:?}");
    let since: Vec<ToView> = stuck.incoming.try_iter().collect();
    assert!(since.is_empty(), "{since:?}");
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

/// **A view that loses focus holds no button down any more.** The button is let go somewhere else, so
/// the release this view would have reported never comes: a move it reports after that is not a drag,
/// where one it reported with the button down was.
#[test]
fn a_view_that_loses_focus_with_a_button_down_is_no_longer_dragging() {
    let broker = start();
    let client = Client::greeting(&broker);
    // A program that asks to be told of drags and then waits for a line that never ends: its terminal
    // echoes what it is sent, an escape as `^[`, so a pointer report is on the screen as text
    let session = client.running("/bin/sh", &["-c", "printf '\\033[?1002h\\033[?1006h'; exec cat"], false);
    let mut view = View::attached(&broker, &session);
    while !view.last.as_ref().is_some_and(|frame| frame.mouse) {
        view.step();
    }
    // Cells ten pixels by twenty, on the second row
    let pointer = |action: u8, x: f32| ToBroker::Pointer {
        channel: 1,
        action,
        button: 0,
        modifiers: 0,
        x,
        y: 30.0,
        cell_width: 10.0,
        cell_height: 20.0,
    };

    view.send(&pointer(0, 15.0));
    view.until_shows("[<0;2;2M");
    // Past the right edge with the button down, which is a drag and is reported at the edge
    view.send(&pointer(2, 900.0));
    view.until_shows("[<32;60;2M");

    view.send(&ToBroker::Focus { channel: 1, gained: false });
    // Past the left edge, which only a view holding a button down is reported at
    view.send(&pointer(2, -30.0));
    view.send(&ToBroker::Text { channel: 1, text: "after".to_owned() });
    view.until_shows("after");
    assert!(!view.screen().contains("[<32;1;2M"), "a drag was reported after the view lost focus:\n{}", view.screen());
}

/// **Nothing that has something for a program waits for the program to take it.** A paste of megabytes
/// into a program that echoes every line is the case where waiting would stop everything: the program
/// reads only as fast as its echo is read, and whatever was waiting on it would be holding what the
/// reader of that echo needs. So the paste is answered at once, the rest of the broker goes on
/// answering, and all of it reaches the program.
#[test]
fn a_paste_of_megabytes_into_a_program_that_echoes_holds_nothing_up() {
    let broker = start();
    let client = Client::greeting(&broker);
    let echoing = client.running("/bin/cat", &[], false);
    let mut view = View::attached(&broker, &echoing);
    view.step();

    let mut text = "the quick brown fox jumps over the lazy dog\n".repeat(3 * (1 << 20) / 44);
    text.push_str("that-is-all-of-it\n");
    view.send(&ToBroker::Paste { channel: 1, request: 3, allow_unsafe: true, text });
    let outcome = view.answer(|message| match message {
        ToView::Pasted { request: 3, outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(outcome, 0);

    // While the program is still reading it: the broker lists its sessions, starts another, and that one
    // is typed at and answers
    say(&client.stream, &Request::Sessions);
    loop {
        if let Event::Sessions { sessions } = client.event() {
            assert_eq!(sessions.len(), 1);
            break;
        }
    }
    let other = client.shell();
    let mut beside = View::attached(&broker, &other);
    beside.until_shows("$ ");
    beside.type_line("echo mean$((1+1))while");
    beside.until_shows("mean2while");

    view.until_shows("that-is-all-of-it");
}

/// A program that never reads its input fills its pty with the first few kilobytes written to it. What
/// is written after that waits in the broker — up to a bound, past which it is refused — and neither the
/// waiting nor the refusal is anything another session, or this one's ending, has to wait for.
#[test]
fn a_program_that_never_reads_holds_up_neither_another_session_nor_its_own_ending() {
    let broker = start();
    let client = Client::greeting(&broker);
    let deaf = client.running("/bin/sleep", &["600"], false);

    // Twenty megabytes of lines, half a megabyte to a request: more than may wait for it. Lines,
    // because a terminal takes a line that is never finished by throwing the end of it away, and one
    // that is finished by keeping it for the program — which is what fills a pty
    let chunk = "a line the program will never read\n".repeat((1 << 19) / 35);
    let mut refused = None;
    for _ in 0..40 {
        say(&client.stream, &Request::Write { session: deaf.clone(), text: chunk.clone() });
    }
    // Each write was accepted or refused, and the broker read every one of them to get to this
    say(&client.stream, &Request::Sessions);
    loop {
        match client.event() {
            Event::Failed { session: Some(session), message, .. } if session == deaf => refused = Some(message),
            Event::Sessions { sessions } => {
                assert_eq!(sessions.len(), 1);
                break;
            }
            _ => {}
        }
    }
    let refused = refused.expect("twenty megabytes were taken for a program that reads nothing");
    assert!(refused.contains("takes no more"), "{refused}");

    // Another session is started, typed at and answers
    let other = client.shell();
    let mut beside = View::attached(&broker, &other);
    beside.until_shows("$ ");
    beside.type_line("echo unhe$((1+1))ld");
    beside.until_shows("unhe2ld");

    // And the one that is not reading ends when it is told to
    say(&client.stream, &Request::Kill { session: deaf.clone() });
    loop {
        if let Event::Exited { session, .. } = client.event()
            && session == deaf
        {
            break;
        }
    }
}

/// **A resize is told to a view by the resize.** A program is told its terminal changed size and may
/// print nothing in answer — `cat` never does — so the frame with the new grid cannot wait for output.
#[test]
fn a_resize_is_sent_to_a_view_though_the_program_prints_nothing() {
    let broker = start();
    let client = Client::greeting(&broker);
    let silent = client.running("/bin/cat", &[], false);
    let mut view = View::attached(&broker, &silent);
    let first = match view.step() {
        ToView::Frame(frame) => frame,
        other => panic!("expected the first frame, was sent {other:?}"),
    };
    assert_eq!((first.columns, first.screen_rows), (60, 8));

    say(&client.stream, &Request::Resize { session: silent, cols: 44, rows: 5, cell_width: 0, cell_height: 0 });
    let resized = view.answer(|message| match message {
        ToView::Frame(frame) if frame.columns != 60 => Some(frame.clone()),
        _ => None,
    });
    assert_eq!((resized.columns, resized.screen_rows), (44, 5));
    assert_ne!(resized.epoch, first.epoch);
}

/// **A view that asks and does not read is hung up on.** What it is owed waits in a queue of its own, and
/// that queue has an end: a view asking for megabytes over and over while taking none of them is not
/// reading, and the broker stops holding answers for it rather than holding ever more.
#[test]
fn a_view_that_asks_without_reading_is_hung_up_on() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    {
        // Something worth asking for: a screen wide and tall enough that its lines are a megabyte
        let mut filling = View::attached(&broker, &session);
        say(
            &client.stream,
            &Request::Resize { session: session.clone(), cols: 500, rows: 100, cell_width: 0, cell_height: 0 },
        );
        filling.until_shows("$ ");
        filling.type_line("i=0; while [ $i -lt 200 ]; do printf '%0500d\\n' $i; i=$((i+1)); done; echo fil$((1+1))led");
        filling.until_shows("fil2led");
    }

    // A connection that greets as a view, attaches, reads its first frame, and from then on only writes
    let stream = connect(&broker);
    say(&stream, &Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View });
    let send = |message: &ToBroker| (&*stream).write_all(&message.encode());
    send(&ToBroker::Attach { channel: 1, session: session.clone() }).expect("the broker reads");
    let first = {
        let mut reader = BufReader::new(&*stream);
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("the handshake is answered");
        let Incoming::Message(body) = read_message(&mut reader) else { panic!("the first frame did not come") };
        match ToView::decode(&body) {
            Ok(ToView::Frame(frame)) => frame,
            other => panic!("expected the first frame, was sent {other:?}"),
        }
    };
    // Everything the session holds, as text: a hundred kilobytes an answer. Far more are asked for than
    // may wait, and the broker — which reads every request — stops reading this connection's, and closes it
    let all = Span {
        start: Position { line: first.first_active - u64::from(first.scrollback), column: 0 },
        end: Position { line: first.first_active + u64::from(first.screen_rows) - 1, column: 499 },
    };
    let ask = ToBroker::TextOf { channel: 1, request: 1, epoch: first.epoch, span: all };
    let hung_up = (0..20_000).any(|_| send(&ask).is_err());
    assert!(hung_up, "twenty thousand answers were kept for a view that read none of them");

    // The session, and a view that does read, are none the worse
    let mut reading = View::attached(&broker, &session);
    reading.until_shows("fil2led");
}

/// One end of a connection as something to read from, for a reader that is kept between reads.
struct Reading(Arc<Stream>);

impl Read for Reading {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buffer)
    }
}

/// A view's connection that **takes what it is sent only when a case reads it**: greeted, attached on
/// channel 1, and with its first frame read. `View` reads on a thread of its own, as a window does, so
/// it cannot be a view that has stopped reading.
struct Unread {
    stream: Arc<Stream>,
    reader: BufReader<Reading>,
    first: Frame,
}

impl Unread {
    fn attached(broker: &Broker, session: &str) -> Self {
        let stream = connect(broker);
        say(&stream, &Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View });
        (&*stream)
            .write_all(&ToBroker::Attach { channel: 1, session: session.to_owned() }.encode())
            .expect("the broker reads");
        let mut reader = BufReader::new(Reading(Arc::clone(&stream)));
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("the handshake is answered");
        let Incoming::Message(body) = read_message(&mut reader) else { panic!("the first frame did not come") };
        match ToView::decode(&body) {
            Ok(ToView::Frame(first)) => Self { stream, reader, first },
            other => panic!("expected the first frame, was sent {other:?}"),
        }
    }

    /// Asks for everything the session holds, as text.
    fn ask_for_all_of_it(&self, request: u32) {
        let first = &self.first;
        let all = Span {
            start: Position { line: first.first_active - u64::from(first.scrollback), column: 0 },
            end: Position { line: first.first_active + u64::from(first.screen_rows) - 1, column: first.columns - 1 },
        };
        let ask = ToBroker::TextOf { channel: 1, request, epoch: first.epoch, span: all };
        (&*self.stream).write_all(&ask.encode()).expect("the broker reads");
    }
}

/// A session holding most of a megabyte of text, with the view that typed it: far more than a
/// connection carries unread, on any platform, so an answer made of it is one the broker is still
/// writing for as long as the view it is for reads nothing.
fn a_session_holding_a_megabyte(broker: &Broker, client: &Client) -> (String, View) {
    let session = client.shell();
    let mut filling = View::attached(broker, &session);
    say(
        &client.stream,
        &Request::Resize { session: session.clone(), cols: 1000, rows: 100, cell_width: 0, cell_height: 0 },
    );
    filling.until_shows("$ ");
    filling.type_line("i=0; while [ $i -lt 900 ]; do printf '%01000d\\n' $i; i=$((i+1)); done; echo fil$((1+1))led");
    filling.until_shows("fil2led");
    (session, filling)
}

/// Closes a connection's own side and waits until the broker has closed the other, which is the last
/// thing it does for a connection: by then whatever that connection's going leads to has happened.
/// `incoming` is what the connection's reader hands on, which ends when the reader is told there is
/// no more.
fn gone<T>(stream: &Stream, incoming: &Receiver<T>) {
    let Stream::UdSocket(socket) = stream;
    socket.inner().shutdown(std::net::Shutdown::Write).expect("the connection closes one way");
    loop {
        match incoming.recv_timeout(PATIENCE) {
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => panic!("the broker kept open a connection that had finished with it"),
        }
    }
}

/// **A view that has finished speaking is still listening, and still connected.** It asked, closed its
/// side of the connection, and only then began to read — by which time everything else had gone: the
/// app's connection, its session, the view beside this one. A broker nothing is connected to leaves,
/// and this one has a view it owes most of a megabyte: the answer is written before that connection is
/// closed, and the broker goes after it rather than from under it.
#[test]
fn a_view_that_finished_speaking_is_sent_its_answer_though_it_is_all_the_broker_has_left() {
    let broker = start();
    let client = Client::greeting(&broker);
    let (session, mut beside) = a_session_holding_a_megabyte(&broker, &client);
    beside.send(&ToBroker::Detach { channel: 1 });

    let mut view = Unread::attached(&broker, &session);
    view.ask_for_all_of_it(1);
    let Stream::UdSocket(socket) = &*view.stream;
    socket.inner().shutdown(std::net::Shutdown::Write).expect("the connection closes one way");

    // **Nothing is read until the broker has seen that the view finished.** What says so is the
    // numbering: a view that comes and goes beside this one is told the numbering this one was told
    // for as long as this one is attached, and a new one once the broker has let go of it
    loop {
        beside.send(&ToBroker::Attach { channel: 1, session: session.clone() });
        let told = first_epoch(&mut beside);
        beside.send(&ToBroker::Detach { channel: 1 });
        if told != view.first.epoch {
            break;
        }
    }
    // The app goes, and its session with it; then the view beside this one
    gone(&client.stream, &client.events);
    gone(&beside.stream, &beside.incoming);

    let mut answer = None;
    while let Incoming::Message(body) = read_message(&mut view.reader) {
        if let Ok(ToView::Text { request: 1, text, .. }) = ToView::decode(&body) {
            answer = Some(text);
        }
    }
    let answer = answer.expect("the connection closed before the view was sent what it had asked for");
    assert!(answer.contains("fil2led"), "the answer is {} bytes and stops short", answer.len());
}

/// **A view that stops making sense is hung up on at once, whatever it is owed.** The thread writing
/// to it may be waiting on a view that will never read, and only the connection closing lets it go: a
/// view left connected there is one the broker holds an answer and a thread for, for as long as the
/// view likes.
#[test]
fn a_view_that_sends_what_is_not_the_protocol_is_hung_up_on_though_it_has_not_read_what_it_is_owed() {
    let broker = start();
    let client = Client::greeting(&broker);
    let (session, _filling) = a_session_holding_a_megabyte(&broker, &client);

    let view = Unread::attached(&broker, &session);
    view.ask_for_all_of_it(1);
    // Not the protocol: a length, a kind no message has, and then anything
    (&*view.stream).write_all(&[3, 0, 0, 0, 250, 1, 2]).expect("the broker reads");

    // Reading none of it, the view goes on writing: what tells it the broker hung up is a write being
    // refused. A write that is merely not taken — the broker no longer reading, the connection still
    // open — waits, and is given up on
    let Stream::UdSocket(socket) = &*view.stream;
    socket.inner().set_write_timeout(Some(PATIENCE)).expect("a write may be given up on");
    let refused = loop {
        if let Err(refused) = (&*view.stream).write_all(&[0]) {
            break refused;
        }
    };
    assert!(
        !matches!(refused.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
        "the connection was left open to a view that had stopped making sense: {refused}"
    );

    // The session, and a view that does read, are none the worse
    let mut reading = View::attached(&broker, &session);
    reading.until_shows("fil2led");
}

/// **A connection says hello once.** Its role is what it is counted as and how it is spoken to; said
/// again it is refused, and the connection is still the one it was.
#[test]
fn a_second_hello_is_refused_and_changes_nothing() {
    let broker = start();
    let client = Client::greeting(&broker);
    assert!(matches!(client.event(), Event::Hello { .. }));
    for role in [Role::Client, Role::View, Role::Lifetime] {
        say(&client.stream, &Request::Hello { protocol: PROTOCOL_VERSION, role });
        match client.event() {
            Event::Failed { message, .. } => assert!(message.contains("already said hello"), "{message}"),
            other => panic!("a second hello was answered {other:?}"),
        }
    }
    // Still a client, spoken to in JSON, whose requests are carried out
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");

    // And counted once: when it goes, nothing is left holding the broker's sessions, and they end
    let Stream::UdSocket(socket) = &*client.stream;
    socket.inner().shutdown(std::net::Shutdown::Both).expect("the connection closes");
    while !view.ended {
        view.step();
    }
    let mut late = View::greeting(&broker);
    late.send(&ToBroker::Attach { channel: 1, session });
    assert!(matches!(late.step(), ToView::Ended { channel: 1 }));
}

/// The epoch of the first frame a view is sent on attaching.
///
/// **The first of an attachment, which is the one that carries the colours** (`Frame::colors`), and not
/// the next frame to arrive: a view that attaches again may still be sent a frame of the attachment it
/// left, written before the broker read that it had gone. Nothing here changes a colour, so the frame
/// that has them is the one a new attachment begins with.
fn first_epoch(view: &mut View) -> u32 {
    view.answer(|message| match message {
        ToView::Frame(frame) if frame.colors.is_some() => Some(frame.epoch),
        _ => None,
    })
}

/// **A view that detaches is a view that has gone**, and with the last of them goes what the views were
/// told: the next to attach is in a numbering of its own, rather than one a view that left may still
/// hold rows of. The same when it is the connection that goes.
#[test]
fn the_last_view_leaving_ends_the_numbering_it_was_told() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();

    let mut view = View::attached(&broker, &session);
    let attached = first_epoch(&mut view);
    view.send(&ToBroker::Detach { channel: 1 });
    view.send(&ToBroker::Attach { channel: 1, session: session.clone() });
    let reattached = first_epoch(&mut view);
    assert_ne!(reattached, attached, "detaching left the view attached");

    // The connection closes, and the broker has let go of it by the time it closes its own end
    let Stream::UdSocket(socket) = &*view.stream;
    socket.inner().shutdown(std::net::Shutdown::Write).expect("the connection closes one way");
    while view.incoming.recv_timeout(PATIENCE).is_ok() {}
    let mut next = View::attached(&broker, &session);
    assert_ne!(first_epoch(&mut next), reattached, "a connection that went left its view attached");
}

/// **A channel whose session is over is attached to nothing**, whether or not the view detaches: what it
/// asks of that channel next is answered as for a channel that was never attached, and the session's pty
/// and screen are nobody's to keep.
#[test]
fn a_channel_whose_session_ended_is_let_go_of_without_being_detached() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    say(&client.stream, &Request::Kill { session });
    while !view.ended {
        view.step();
    }

    let asked = |view: &mut View, request: u32| {
        view.send(&ToBroker::Paste { channel: 1, request, allow_unsafe: true, text: "x".to_owned() });
        view.answer(|message| match message {
            ToView::Failed { request: answered, message, .. } if *answered == request => Some(message.clone()),
            _ => None,
        })
    };
    // The first asks of a session that has ended; from then on the channel is simply not attached
    let first = asked(&mut view, 1);
    assert!(first.contains("no session") || first.contains("ended"), "{first}");
    assert_eq!(asked(&mut view, 2), "this channel is not attached to a session");
}

/// **And what a view sends that has no answer lets go of the channel too.** A key or text for a session
/// that is over is told to nobody, so it is the broker that notices: the next thing asked on that
/// channel is asked of a channel attached to nothing.
#[test]
fn text_for_a_session_that_ended_lets_go_of_its_channel_though_nothing_answers_it() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    say(&client.stream, &Request::Kill { session });
    while !view.ended {
        view.step();
    }

    view.send(&ToBroker::Text { channel: 1, text: "x".to_owned() });
    view.send(&ToBroker::Paste { channel: 1, request: 1, allow_unsafe: true, text: "x".to_owned() });
    let refused = view.answer(|message| match message {
        ToView::Failed { request: 1, message, .. } => Some(message.clone()),
        _ => None,
    });
    assert_eq!(refused, "this channel is not attached to a session");
}

/// **A question that is refused is not a session that ended.** One put in a numbering that is over — the
/// lines were numbered again by a resize while the question was on its way — is refused saying so, and
/// the channel is still attached: the same view asks again in the numbering it has since been told.
#[test]
fn a_question_in_a_numbering_that_is_over_is_refused_and_its_channel_stays_attached() {
    let broker = start();
    let client = Client::greeting(&broker);
    let session = client.shell();
    let mut view = View::attached(&broker, &session);
    view.until_shows("$ ");
    let before = view.last.as_ref().expect("a frame arrived").epoch;

    say(&client.stream, &Request::Resize { session, cols: 44, rows: 5, cell_width: 0, cell_height: 0 });
    let now = view.answer(|message| match message {
        ToView::Frame(frame) if frame.epoch != before => Some(frame.epoch),
        _ => None,
    });

    let top = Position { line: 0, column: 0 };
    view.send(&ToBroker::TextOf { channel: 1, request: 1, epoch: before, span: Span { start: top, end: top } });
    let refused = view.answer(|message| match message {
        ToView::Failed { request: 1, message, .. } => Some(message.clone()),
        _ => None,
    });
    assert!(refused.contains("numbered again"), "{refused}");

    view.send(&ToBroker::Search { channel: 1, request: 2, needle: "$".to_owned() });
    let answered = view.answer(|message| match message {
        ToView::Spans { request: 2, epoch, .. } => Some(Ok(*epoch)),
        ToView::Failed { request: 2, message, .. } => Some(Err(message.clone())),
        _ => None,
    });
    assert_eq!(answered, Ok(now), "a refusal took the channel's session from it");
}
