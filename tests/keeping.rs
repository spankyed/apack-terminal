//! Sessions that outlast what started them: one kept past the app's hold on the broker, one whose screen
//! is picked up from its recording by the session that replaces it, and a recording looked back through.
//!
//! Against the real binary and a real shell, over connections that are hung up the way an app quitting
//! hangs them up.
//!
//! **The shell is the platform's own.** A case types the same line on both: on POSIX a `/bin/sh` line, on
//! Windows the PowerShell that prints the same text. Every case waits for the prompt `$ ` and for the text it
//! asked for, so the two differ only in the lines typed and in the few cases about what a POSIX process does
//! that Windows has no name for, which are marked where they are.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use apack_terminal::protocol::{CommandOutcome, Event, PROTOCOL_VERSION, Request, Role, SessionSummary, Unreported};
use apack_terminal::view::wire::{Frame, Incoming, Row, ToBroker, ToView, read_message};
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

/// How long a wait in this file goes on with nothing arriving. **Shorter than the runner's deadline for a
/// case** (`.config/nextest.toml`), so a wait that runs out fails saying what it was waiting for, where
/// one as long as the deadline is ended by the runner saying nothing.
const PATIENCE: Duration = Duration::from_secs(15);

/// A message the broker cannot read, which it answers and so wakes a reader that has to be let go.
const NONSENSE_VIEW: &[u8] = &[3, 0, 0, 0, 250, 1, 2];
const NONSENSE_CLIENT: &[u8] = b"not a frame\n";

/// Where a broker listens. A path on POSIX; on Windows a named pipe, one namespace for the whole machine,
/// so the name carries this process and a counter (the spelling `tests/broker.rs` settled on).
#[cfg(unix)]
fn socket_in(directory: &Path) -> PathBuf {
    directory.join("broker.sock")
}

#[cfg(windows)]
fn socket_in(_directory: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    PathBuf::from(format!(r"\\.\pipe\apack-terminal-keeping-{}-{nth}", std::process::id()))
}

/// One connection, readable and writable from two places: `&Stream` implements both, so an `Arc` is what
/// the crate's deprecated `split` tells callers to use instead of halves.
#[derive(Clone)]
struct Wire(Arc<Stream>);

impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        Read::read(&mut &*self.0, buffer)
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

/// A connection to the broker at `socket`, which every client must then greet.
fn connect(socket: &Path) -> Wire {
    let name = socket.to_str().expect("a utf-8 socket path").to_fs_name::<GenericFilePath>().expect("a socket name");
    Wire(Arc::new(Stream::connect(name).expect("the broker accepts")))
}

/// Ends a connection from this end, so the broker sees the client leave and the thread reading it is let go.
///
/// POSIX shuts the socket down, which the far end sees at once. A named pipe cannot be shut down one way, and
/// the reader thread holds the pipe open until something wakes it: so the receiver goes first and one
/// message the broker answers is sent, which the reader takes, fails to hand on, and returns from. That drops
/// its clone, and the last reference is what closes the pipe.
fn hang_up_stream<T>(events: Receiver<T>, stream: Wire, nonsense: &[u8]) {
    #[cfg(unix)]
    {
        let Stream::UdSocket(socket) = &*stream.0;
        let _ = socket.inner().shutdown(std::net::Shutdown::Both);
        let _ = nonsense;
        drop(events);
    }
    #[cfg(windows)]
    {
        drop(events);
        let _ = (&*stream.0).write_all(nonsense);
        drop(stream);
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Says nothing more will be sent, and waits for the broker to have let the connection go: it closes its end
/// last, after everything a connection ending leads to.
///
/// POSIX closes its sending side and waits for the broker to close the other. A named pipe has no half close,
/// so there the connection is let go as `hang_up_stream` lets it go, and the broker sees the client leave; the
/// wait is for the pipe to be closed, which is a sleep rather than an answer.
fn closed_stream<T>(events: Receiver<T>, stream: Wire, nonsense: &[u8]) {
    #[cfg(unix)]
    {
        let Stream::UdSocket(socket) = &*stream.0;
        let _ = socket.inner().shutdown(std::net::Shutdown::Write);
        let _ = nonsense;
        while events.recv_timeout(PATIENCE).is_ok() {}
    }
    #[cfg(windows)]
    {
        hang_up_stream(events, stream, nonsense);
    }
}

struct Broker {
    process: Child,
    socket: PathBuf,
    directory: tempfile::TempDir,
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

impl Broker {
    fn start() -> Self {
        let directory = tempfile::tempdir().expect("a directory for the socket");
        let socket = socket_in(directory.path());
        let mut command = Command::new(env!("CARGO_BIN_EXE_apack-terminal"));
        command.arg(&socket).stdout(Stdio::piped());
        // A session of its own, so a signal from the broker cannot leave this test (`tests/broker.rs`)
        #[cfg(unix)]
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
        Self { process, socket, directory, reaper }
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

/// The shell every case runs: `/bin/sh` with `$ ` as its prompt on POSIX, PowerShell with `$ ` as its prompt
/// on Windows. Returned as the program, its arguments and its environment.
fn shell_command() -> (String, Vec<String>, Vec<(String, String)>) {
    #[cfg(unix)]
    {
        ("/bin/sh".to_owned(), Vec::new(), vec![("PS1".to_owned(), "$ ".to_owned()), ("ENV".to_owned(), String::new())])
    }
    #[cfg(windows)]
    {
        (
            "powershell.exe".to_owned(),
            ["-NoLogo", "-NoProfile", "-NoExit", "-Command", "function prompt { '$ ' }"].map(str::to_owned).to_vec(),
            Vec::new(),
        )
    }
}

/// A line that prints `before`, then the sum of two numbers, then `after`. The sum is what a case waits for,
/// so the line typed never contains the text it is waiting for.
fn echo_sum(before: &str, left: u32, right: u32, after: &str) -> String {
    #[cfg(unix)]
    {
        format!("echo {before}$(({left}+{right})){after}")
    }
    #[cfg(windows)]
    {
        format!("Write-Output \"{before}$({left}+{right}){after}\"")
    }
}

/// Prints `body` as the shell would print escape sequences: `\033` is ESC, `\007` is BEL and `\n` a newline.
fn printed(body: &str) -> String {
    #[cfg(unix)]
    {
        format!("printf '{body}'")
    }
    #[cfg(windows)]
    {
        let body = body.replace("\\033", "$([char]27)").replace("\\007", "$([char]7)").replace("\\n", "`n");
        format!("[Console]::Write(\"{body}\")")
    }
}

/// Shows an application's alternate screen with the mouse reported, then waits: `APP-2-SHOWN` is on it.
fn show_application_then_wait() -> String {
    #[cfg(unix)]
    {
        "printf \"\\033[?1049h\\033[?1000hAPP-$((1+1))-SHOWN\"; sleep 100".to_owned()
    }
    #[cfg(windows)]
    {
        "[Console]::Write(\"$([char]27)[?1049h$([char]27)[?1000hAPP-$(1+1)-SHOWN\"); Start-Sleep 100".to_owned()
    }
}

/// Prints twelve numbered lines, the last `row-12-of-the-first`.
fn twelve_rows_of_the_first() -> String {
    #[cfg(unix)]
    {
        "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo row-$i-of-the-first; done".to_owned()
    }
    #[cfg(windows)]
    {
        "1..12 | ForEach-Object { Write-Output \"row-$_-of-the-first\" }".to_owned()
    }
}

/// Makes a file at `path`, as a typed line would.
fn touch_line(path: &Path) -> String {
    #[cfg(unix)]
    {
        format!("touch {}", path.display())
    }
    #[cfg(windows)]
    {
        format!("New-Item -ItemType File -Force -Path '{}' | Out-Null", path.display())
    }
}

/// Connection: the hold on the broker, or the one that asks for sessions.
struct Connection {
    stream: Wire,
    events: Receiver<Event>,
}

impl Connection {
    fn greeting(broker: &Broker, role: Role) -> Self {
        let stream = connect(&broker.socket);
        let (sender, events) = channel();
        std::thread::spawn({
            let stream = stream.clone();
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
        (&*self.stream.0).write_all(line.as_bytes()).expect("the broker reads");
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
        self.spawned(record, restore).0
    }

    /// A shell, with whether the broker says it is recorded and whether it was restored.
    fn spawned(&self, record: Option<&Path>, restore: bool) -> (String, bool, bool) {
        self.spawned_with(record, restore, None)
    }

    /// The same, in a session whose marks `marks_key` vouches for when there is one.
    fn spawned_with(&self, record: Option<&Path>, restore: bool, marks_key: Option<&str>) -> (String, bool, bool) {
        let (shell, args, env) = shell_command();
        self.say(&Request::Spawn {
            cols: 60,
            rows: 8,
            shell: Some(shell),
            args,
            env,
            unset: Vec::new(),
            cwd: None,
            colors: None,
            output: false,
            record: record.map(|path| path.to_str().expect("a utf-8 path").to_owned()),
            restore,
            images: false,
            image_files: false,
            marks_key: marks_key.map(str::to_owned),
        });
        self.answer(|event| match event {
            Event::Spawned { session, recorded, restored, .. } => Some((session, recorded, restored)),
            _ => None,
        })
    }

    /// Starts a program of the broker's choosing (not the shell every case runs), and returns its session.
    fn spawn_program(&self, shell: &str, args: Vec<String>) -> String {
        self.say(&Request::Spawn {
            cols: 60,
            rows: 8,
            shell: Some(shell.to_owned()),
            args,
            env: Vec::new(),
            unset: Vec::new(),
            cwd: None,
            colors: None,
            output: false,
            record: None,
            restore: false,
            images: false,
            image_files: false,
            marks_key: None,
        });
        self.answer(|event| if let Event::Spawned { session, .. } = event { Some(session) } else { None })
    }

    /// The commands a session's shell has reported, each as its line, where the shell was, and how it
    /// ended.
    fn commands(&self, session: &str) -> Vec<(Option<String>, Option<String>, CommandOutcome)> {
        self.say(&Request::Commands { session: session.to_owned() });
        let commands =
            self.answer(|event| if let Event::Commands { commands, .. } = event { Some(commands) } else { None });
        commands.into_iter().map(|one| (one.command, one.cwd, one.outcome)).collect()
    }

    /// Ends a session and waits for it to have ended.
    fn kill(&self, session: &str) {
        self.say(&Request::Kill { session: session.to_owned() });
        self.answer(|event| matches!(event, Event::Exited { session: ended, .. } if ended == session).then_some(()));
    }

    fn sessions(&self) -> Vec<SessionSummary> {
        self.say(&Request::Sessions);
        self.answer(|event| if let Event::Sessions { sessions } = event { Some(sessions) } else { None })
    }

    /// Closes the connection, as its process ending would.
    fn hang_up(self) {
        hang_up_stream(self.events, self.stream, NONSENSE_CLIENT);
    }

    /// Says nothing more will be sent, and waits for the broker to have let the connection go.
    fn closed(self) {
        closed_stream(self.events, self.stream, NONSENSE_CLIENT);
    }
}

/// A view's connection, and the screen it has been sent.
struct View {
    stream: Wire,
    incoming: Receiver<ToView>,
    rows: BTreeMap<u64, Row>,
    last: Option<Frame>,
}

impl View {
    fn attached(broker: &Broker, session: &str) -> Self {
        let stream = connect(&broker.socket);
        let mut hello = serde_json::to_string(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View })
            .expect("a request serializes");
        hello.push('\n');
        (&*stream.0).write_all(hello.as_bytes()).expect("the broker reads");
        let (sender, incoming) = channel();
        std::thread::spawn({
            let stream = stream.clone();
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
        (&*self.stream.0).write_all(&message.encode()).expect("the broker reads");
    }

    /// Messages until the view is told its session is over.
    fn until_ended(&mut self) {
        loop {
            let message = self.incoming.recv_timeout(PATIENCE).expect("the broker sends");
            if matches!(message, ToView::Ended { .. }) {
                return;
            }
        }
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
                composed: false,
            });
        }
        self.send(&ToBroker::Key {
            channel: 1,
            action: 0,
            modifiers: 0,
            code: "Enter".to_owned(),
            text: String::new(),
            unshifted: 0,
            composed: false,
        });
    }

    fn hang_up(self) {
        hang_up_stream(self.incoming, self.stream, NONSENSE_VIEW);
    }

    /// Says nothing more will be sent, and waits for the broker to have let the connection go.
    fn closed(self) {
        closed_stream(self.incoming, self.stream, NONSENSE_VIEW);
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

/// Types characters at a session without entering them.
fn type_without_entering(view: &View, text: &str) {
    for character in text.chars() {
        view.send(&ToBroker::Key {
            channel: 1,
            action: 0,
            modifiers: 0,
            code: String::new(),
            text: character.to_string(),
            unshifted: u32::from(character),
            composed: false,
        });
    }
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
    view.type_line(&echo_sum("be", 1, 1, "fore"));
    view.until_shows("be2fore");

    // Each waited for, so there is a moment when nothing at all is connected and the kept session is the
    // only reason the broker is still there
    view.closed();
    client.closed();
    hold.closed();

    // A new app: it holds the broker again and asks what is there
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);
    let found = client.sessions();
    assert_eq!(found.iter().map(|one| one.session.as_str()).collect::<Vec<_>>(), [kept.as_str()], "not {ended}");
    assert!(found[0].kept);
    assert_eq!((found[0].cols, found[0].rows), (60, 8));
    let mut view = View::attached(&broker, &kept);
    view.until_shows("be2fore");
    view.type_line(&echo_sum("af", 1, 2, "ter"));
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
    #[cfg(unix)]
    view.type_line("sleep 1; exit");
    #[cfg(windows)]
    view.type_line("Start-Sleep 1; exit");
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
    view.type_line(&twelve_rows_of_the_first());
    view.until_shows("row-12-of-the-first");
    // Left inside an application on the alternate screen, with the mouse reported
    view.type_line(&show_application_then_wait());
    view.until_shows("APP-2-SHOWN");
    view.hang_up();
    client.kill(&first);

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
    view.type_line(&echo_sum("from-the-se", 1, 1, "cond"));
    view.until_shows("from-the-se2cond");
    view.hang_up();
    client.kill(&second);
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
    view.type_line(&echo_sum("ear", 1, 1, "ly"));
    view.until_shows("ear2ly");
    // Recordings are stamped in milliseconds: far enough apart that a moment between the two exists
    std::thread::sleep(Duration::from_millis(60));
    let between = apack_terminal::recording::now();
    std::thread::sleep(Duration::from_millis(60));
    view.type_line(&echo_sum("la", 2, 2, "ter"));
    view.until_shows("la4ter");

    client.say(&Request::Rewind { recording: recording.to_str().expect("a utf-8 path").to_owned(), cols: 60, rows: 8 });
    let (rewound, first, last) = client.answer(|event| {
        if let Event::Rewound { session, first, last } = event { Some((session, first, last)) } else { None }
    });
    assert!(first < between && between < last, "{first} {between} {last}");
    // Opened again it is another session, with an id of its own and no live session's
    client.say(&Request::Rewind { recording: recording.to_str().expect("a utf-8 path").to_owned(), cols: 60, rows: 8 });
    let again = client.answer(|event| if let Event::Rewound { session, .. } = event { Some(session) } else { None });
    assert!(again != rewound && again != live && rewound != live, "{again} {rewound} {live}");
    client.say(&Request::Kill { session: again });
    let mut looking = View::attached(&broker, &rewound);
    looking.until_shows("la4ter");

    client.say(&Request::Seek { session: rewound.clone(), at: between });
    client.answer(|event| matches!(event, Event::Sought { .. }).then_some(()));
    looking.until("showed the earlier moment", |view| {
        view.screen().contains("ear2ly") && !view.screen().contains("la4ter")
    });
    // Typed at, it does nothing, and the live session is not what it reached
    looking.type_line(&echo_sum("no", 1, 1, "where"));
    client.say(&Request::Seek { session: rewound.clone(), at: last });
    client.answer(|event| matches!(event, Event::Sought { .. }).then_some(()));
    looking.until_shows("la4ter");
    assert!(!looking.screen().contains("no2where"), "{}", looking.screen());
    assert!(!view.screen().contains("no2where"));

    // It is not a session with a process, and ending it ends no shell
    assert_eq!(client.sessions().iter().map(|one| one.session.as_str()).collect::<Vec<_>>(), [live.as_str()]);
    client.say(&Request::Kill { session: rewound });
    view.type_line(&echo_sum("still-he", 1, 1, "re"));
    view.until_shows("still-he2re");
}

/// **A spawn says what it got, not only that it started.** Whether a session is recorded, and whether it
/// began with what a recording held, are things a caller asked for and may not have been given: nothing
/// to record to, a file somebody else is writing, a file that is no recording.
#[test]
fn a_spawn_says_whether_it_is_recorded_and_whether_it_was_restored() {
    let broker = Broker::start();
    let recording = broker.recording("terminal-1");
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);

    // Asked to restore and given nowhere to restore from
    let (unrecorded, recorded, restored) = client.spawned(None, true);
    assert_eq!((recorded, restored), (false, false));

    // A recording begun: recorded, and nothing was there to restore
    let (first, recorded, restored) = client.spawned(Some(&recording), true);
    assert_eq!((recorded, restored), (true, false));
    let mut view = View::attached(&broker, &first);
    view.until_shows("$ ");
    view.type_line(&echo_sum("from-the-fi", 1, 1, "rst"));
    view.until_shows("from-the-fi2rst");

    // **One writer a file.** A second session asked to record there while the first still is starts,
    // unrecorded — and with nothing restored, the recording being somebody else's to write
    let (second, recorded, restored) = client.spawned(Some(&recording), true);
    assert_eq!((recorded, restored), (false, false));
    let listed = client.sessions();
    let is_recorded = |session: &str| listed.iter().find(|one| one.session == session).map(|one| one.recorded);
    assert_eq!(
        (is_recorded(&first), is_recorded(&second), is_recorded(&unrecorded)),
        (Some(true), Some(false), Some(false))
    );
    // The same file by another spelling is the same file: through `.`, through `..`, and through a link
    // to the directory it is in
    let directory = recording.parent().expect("a directory");
    let linked = broker.directory.path().join("linked");
    make_directory_link(directory, &linked);
    for respelled in [
        directory.join(".").join("terminal-1"),
        directory.join("..").join("recordings").join("terminal-1"),
        linked.join("terminal-1"),
    ] {
        assert!(!client.spawned(Some(&respelled), false).1, "{} was taken for another file", respelled.display());
    }

    // The first ended, the file is free again, and what it recorded is all there to begin from
    client.kill(&first);
    let (third, recorded, restored) = client.spawned(Some(&recording), true);
    assert_eq!((recorded, restored), (true, true));
    let mut view = View::attached(&broker, &third);
    view.until_shows("from-the-fi2rst");

    // A file that is there and is no recording is neither recorded to nor emptied
    let notes = broker.directory.path().join("notes.txt");
    std::fs::write(&notes, b"somebody's notes").expect("the file is written");
    let (_, recorded, restored) = client.spawned(Some(&notes), true);
    assert_eq!((recorded, restored), (false, false));
    assert_eq!(std::fs::read(&notes).expect("the file reads"), b"somebody's notes");
    drop(hold);
}

/// A directory link to `target` at `link`. A symbolic link on POSIX; a junction on Windows, which needs no
/// privilege to make.
fn make_directory_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("the link is made");
    }
    #[cfg(windows)]
    {
        let made =
            Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(link).arg(target).status().expect("mklink runs");
        assert!(made.success(), "mklink /J failed");
    }
}

/// A session that ends of its own accord, and one ended with its holder, each leave their recording whole
/// and free: the session that replaces either is recorded there, and begins with what it held.
#[test]
fn a_recording_is_free_and_whole_however_its_session_ended() {
    let broker = Broker::start();
    let recording = broker.recording("terminal-1");
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);

    // The shell exits because it was told to, by the person typing at it
    let (first, ..) = client.spawned(Some(&recording), false);
    let mut view = View::attached(&broker, &first);
    view.until_shows("$ ");
    view.type_line(&format!("{}; exit", echo_sum("typed-ex", 1, 1, "it")));
    client.answer(|event| matches!(event, Event::Exited { .. }).then_some(()));
    let (second, recorded, restored) = client.spawned(Some(&recording), true);
    assert_eq!((recorded, restored), (true, true));
    let mut view = View::attached(&broker, &second);
    view.until_shows("typed-ex2it");
    view.until_shows("$ ");
    view.type_line(&echo_sum("before-the-ho", 1, 1, "lder-went"));
    view.until_shows("before-the-ho2lder-went");

    // The app goes, and its sessions with it; what is still connected starts one in the same place
    hold.hang_up();
    client.answer(|event| matches!(event, Event::Exited { .. }).then_some(()));
    let (third, recorded, restored) = client.spawned(Some(&recording), true);
    assert_eq!((recorded, restored), (true, true));
    let mut view = View::attached(&broker, &third);
    view.until_shows("before-the-ho2lder-went");
}

/// **A restored session lists the commands of the one before it as they ended.** The recording holds
/// the stream as it was printed — the marks the session believed and the ones it refused alike — and the
/// key that stream was vouched with, so the commands rebuilt from it are the ones that were: a finish
/// printed by something without the key ends nothing the second time either, and the command the last
/// shell left running is over without having said how. The session that restores it has a key of its own.
#[test]
fn a_restored_keyed_session_lists_the_commands_of_the_one_before_it_as_they_ended() {
    const FIRST: &str = "00112233445566778899aabbccddeeff";
    const SECOND: &str = "ffeeddccbbaa99887766554433221100";
    let start = |key: &str, number: u32, line: &str| format!("\\033]633;V;S;{number};{key};{line}\\007");
    let finish = |key: &str, number: u32, status: u32| format!("\\033]633;V;F;{number};{key};{status};/src\\007");

    let broker = Broker::start();
    let recording = broker.recording("terminal-1");
    let client = Connection::greeting(&broker, Role::Client);
    let (first, ..) = client.spawned_with(Some(&recording), false, Some(FIRST));
    let mut view = View::attached(&broker, &first);
    view.until_shows("$ ");
    // What a hook holding the key writes around two commands, and between them what a program without
    // it writes to be believed: an end in each family anything can print, and a keyed one of its own
    let forged = format!("\\033]133;D;0\\007\\033]633;D;0\\007\\033]133;A\\007{}", finish(SECOND, 1, 0));
    let body =
        format!("{}built\\n{forged}{}{}", start(FIRST, 1, "make"), finish(FIRST, 1, 3), start(FIRST, 2, "sleep 100"));
    view.type_line(&format!("{}; {}", printed(&body), echo_sum("mar", 1, 1, "ked")));
    view.until_shows("mar2ked");
    let exited_3 = (Some("make".to_owned()), Some("/src".to_owned()), CommandOutcome::Exited { code: 3 });
    assert_eq!(
        client.commands(&first),
        vec![exited_3.clone(), (Some("sleep 100".to_owned()), None, CommandOutcome::Running)]
    );
    client.kill(&first);

    let (second, recorded, restored) = client.spawned_with(Some(&recording), true, Some(SECOND));
    assert_eq!((recorded, restored), (true, true));
    let left_running = (Some("sleep 100".to_owned()), None, CommandOutcome::Unknown { why: Unreported::SessionEnded });
    assert_eq!(client.commands(&second), vec![exited_3.clone(), left_running.clone()]);

    // Its own shell's marks are heard with its own key, numbered from one
    let mut view = View::attached(&broker, &second);
    view.until_shows("$ ");
    let body = format!("{}{}", start(SECOND, 1, "ls"), start(FIRST, 3, "forged"));
    view.type_line(&format!("{}; {}", printed(&body), echo_sum("mar", 2, 1, "ked")));
    view.until_shows("mar3ked");
    assert_eq!(
        client.commands(&second),
        vec![exited_3, left_running, (Some("ls".to_owned()), None, CommandOutcome::Running)]
    );
}

/// **A recording being looked through is its opener's.** Nothing else knows its id, and it is no process
/// that would end by itself; so it ends when the connection that opened it does, and a view of it is told.
#[test]
fn a_recording_being_looked_through_ends_with_the_connection_that_opened_it() {
    let broker = Broker::start();
    let recording = broker.recording("terminal-1");
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let opener = Connection::greeting(&broker, Role::Client);
    let live = opener.shell(Some(&recording), false);
    let mut view = View::attached(&broker, &live);
    view.until_shows("$ ");
    view.type_line(&echo_sum("reco", 1, 1, "rded"));
    view.until_shows("reco2rded");

    opener.say(&Request::Rewind { recording: recording.to_str().expect("a utf-8 path").to_owned(), cols: 60, rows: 8 });
    let rewound = opener.answer(|event| if let Event::Rewound { session, .. } = event { Some(session) } else { None });
    let mut looking = View::attached(&broker, &rewound);
    looking.until_shows("reco2rded");

    // The opener goes. The app still holds the broker, so the live session stays — and the look back goes
    opener.hang_up();
    looking.until_ended();
    View::attached(&broker, &rewound).until_ended();

    let next = Connection::greeting(&broker, Role::Client);
    assert_eq!(next.sessions().iter().map(|one| one.session.as_str()).collect::<Vec<_>>(), [live.as_str()]);
    view.type_line(&echo_sum("still-run", 1, 1, "ning"));
    view.until_shows("still-run2ning");
    drop(hold);
}

/// **Ending a session submits nothing to it.** A line typed at a shell and not entered is not entered by
/// the terminal being closed, nor by the app going: here each line would make a file, and no file is
/// made. Checked once the shell is known to be gone, after which nothing could make one.
#[test]
fn a_line_typed_and_not_entered_is_not_run_when_its_session_is_ended() {
    let broker = Broker::start();
    let hold = Connection::greeting(&broker, Role::Lifetime);
    let client = Connection::greeting(&broker, Role::Client);
    let made = |name: &str| broker.directory.path().join(name);

    // Closed: the session is killed
    let closed = client.shell(None, false);
    let mut view = View::attached(&broker, &closed);
    view.until_shows("$ ");
    type_without_entering(&view, &touch_line(&made("on-kill")));
    view.until_shows("on-kill");
    client.kill(&closed);
    assert!(!made("on-kill").exists(), "killing a session entered the line that was on its prompt");

    // The same with a program that takes whatever line it is given: writing what it reads to a file
    let reading = {
        #[cfg(unix)]
        let args = vec!["-c".to_owned(), format!("cat > {}", made("read-by-cat").display())];
        // Reads a line at a time as the console does, echoing it, and writes each line it was given: a line
        // typed and not entered is read by nothing, so nothing is written
        #[cfg(windows)]
        let args = vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            format!(
                "while ($true) {{ $line = [Console]::ReadLine(); if ($line -eq $null) {{ break }}; Add-Content -Path '{}' -Value $line }}",
                made("read-by-cat").display()
            ),
        ];
        #[cfg(unix)]
        let shell = "/bin/sh";
        #[cfg(windows)]
        let shell = "powershell.exe";
        client.spawn_program(shell, args)
    };
    let mut view = View::attached(&broker, &reading);
    type_without_entering(&view, "half a line");
    view.until_shows("half a line");
    client.kill(&reading);
    assert_eq!(std::fs::read(made("read-by-cat")).unwrap_or_default(), b"", "a line was submitted to the program");

    // Quit: the app's hold on the broker goes, and its sessions with it
    let quit = client.shell(None, false);
    let mut view = View::attached(&broker, &quit);
    view.until_shows("$ ");
    type_without_entering(&view, &touch_line(&made("on-quit")));
    view.until_shows("on-quit");
    hold.hang_up();
    client.answer(|event| matches!(event, Event::Exited { session, .. } if session == quit).then_some(()));
    assert!(!made("on-quit").exists(), "the app going entered the line that was on a prompt");
}

/// **A view that stops making sense and stops reading is hung up on, not waited for.** It is owed more
/// than its socket holds, so the thread writing to it is parked; it then sends something that is not the
/// protocol and goes quiet, its connection still open. The broker lets go of it there and then — which
/// shows as the broker exiting when its last real connection leaves, with the view's socket still held
/// open at this end.
#[test]
fn a_view_that_sends_nonsense_and_stops_reading_does_not_keep_the_broker() {
    let mut broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let session = client.shell(None, false);
    {
        let mut drawn = View::attached(&broker, &session);
        drawn.until_shows("$ ");
        drawn.hang_up();
    }

    // Greets as a view, attaches, and never reads a byte
    let vandal = connect(&broker.socket);
    let hello = serde_json::to_string(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View }).expect("hello");
    (&*vandal.0).write_all(format!("{hello}\n").as_bytes()).expect("the broker reads");
    (&*vandal.0)
        .write_all(&ToBroker::Attach { channel: 1, session: session.clone() }.encode())
        .expect("the broker reads");
    // Megabytes of answers it will not read: far more than a socket holds, far less than it may leave
    let ask = ToBroker::Commands { channel: 1, request: 1 }.encode();
    let search = ToBroker::Search { channel: 1, request: 2, needle: "$".to_owned(), case_sensitive: false }.encode();
    for _ in 0..40_000 {
        (&*vandal.0).write_all(&ask).expect("the broker reads");
        (&*vandal.0).write_all(&search).expect("the broker reads");
    }
    // Then something that is no message, and nothing more
    (&*vandal.0).write_all(NONSENSE_VIEW).expect("the broker reads");

    client.hang_up();
    assert!(broker.exits(), "the broker stayed for a view it could not write to and would not hear from");
    drop(vandal);
}

/// **What a session printed last is in its recording, though the broker left the moment it ended.** A
/// recording is written by a thread of its own, behind the session; a broker with nothing left exits,
/// and it waits for that thread first — so the session that picks the recording up, under another
/// broker, begins with everything the last one printed.
#[test]
fn a_recording_holds_all_its_session_printed_though_the_broker_left_as_it_ended() {
    let recordings = tempfile::tempdir().expect("a directory for the recording");
    let recording = recordings.path().join("kept").join("terminal-1");
    {
        let mut broker = Broker::start();
        let client = Connection::greeting(&broker, Role::Client);
        let kept = client.shell(Some(&recording), false);
        client.say(&Request::Keep { session: kept.clone(), kept: true });
        client.answer(|event| matches!(event, Event::Kept { .. }).then_some(()));
        let mut view = View::attached(&broker, &kept);
        view.until_shows("$ ");
        // Once everyone has gone: a great deal of output, a last line, and the shell's own exit — which is
        // the broker's last reason to stay
        #[cfg(unix)]
        let typed = format!(
            "sleep 1; i=0; while [ $i -lt 20000 ]; do echo \"line $i of what was printed before the end\"; \
             i=$((i+1)); done; {}; exit",
            echo_sum("the-la", 1, 1, "st-line")
        );
        #[cfg(windows)]
        let typed = format!(
            "Start-Sleep 1; for ($i = 0; $i -lt 20000; $i++) {{ [Console]::WriteLine(\"line $i of what was printed before the end\") }}; {}; exit",
            echo_sum("the-la", 1, 1, "st-line")
        );
        view.type_line(&typed);
        view.hang_up();
        client.hang_up();
        assert!(broker.exits(), "the broker stayed after its last session ended");
    }

    let broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let (session, recorded, restored) = client.spawned(Some(&recording), true);
    assert_eq!((recorded, restored), (true, true));
    let mut view = View::attached(&broker, &session);
    view.until_shows("the-la2st-line");
}

/// **Ending a session ends it, whatever its program makes of being asked.** What a session is sent is a
/// hangup, and a program may ignore one; such a session is still reported over, because what was asked
/// for and not done is then done for certain.
///
/// POSIX only: a hangup is a signal, and `trap '' HUP` is a POSIX program ignoring it. A Windows console
/// sends a close event that a program may handle, but has no signal a shell can ignore.
#[cfg(unix)]
#[test]
fn a_session_whose_program_ignores_a_hangup_is_ended_all_the_same() {
    let broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let session = client.spawn_program(
        "/bin/sh",
        vec!["-c".to_owned(), "trap '' HUP; echo rea$((1+1))dy; while :; do sleep 1; done".to_owned()],
    );
    // Shown once the hangup is being ignored
    View::attached(&broker, &session).until_shows("rea2dy");

    client.say(&Request::Kill { session: session.clone() });
    let (code, signal) = client.answer(|event| match event {
        Event::Exited { session: ended, code, signal } if ended == session => Some((code, signal)),
        _ => None,
    });
    assert_eq!((code, signal.as_deref()), (None, Some("SIGKILL")), "it was not the broker that ended it");
}

/// **And what its shell had running goes with it.** A shell that is killed passes no hangup on to its
/// jobs, and the kernel hangs up only the foreground group, so a job in a group of its own would be left
/// running under nothing. It is hung up before its shell is killed.
///
/// POSIX only: it is about process groups, which a Windows job object replaces with a different guarantee
/// (`reaper.rs`). The Windows side of that guarantee is its own case.
#[cfg(unix)]
#[test]
fn a_job_in_a_group_of_its_own_ends_with_a_shell_that_had_to_be_killed() {
    let broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let notes = tempfile::tempdir().expect("a directory for the job's pid");
    let pid_file = notes.path().join("job.pid");
    // A shell that takes a hangup and carries on, so it has to be killed; and under it a job that made
    // itself a process group, as a shell with job control makes of one put in the background
    let script = format!(
        "trap : HUP; perl -e 'setpgrp(0, 0); sleep 300' & echo $! > {}; echo rea$((1+1))dy; while :; do sleep 1; done",
        pid_file.display()
    );
    let session = client.spawn_program("/bin/sh", vec!["-c".to_owned(), script]);
    View::attached(&broker, &session).until_shows("rea2dy");
    let job = std::fs::read_to_string(&pid_file).expect("the job's pid").trim().to_owned();
    let running =
        || std::process::Command::new("kill").args(["-0", &job]).output().is_ok_and(|said| said.status.success());
    assert!(running(), "the job {job} is running");

    client.say(&Request::Kill { session: session.clone() });
    client.answer(|event| match event {
        Event::Exited { session: ended, .. } if ended == session => Some(()),
        _ => None,
    });

    // A process being torn down reports nothing but its absence, so this is a count of looks and no wait
    // on anything it does
    let gone = (0..200).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(25));
        !running()
    });
    assert!(gone, "the job {job} outlived the shell it was started in");
}

/// **Asking after a recording makes nothing.** A recording that is not there is answered as not there,
/// and the directory it would have been in is not there afterwards either.
#[test]
fn looking_back_through_a_recording_that_does_not_exist_leaves_nothing_behind() {
    let broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let absent = broker.recording("never-recorded");
    client.say(&Request::Rewind { recording: absent.to_str().expect("a utf-8 path").to_owned(), cols: 60, rows: 8 });
    loop {
        match client.event() {
            Event::Failed { .. } => break,
            Event::Rewound { .. } => panic!("a recording that does not exist was opened"),
            _ => {}
        }
    }
    let directory = absent.parent().expect("a recording is in a directory");
    assert!(!directory.exists(), "{} was made by asking after a recording in it", directory.display());
}

/// **A view that finished speaking and then takes nothing is not waited for.** It closed its side, which
/// is what a view that is still listening does — and then read none of the megabytes it was owed, with
/// its connection left open. The broker gives up on it when it has taken nothing for a while, rather
/// than keeping a thread, and itself, for a view that will never read.
///
/// POSIX only: it needs the view to close its side and keep the connection open, and a named pipe has no
/// one-way close to do that with.
#[cfg(unix)]
#[test]
fn a_view_that_stops_speaking_and_never_reads_does_not_keep_the_broker() {
    let mut broker = Broker::start();
    let client = Connection::greeting(&broker, Role::Client);
    let session = client.shell(None, false);
    {
        let mut drawn = View::attached(&broker, &session);
        drawn.until_shows("$ ");
        drawn.hang_up();
    }

    let silent = connect(&broker.socket);
    let hello = serde_json::to_string(&Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View }).expect("hello");
    (&*silent.0).write_all(format!("{hello}\n").as_bytes()).expect("the broker reads");
    (&*silent.0)
        .write_all(&ToBroker::Attach { channel: 1, session: session.clone() }.encode())
        .expect("the broker reads");
    // Megabytes of answers it will not read: far more than a socket holds, far less than it may leave
    let ask = ToBroker::Commands { channel: 1, request: 1 }.encode();
    let search = ToBroker::Search { channel: 1, request: 2, needle: "$".to_owned(), case_sensitive: false }.encode();
    for _ in 0..40_000 {
        (&*silent.0).write_all(&ask).expect("the broker reads");
        (&*silent.0).write_all(&search).expect("the broker reads");
    }
    // Nothing more to say, said properly — and the connection kept open, unread
    let Stream::UdSocket(socket) = &*silent.0;
    socket.inner().shutdown(std::net::Shutdown::Write).expect("its side closes");

    client.hang_up();
    assert!(broker.exits(), "the broker stayed for a view that had stopped reading");
    drop(silent);
}
