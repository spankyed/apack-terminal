//! The apack terminal broker: one process that owns ptys so no session's lifetime is the app's.
//!
//! `apack-terminal <socket>` listens there and speaks newline-delimited JSON (`protocol`).
//!
//! **The sessions are the broker's, not a connection's.** They were a connection's while the only question
//! was whether the seam worked; keeping them there made a session's lifetime the lifetime of whichever
//! process opened it, which is the thing this component exists to stop. So one `Sessions` and one writer
//! live for the broker's life, and a connection is a thing that comes and goes beside them.
//!
//! **Who ends them is `Role`.** A `lifetime` connection holds them and ending it ends them all; absent one,
//! the last `client` leaving does, as it always has. The app's own process holds the former, so restarting
//! the service that opened a session does not end it and quitting the app does.

use std::cell::Cell;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

use apack_terminal::feed::{FEED_BYTES_ENV, Feed, feed_bytes};
use apack_terminal::frames::{Frame, read_frame};
use apack_terminal::listening::{self, HELD_EXIT, Refused, START_GRACE_ENV, hang_up};
use apack_terminal::output::{Outgoing, channel_with_credit};
use apack_terminal::protocol::{CommandRecord, ErrorCode, Event, PROTOCOL_VERSION, Request, Role, SearchMatch};
use apack_terminal::recording;
use apack_terminal::screen::{Colors, HISTORY_LINES};
use apack_terminal::session::{Handle, Refusal, Sessions, Spawning};
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::sink::Sink;
use apack_terminal::view::wire::{self, CommandRow, Incoming, Placement, ToBroker, ToView, read_message};
use apack_terminal::vt::input::{Key, Pointer};

/// The most images one answer lists.
///
/// A program places images where it likes and as many as it likes; each is a few dozen bytes told, so
/// this many is well inside one message (`wire::MAX_MESSAGE_BYTES`) and far past what a screen shows.
/// A session with more is told it has too many to list, rather than sent a message too long to read.
const IMAGES_LISTED: usize = 50_000;

/// The variable a broker is told a number other than `IMAGES_LISTED` by.
///
/// A parameter for the reason `feed::FEED_BYTES_ENV` is one: the bound is the subject of a case, and
/// fifty thousand images take the core longer to hold than the whole suite takes to run.
const IMAGES_LISTED_ENV: &str = "APACK_TERMINAL_IMAGES_LISTED";

/// Says why this broker is not going to listen, to whatever started it and to whoever reads its errors,
/// and exits.
///
/// **On stdout as well, as the line `listening` would have been.** A proxy that starts a broker reads
/// that one line to learn what became of it (`proxy::start_broker`), and reads nothing else of the
/// broker's.
fn refuse(socket: &str, refused: &Refused) -> ! {
    println!("failed {refused}");
    let _ = std::io::stdout().flush();
    eprintln!("apack-terminal: cannot listen on {socket}: {refused}");
    std::process::exit(if matches!(refused, Refused::Held) { HELD_EXIT } else { 1 });
}

fn main() -> std::io::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let socket = match arguments.as_slice() {
        // This process's stdin and stdout as a connection to the broker there (`proxy`), or to the one
        // at the place a broker is by default when no place is named
        [mode, rest @ ..] if mode == "proxy" && rest.len() <= 1 => {
            let Some(socket) = rest.first().cloned().or_else(listening::default_socket) else {
                eprintln!("apack-terminal: no socket was named, and there is no home directory to keep one in");
                std::process::exit(2);
            };
            return apack_terminal::proxy::run(&socket).inspect_err(|error| eprintln!("apack-terminal: {error}"));
        }
        [socket] => socket.clone(),
        _ => {
            eprintln!("usage: apack-terminal <socket>\n       apack-terminal proxy [<socket>]");
            std::process::exit(2);
        }
    };
    // **In this order, and before anything else exists**: the directory is one only this user can put a
    // socket in; this process is the one broker for the socket; and only then is a socket file that is
    // already there known to be a dead broker's and replaced (`listening`)
    let listening = listening::private_directory(&socket)
        .and_then(|()| listening::hold(&socket))
        .and_then(|held| listening::listen(&socket).map(|listener| (held, listener)));
    let (_held, listener) = match listening {
        Ok(listening) => listening,
        Err(refused) => refuse(&socket, &refused),
    };

    // Ready on stdout before the first accept, so a parent knows when to connect rather than retrying
    println!("listening {socket}");
    std::io::stdout().flush()?;

    let (events, outgoing, credit) = channel_with_credit();
    let broker = Broker {
        sessions: Arc::new(Sessions::new(events, credit)),
        feed: Arc::new(Mutex::new(None)),
        feed_bytes: feed_bytes(std::env::var(FEED_BYTES_ENV).ok().as_deref()),
        images_listed: std::env::var(IMAGES_LISTED_ENV)
            .ok()
            .and_then(|asked| asked.parse().ok())
            .unwrap_or(IMAGES_LISTED),
        owned: Arc::new(AtomicBool::new(false)),
        clients: Arc::new(AtomicUsize::new(0)),
        connections: Arc::new(AtomicUsize::new(0)),
        accepted: Arc::new(AtomicU64::new(0)),
    };
    // One thread takes everything off the queue for the broker's life, so what is reported is in one
    // order, and a client arriving or leaving changes where it goes rather than who decides that
    std::thread::spawn({
        let broker = broker.clone();
        move || write_out(outgoing, broker)
    });
    // **A broker nothing ever connects to has nothing to be either**, and no connection closing to tell
    // it so: what started it died before it could connect. So it gives whatever that was `START_GRACE`
    // to arrive (`listening::START_GRACE` on why this one is a clock), and then, if nothing has,
    // **connects to itself and hangs up**. That is a connection closing like any other, and what follows
    // from it is decided where every such thing is: with nothing else connected and no session, the
    // broker exits. A caller that was accepted first, or was waiting to be, is ahead of it in the queue
    // and already counted by then, so it is served rather than hung up on by a decision taken elsewhere.
    //
    // **A broker that cannot reach its own socket leaves without the connection.** Its socket's file was
    // removed, or the directory it is in: nothing else can reach it there either, so with nothing ever
    // accepted and nothing connected there is nobody to be hung up on by going
    std::thread::spawn({
        let (broker, socket) = (broker.clone(), socket.clone());
        move || {
            std::thread::sleep(listening::start_grace(std::env::var(START_GRACE_ENV).ok().as_deref()));
            if broker.accepted.load(Ordering::SeqCst) != 0 {
                return;
            }
            let reached = socket.as_str().to_fs_name::<GenericFilePath>().and_then(Stream::connect).is_ok();
            if !reached && broker.accepted.load(Ordering::SeqCst) == 0 && broker.connections.load(Ordering::SeqCst) == 0
            {
                leave();
            }
        }
    });

    for connection in listener.incoming() {
        match connection {
            // **A thread per connection**, because a lifetime connection and a client now coexist: serving
            // them in turn would mean the app's own hold on the broker blocked every session request
            Ok(stream) => {
                let broker = broker.clone();
                broker.connections.fetch_add(1, Ordering::SeqCst);
                let connection = broker.accepted.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || serve(stream, connection, broker));
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

/// Exits, once what was recorded is in its files.
///
/// **A recording is written by a thread, and a thread ends with its process.** A session's ending stops
/// its recording and leaves the last of it to the recorder's thread (`recording`), which is what keeps
/// an ending from waiting on a disk — and is, at the one moment the process itself ends, the end of
/// that recording never reaching its file. So every way out of a broker that has had sessions goes
/// through here, and waits for the recorders that have stopped: for as long as `RECORDER_PATIENCE`,
/// since a disk that takes nothing must not keep a broker nobody wants.
fn leave() -> ! {
    recording::settle_all();
    std::process::exit(0)
}

/// What every connection shares: the sessions, where their output currently goes, and who holds them.
#[derive(Clone)]
struct Broker {
    sessions: Arc<Sessions>,
    /// The client receiving the feed. A `lifetime` connection never installs itself here — it asks for
    /// nothing and is sent nothing, which is what makes holding one cost no output.
    feed: Arc<Mutex<Option<Arc<Client>>>>,
    /// How much may be waiting for a client to read before it is let go (`feed::FEED_BYTES`)
    feed_bytes: usize,
    /// How many of a session's images are listed for a view at most (`IMAGES_LISTED`)
    images_listed: usize,
    /// Whether a `lifetime` connection is open.
    owned: Arc<AtomicBool>,
    /// How many `client` connections are open.
    clients: Arc<AtomicUsize>,
    /// How many connections of any kind are open, **counted from the accept and not from the handshake**.
    /// That is what makes exiting at zero safe: a caller that has connected but not yet greeted is already
    /// counted, so it cannot be hung up on by a broker deciding nobody wants it.
    connections: Arc<AtomicUsize>,
    /// How many connections there have ever been, which is what tells one connection's views — and the
    /// recordings it opened — from another's, and whether anything has connected at all.
    accepted: Arc<AtomicU64>,
}

/// Deals with everything the sessions report, for as long as the broker runs: what a report means for
/// the broker is done here, and the report is handed to whichever client is attached.
///
/// **Handed on, never written here.** Each client has a queue and a writer of its own (`feed`), so this
/// thread waits on no connection: a client that has stopped reading is behind on what it is told, and
/// a session that ended is forgotten, its views are told and a broker with nothing left goes, all the
/// same.
fn write_out(outgoing: Receiver<Outgoing>, broker: Broker) {
    for mut message in outgoing {
        // **A shell that ended of its own accord is gone from here too.** Only `kill` used to remove a
        // session, so a user typing `exit` left its record behind: the map grew with every shell they
        // closed, and a later write answered `PTY` — the OS refusing a dead handle — where the true answer
        // is that there is no such session. On Windows that is X9's window, where ConPTY throws for about a
        // second after the console goes; the same wrong answer, louder.
        let exited = if let Some(Event::Exited { session, .. }) = &message.event {
            broker.sessions.forget(session);
            true
        } else {
            false
        };
        // The last kept session ending with nobody connected is the other way a broker runs out of
        // reasons to stay (`nothing_left`)
        if exited && nothing_left(&broker) {
            leave();
        }
        let frame = message.event.take().and_then(|event| serde_json::to_string(&event).ok());
        let attached = broker.feed.lock().ok().and_then(|feed| feed.clone());
        match attached {
            // Refused by a client that has been let go: the next one to greet installs itself. Nothing
            // is ended here, because whether a client leaving ends the sessions is `Role`'s question
            // and is answered in `serve`
            Some(client) => {
                if !client.feed.deliver(frame, message) {
                    detach(&broker, &client.wire);
                }
            }
            // **The credit returns whether or not anyone was listening.** It is what bounds the channel,
            // so a frame dropped for want of a client must not hold a slot — otherwise a restart of the
            // service that was attached would leave the broker permanently short of credit and its
            // output would stall.
            None => message.written(),
        }
    }
}

/// A client the broker reports to: its connection, and what is waiting to be written to it.
struct Client {
    wire: Arc<Wire>,
    feed: Arc<Feed>,
}

/// Makes `wire` a client the broker can report to, with a thread that writes what is queued for it.
///
/// **The writer returns what each message owes once it is written**, which is what makes credit a bound
/// on output that has not reached the client rather than on output that has not reached this queue. It
/// ends when the queue is closed or the client has gone, and takes the client out of the feed as it
/// does: a client that cannot be written to is not one to queue anything more for.
fn attachable(wire: &Arc<Wire>, broker: &Broker) -> Arc<Client> {
    let feed = Feed::holding(broker.feed_bytes, {
        let stream = Arc::clone(&wire.stream);
        move || {
            hang_up(&stream);
        }
    });
    let client = Arc::new(Client { wire: Arc::clone(wire), feed });
    std::thread::spawn({
        let (client, broker) = (Arc::clone(&client), broker.clone());
        move || {
            while let Some((frame, message)) = client.feed.next() {
                let gone = frame.is_some_and(|frame| client.wire.write_line(&frame).is_err());
                message.written();
                if gone {
                    break;
                }
            }
            client.feed.close();
            detach(&broker, &client.wire);
        }
    });
    client
}

/// Takes `wire` out of the feed, if it is still the one attached, **and lets every session be read
/// again**.
///
/// A pause belongs to the client that asked for one (`Request::Pause`). The next client to attach never
/// asked and has no way to find out, so a session nobody is reading would look like a shell that had
/// stopped writing. The client that paused it is gone; the pause goes with it.
///
/// **The feed's lock is let go before the sessions are touched.** Nothing else is ever locked while the
/// record of sessions is (`Sessions`), and resuming inside the feed's guard would be the first place a
/// lock was held across a call into it.
fn detach(broker: &Broker, wire: &Arc<Wire>) {
    let was_attached = match broker.feed.lock() {
        Ok(mut feed) if feed.as_ref().is_some_and(|attached| Arc::ptr_eq(&attached.wire, wire)) => {
            *feed = None;
            true
        }
        _ => false,
    };
    if was_attached {
        broker.sessions.resume_all();
    }
}

/// Answers this connection directly, rather than through the feed.
///
/// **A handshake is between the broker and one caller**, so its answer — and a refusal of a frame that
/// never named a session — goes to whoever asked. Sending those through the shared channel would deliver a
/// refusal of the lifetime connection's malformed frame to whichever client happened to be attached.
fn say(wire: &Wire, event: &Event) {
    if let Ok(frame) = serde_json::to_string(event) {
        let _ = wire.write_line(&frame);
    }
}

/// A connection, and what makes each line written to it whole.
///
/// **Two threads write to a client.** Its own writer sends it what the sessions report (`attachable`),
/// and the thread reading the connection answers it directly (`say`). A client reads a
/// line at a time, and two events on one line are two it can read neither of — its greeting's answer
/// among them. So each is written under `line`, in one write, and neither's lands inside the other's.
///
/// **A lock of the connection's own, held for that write and nothing else.** A client that has stopped
/// reading holds up what is said to it, and nothing that is said to anyone else: the next client to
/// greet is answered on a connection nobody else is writing to.
struct Wire {
    stream: Arc<Stream>,
    line: Mutex<()>,
}

impl Wire {
    fn write_line(&self, frame: &str) -> std::io::Result<()> {
        let mut line = String::with_capacity(frame.len() + 1);
        line.push_str(frame);
        line.push('\n');
        let _whole = self.line.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        (&*self.stream).write_all(line.as_bytes())
    }
}

/// One connection, for as long as it is open.
///
/// What it does on the way out is the whole of the lifetime rule: a `lifetime` connection ending ends every
/// session, and a `client` ending does only when no lifetime connection is open and it was the last client.
fn serve(stream: Stream, connection: u64, broker: Broker) {
    // **One stream in an `Arc`, not two halves.** `&Stream` implements `Read` and `Write`, which is what the
    // crate's deprecated `split` says to do instead — and dropping a half there does not shut it down, so
    // the halves were a shape that reads like a socket's and is not one.
    let wire = Arc::new(Wire { stream: Arc::new(stream), line: Mutex::new(()) });
    let mut leaving = Leaving { wire: Arc::clone(&wire), connection, broker: broker.clone(), role: None, feed: None };
    converse(&wire, connection, &broker, &mut leaving.role, &mut leaving.feed);
}

/// What a connection ending leads to, as a value whose going is the ending.
///
/// **So that it happens however the connection's thread stops.** A connection that was counted and never
/// uncounted is a broker that never exits, and a client that left without its sessions being ended is
/// shells left running; neither may hang on the thread having reached its last line.
struct Leaving {
    wire: Arc<Wire>,
    connection: u64,
    broker: Broker,
    role: Option<Role>,
    /// What was waiting to be written to it, for a connection that greeted as a client
    feed: Option<Arc<Feed>>,
}

impl Drop for Leaving {
    fn drop(&mut self) {
        match self.role {
            // **The app is gone, so its shells are.** The descriptor closed, which it does however the process
            // died — quit, crash or kill — so this is reached without anything having remembered to call it.
            Some(Role::Lifetime) => {
                self.broker.owned.store(false, Ordering::SeqCst);
                end_everything(&self.broker);
            }
            // **A client leaving ends nothing while an owner holds the broker**, which is what lets the service
            // that opened a session restart without taking it down. With no owner this is the behaviour the
            // broker has always had, and it is what keeps a standalone run — the suite, tooling — from leaving
            // shells behind.
            Some(Role::Client) => {
                // **What it asked last is answered before it is let go.** Its answers are written by the
                // attached client's writer, behind whatever was queued first, and a client may stop sending
                // while it still reads — a proxy whose input ended does exactly that. So that writer is
                // waited for to reach this point in its queue; only then is the connection taken out of
                // the feed and closed
                self.broker.sessions.settled();
                detach(&self.broker, &self.wire);
                // Its writer has nothing more to write and ends. One held in a write by a client that
                // stopped reading is let go by the connection closing, which is what this connection
                // ending is
                if let Some(feed) = &self.feed {
                    feed.close();
                }
                hang_up(&self.wire.stream);
                // The recordings it opened to look back through were its alone to name
                self.broker.sessions.end_rewound_by(self.connection);
                let left = self.broker.clients.fetch_sub(1, Ordering::SeqCst) - 1;
                if left == 0 && !self.broker.owned.load(Ordering::SeqCst) {
                    end_everything(&self.broker);
                }
            }
            // A view asked for no session and ends none
            Some(Role::View) | None => {}
        }

        // **A broker nobody is connected to has nothing left to be**, and exiting is the whole of its cleanup:
        // the socket goes with the process, and so does the memory every screen was holding. It used to listen
        // for ever instead, so each run of the app left one behind — harmless on its own, since its sessions
        // were already ended, and a process per launch all the same.
        if self.broker.connections.fetch_sub(1, Ordering::SeqCst) == 1 {
            end_everything(&self.broker);
            if nothing_left(&self.broker) {
                leave();
            }
        }
    }
}

/// Reads a connection's requests and carries them out, until it has no more. `role` is what the
/// connection greeted as, once it has.
fn converse(wire: &Arc<Wire>, connection: u64, broker: &Broker, role: &mut Option<Role>, feed: &mut Option<Arc<Feed>>) {
    let mut reader = BufReader::new(&*wire.stream);
    let mut line = String::new();

    loop {
        // **Bounded, because a frame is read before it is understood** (`frames`): one client writing
        // without ever sending a newline would otherwise take the broker's memory and every session with
        // it, and a process that owns the app's terminals must not be killable by a sender that declines to
        // finish a sentence.
        let request = match read_frame(&mut reader, &mut line) {
            Frame::Request(request) => request,
            Frame::Blank => continue,
            Frame::Malformed(message) => {
                say(wire, &Event::Failed { session: None, code: ErrorCode::Malformed, message });
                continue;
            }
            Frame::TooLong(message) => {
                say(wire, &Event::Failed { session: None, code: ErrorCode::Malformed, message });
                break;
            }
            Frame::Ended => break,
        };
        // **What the caller says last, and is then let go of.** Ends the conversation as a closed connection
        // does, and so is answered with the same things owed to a caller that has finished
        if matches!(request, Request::Done) {
            break;
        }
        // **The handshake is first or the connection is refused**, because the monorepo pins a commit of this
        // repository: a stale pin has to be a clear refusal rather than a message that means something else.
        match (&request, *role) {
            // **And it is said once.** A connection's role is what it is counted as and how it is spoken
            // to from then on: a client that greeted again would be counted twice and leave once, and one
            // that greeted again as a view would have its JSON answers and a view's binary frames written
            // to the same socket
            (Request::Hello { .. }, Some(_)) => {
                say(
                    wire,
                    &Event::Failed {
                        session: None,
                        code: ErrorCode::Handshake,
                        message: "this connection has already said hello, and its role is the one it gave then"
                            .to_owned(),
                    },
                );
                continue;
            }
            (Request::Hello { protocol, role: asked }, None) => {
                if *protocol != PROTOCOL_VERSION {
                    say(
                        wire,
                        &Event::Failed {
                            session: None,
                            code: ErrorCode::Handshake,
                            message: format!(
                                "the app speaks protocol {protocol}; this broker speaks {PROTOCOL_VERSION}"
                            ),
                        },
                    );
                    break;
                }
                *role = Some(*asked);
                match asked {
                    Role::Client => {
                        broker.clients.fetch_add(1, Ordering::SeqCst);
                    }
                    Role::Lifetime => broker.owned.store(true, Ordering::SeqCst),
                    Role::View => {}
                }
                say(wire, &Event::Hello { protocol: PROTOCOL_VERSION, history: HISTORY_LINES as u32 });
                match asked {
                    // Last greeting wins the feed: one app, one broker per data dir, and a client that
                    // reconnects after its service restarted is the one that should be receiving.
                    // **Once its greeting is answered, and not before**: from here it is sent whatever a
                    // session reports next, and the first thing a client hears is the handshake that
                    // says what those reports mean
                    Role::Client => {
                        let client = attachable(wire, broker);
                        *feed = Some(Arc::clone(&client.feed));
                        if let Ok(mut attached) = broker.feed.lock() {
                            *attached = Some(client);
                        }
                        // **A pause was the last reader's, and this is the reader now.** It asked for none
                        // and has no way to learn of one, and the client that did is no longer the one a
                        // session's output goes to — so nothing it could do would be held up by lifting
                        // it. Left to the old client's leaving, a pause outlived it whenever the new one
                        // greeted first: by then the one leaving is not the attached one, and `detach`
                        // resumes nothing. Outside the feed's lock, as there
                        broker.sessions.resume_all();
                    }
                    Role::View => {
                        // From here the connection speaks `view::wire`, on the reader that already holds
                        // whatever arrived behind the handshake
                        serve_view(&mut reader, &wire.stream, connection, broker);
                        break;
                    }
                    Role::Lifetime => {}
                }
                continue;
            }
            (_, None) => {
                say(
                    wire,
                    &Event::Failed {
                        session: None,
                        code: ErrorCode::Handshake,
                        message: "the first message must be hello".to_owned(),
                    },
                );
                break;
            }
            _ => {}
        }
        carry_out(&broker.sessions, connection, request);
    }
}

/// One view connection, from its handshake until it closes.
///
/// **Its own writer, and its own queue.** A client's events share one queue because one client receives
/// them; a view's frames are for that view, and a view that has stopped reading must not hold up another's
/// screen or a client's answer. The queue is bounded (`view::sink`): a view that asks and does not read
/// is hung up on rather than remembered.
fn serve_view(reader: &mut BufReader<&Stream>, stream: &Arc<Stream>, connection: u64, broker: &Broker) {
    // **Hung up on where it is overrun** (`Sink::new`): by whatever queued the message that was one too
    // many, which may be a session's reader with a frame, at a moment this connection is saying nothing
    let sink = Sink::new({
        let stream = Arc::clone(stream);
        move || {
            hang_up(&stream);
        }
    });
    let (writer_ended, written) = channel::<()>();
    let writing = std::thread::spawn({
        let (stream, sink) = (Arc::clone(stream), Arc::clone(&sink));
        move || {
            // Dropped when this thread ends, which is how whoever waits for it knows without joining
            let _ended = writer_ended;
            while let Some(message) = sink.next() {
                if (&*stream).write_all(&message).is_err() {
                    sink.close();
                    return;
                }
            }
        }
    });
    let mut leaving = ViewLeaving {
        broker,
        connection,
        stream: Arc::clone(stream),
        sink: Arc::clone(&sink),
        writing: Some(writing),
        written,
        taken: 0,
        said_all: false,
    };
    // The session each of this connection's channels is attached to
    let mut attached: HashMap<u32, Handle> = HashMap::new();
    loop {
        match read_message(reader) {
            Incoming::Message(body) => match ToBroker::decode(&body) {
                Ok(message) => carry_out_view(broker, connection, &sink, &mut attached, message),
                // Not the protocol: there is no telling what the rest of the stream is either
                Err(_) => break,
            },
            // A message so long that where the next one starts is not known
            Incoming::TooLong => break,
            // Nothing more is coming: the view said all it had to
            Incoming::Ended => {
                leaving.said_all = true;
                break;
            }
        }
    }
}

/// How long a view that has finished speaking may go without taking a message it is owed before it is
/// taken to have stopped listening. On the clock because what is waited for is the other end of a
/// connection reading, which nothing reports but the writing getting further.
const UNREAD_PATIENCE: Duration = Duration::from_secs(3);

/// What a view connection ending leads to, as a value whose going is the ending (`Leaving`).
struct ViewLeaving<'a> {
    broker: &'a Broker,
    connection: u64,
    stream: Arc<Stream>,
    sink: Arc<Sink>,
    writing: Option<std::thread::JoinHandle<()>>,
    /// Closed when the thread writing to the view has ended
    written: Receiver<()>,
    /// How many messages the view had been handed when that was last looked at
    taken: u64,
    /// The view closed its side, having said everything it was going to. Anything else that ends the
    /// reading — a message that is not one, a view hung up on — is a view that may never read again
    said_all: bool,
}

impl Drop for ViewLeaving<'_> {
    fn drop(&mut self) {
        self.broker.sessions.detach_connection(self.connection);
        // **A view that finished speaking is still listening**: what it was already owed an answer to is
        // written before the connection closes, as for a client. One that stopped being read for any
        // other reason is hung up on first, since the thread writing to it may be parked in a write to a
        // view that will never read it, and only the connection closing ends that
        let mut closed = self.said_all || hang_up(&self.stream);
        self.sink.finish();
        // **Listening is something a view does, not something it said.** One that finished speaking and
        // then takes nothing — alive, its side of the connection open, reading no more — has the thread
        // writing to it parked for as long as it likes. So what is waited for is the view taking
        // messages: a stretch of `UNREAD_PATIENCE` in which it took none is a view that is not
        // listening, and it is hung up on like any other
        self.taken = self.sink.handed_out();
        while self.said_all && self.written.recv_timeout(UNREAD_PATIENCE) == Err(RecvTimeoutError::Timeout) {
            let taken = std::mem::replace(&mut self.taken, self.sink.handed_out());
            if taken == self.taken {
                closed = hang_up(&self.stream);
                break;
            }
        }
        if let Some(writing) = self.writing.take()
            && closed
        {
            let _ = writing.join();
        }
    }
}

/// One message from a view. Whatever it asked, it is answered on its own connection.
///
/// **With no lock of the broker's held.** A channel's session is looked up once, when the view attaches,
/// and what it asks from then on goes to that session directly (`session::Handle`): a search of one
/// session's history is no other view's wait, and a session whose program has stopped reading holds up
/// nothing but its own input.
fn carry_out_view(
    broker: &Broker,
    connection: u64,
    sink: &Arc<Sink>,
    attached: &mut HashMap<u32, Handle>,
    message: ToBroker,
) {
    let send = |message: Vec<u8>| {
        sink.deliver(message);
    };
    // The channel whose session turned out to be over, if this message found one
    let ended: Cell<Option<u32>> = Cell::new(None);
    let failed = |channel: u32, request: u32, refusal: Refusal| {
        if refusal.code == ErrorCode::NoSuchSession {
            ended.set(Some(channel));
        }
        if let Ok(told) = (ToView::Failed { channel, request, message: refusal.message }).encode() {
            send(told);
        }
    };
    // An answer too long to be one is a refusal, to the request it would have answered
    let answer = |channel: u32, request: u32, message: ToView| match message.encode() {
        Ok(message) => send(message),
        Err(too_long) => failed(channel, request, Refusal { code: ErrorCode::Pty, message: too_long.to_string() }),
    };
    // What a view sends that has no answer still says when its session is over
    let unanswered = |channel: u32, outcome: Result<(), Refusal>| {
        if outcome.is_err_and(|refusal| refusal.code == ErrorCode::NoSuchSession) {
            ended.set(Some(channel));
        }
    };
    let unattached =
        || Refusal { code: ErrorCode::NoSuchSession, message: "this channel is not attached to a session".to_owned() };
    let id = |channel: u32| ViewId { connection, channel };
    let session_of = |channel: u32| attached.get(&channel).ok_or_else(unattached);

    match message {
        ToBroker::Attach { channel, session } => {
            if let Some(previous) = attached.remove(&channel) {
                previous.detach_view(id(channel));
            }
            let handle = broker.sessions.handle(&session);
            match handle.and_then(|handle| handle.attach_view(id(channel), Arc::clone(sink)).map(|()| handle)) {
                Ok(handle) => {
                    attached.insert(channel, handle);
                }
                // A session that is not there is one that ended: the view is told what it would have been
                // told had it been attached when it did
                Err(refusal) if refusal.code == ErrorCode::NoSuchSession => {
                    answer(channel, 0, ToView::Ended { channel })
                }
                // Anything else is a session that is there and could not be shown, which is not an ending
                Err(refusal) => failed(channel, 0, refusal),
            }
        }
        ToBroker::Detach { channel } => {
            if let Some(handle) = attached.remove(&channel) {
                handle.detach_view(id(channel));
            }
        }
        ToBroker::Acknowledge { channel, sequence } => {
            if let Ok(handle) = session_of(channel) {
                unanswered(channel, handle.acknowledge(id(channel), sequence));
            }
        }
        ToBroker::Fetch { channel, request, epoch, first, count } => {
            match session_of(channel).and_then(|handle| handle.fetch(epoch, first, count)) {
                Ok(fetched) => match wire::rows(channel, request, epoch, fetched.first, fetched.count, &fetched.rows) {
                    Ok(message) => send(message),
                    Err(too_long) => {
                        failed(channel, request, Refusal { code: ErrorCode::Pty, message: too_long.to_string() });
                    }
                },
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        // Input has no answer: a key that could not be sent is a session that has ended, which the view
        // is told about on its own
        ToBroker::Key { channel, action, modifiers, code, text, unshifted, composed } => {
            if let Ok(handle) = session_of(channel) {
                let key = Key { action, code: &code, modifiers, text: &text, unshifted, composed };
                unanswered(channel, handle.key(&key));
            }
        }
        ToBroker::Text { channel, text } => {
            if let Ok(handle) = session_of(channel) {
                unanswered(channel, handle.text(&text));
            }
        }
        ToBroker::Pointer { channel, action, button, modifiers, x, y, cell_width, cell_height } => {
            if let Ok(handle) = session_of(channel) {
                let pointer = Pointer { action, button, modifiers, x, y, cell_width, cell_height };
                unanswered(channel, handle.pointer(id(channel), &pointer));
            }
        }
        ToBroker::Focus { channel, gained } => {
            if let Ok(handle) = session_of(channel) {
                unanswered(channel, handle.focus(id(channel), gained));
            }
        }
        ToBroker::Paste { channel, request, allow_unsafe, text } => {
            match session_of(channel).and_then(|handle| handle.paste(&text, allow_unsafe)) {
                Ok(pasted) => answer(channel, request, ToView::Pasted { channel, request, outcome: u8::from(!pasted) }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::WordAt { channel, request, epoch, at } => {
            match session_of(channel).and_then(|handle| handle.word_at(epoch, at)) {
                Ok(span) => answer(channel, request, ToView::Span { channel, request, epoch, span }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::LineAt { channel, request, epoch, at } => {
            match session_of(channel).and_then(|handle| handle.line_at(epoch, at)) {
                Ok(span) => answer(channel, request, ToView::Span { channel, request, epoch, span }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::TextOf { channel, request, epoch, span } => {
            match session_of(channel).and_then(|handle| handle.text_of(epoch, span)) {
                Ok(text) => answer(channel, request, ToView::Text { channel, request, epoch, text }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::Search { channel, request, needle, case_sensitive } => {
            match session_of(channel).and_then(|handle| handle.find(&needle, case_sensitive)) {
                Ok((epoch, spans)) => answer(channel, request, ToView::Spans { channel, request, epoch, spans }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::Commands { channel, request } => match session_of(channel).and_then(Handle::commands) {
            Ok((epoch, commands)) => {
                let commands = commands
                    .into_iter()
                    .map(|one| CommandRow {
                        id: one.id,
                        command: one.command,
                        cwd: one.cwd,
                        started_ms: one.started_ms,
                        finished_ms: one.finished_ms,
                        outcome: one.outcome.into(),
                        prompt: one.prompt,
                        output: one.output,
                        end: one.end,
                    })
                    .collect();
                answer(channel, request, ToView::Commands { channel, request, epoch, commands });
            }
            Err(refusal) => failed(channel, request, refusal),
        },
        ToBroker::Images { channel, request } => match session_of(channel).and_then(Handle::images) {
            Ok((_, _, placed)) if placed.len() > broker.images_listed => answer(
                channel,
                request,
                ToView::Failed {
                    channel,
                    request,
                    message: format!(
                        "the session holds {} images, and {} at most are listed",
                        placed.len(),
                        broker.images_listed
                    ),
                },
            ),
            Ok((epoch, (cell_width, cell_height), placed)) => {
                let placements = placed
                    .into_iter()
                    .map(|(line, one)| Placement {
                        image: one.image,
                        generation: one.generation,
                        z: one.z,
                        line,
                        column: one.column,
                        columns: one.columns,
                        rows: one.rows,
                        offset_x: one.offset.0,
                        offset_y: one.offset.1,
                        width: one.size.0,
                        height: one.size.1,
                        source_x: one.source.0,
                        source_y: one.source.1,
                        source_width: one.source.2,
                        source_height: one.source.3,
                    })
                    .collect();
                answer(
                    channel,
                    request,
                    ToView::Images { channel, request, epoch, cell_width, cell_height, placements },
                );
            }
            Err(refusal) => failed(channel, request, refusal),
        },
        ToBroker::Image { channel, request, image, offset } => {
            match session_of(channel).and_then(|handle| handle.image(image, offset)) {
                Ok(Some(read)) => answer(
                    channel,
                    request,
                    ToView::Image {
                        channel,
                        request,
                        image,
                        generation: read.generation,
                        width: read.width,
                        height: read.height,
                        channels: read.channels,
                        total: read.total,
                        offset,
                        bytes: read.bytes,
                    },
                ),
                Ok(None) => {
                    answer(channel, request, ToView::Failed { channel, request, message: format!("no image {image}") })
                }
                Err(refusal) => failed(channel, request, refusal),
            }
        }
    }
    // A channel whose session is over is attached to nothing, whether or not the view says so
    if let Some(channel) = ended.take() {
        attached.remove(&channel);
    }
}

fn end_everything(broker: &Broker) {
    broker.sessions.end_unkept();
}

/// Whether the broker has nothing left to be: nobody connected, and no session still running.
///
/// **A session that was asked to be kept is a reason to stay** (`Request::Keep`), so a broker nobody is
/// connected to is not on its own a broker with nothing to do. It is asked at the moments either half
/// can become true — a connection closing, and a session's process ending — and once more for a broker
/// nothing ever connected to.
fn nothing_left(broker: &Broker) -> bool {
    broker.connections.load(Ordering::SeqCst) == 0 && broker.sessions.none_live()
}

/// One request from a client. `connection` is which connection asked, which is whose a recording opened
/// here is.
///
/// **A session's own work is done on its `Handle`**, after the record of sessions has been let go: what
/// one request costs is that session's and no other's.
fn carry_out(live: &Sessions, connection: u64, request: Request) {
    let outcome = match request {
        Request::Hello { .. } | Request::Done => return,
        Request::Spawn {
            cols,
            rows,
            shell,
            args,
            env,
            unset,
            cwd,
            colors,
            output,
            record,
            restore,
            images,
            image_files,
            marks_key,
        } => {
            let colors = colors.map(|colors| Colors {
                foreground: colors.foreground,
                background: colors.background,
                cursor: colors.cursor,
                ansi: colors.ansi,
            });
            live.spawn(Spawning {
                cols,
                rows,
                shell,
                args,
                env,
                unset,
                cwd,
                colors,
                output,
                record,
                restore,
                images,
                image_files,
                marks_key,
            })
            // What it started is said by the spawn itself, ahead of anything the session can say
            .map_err(|refusal| (None, refusal))
        }
        Request::Write { session, text } => {
            live.handle(&session).and_then(|handle| handle.write(&text)).map_err(|refusal| (Some(session), refusal))
        }
        Request::Resize { session, cols, rows, cell_width, cell_height } => live
            .handle(&session)
            .and_then(|handle| handle.resize(cols, rows, cell_width, cell_height))
            .map_err(|refusal| (Some(session), refusal)),
        Request::SetCursorDefaults { session, style, blink } => live
            .handle(&session)
            .and_then(|handle| handle.set_cursor_defaults(style, blink))
            .map_err(|refusal| (Some(session), refusal)),
        Request::Pause { session } => live.pause(&session).map_err(|refusal| (Some(session), refusal)),
        Request::Resume { session } => live.resume(&session).map_err(|refusal| (Some(session), refusal)),
        Request::Signal { session, signal, scope } => {
            live.signal(&session, signal, scope).map_err(|refusal| (Some(session), refusal))
        }
        Request::Screen { session, history } => {
            match live.handle(&session).and_then(|handle| handle.snapshot(history)) {
                Ok(snapshot) => {
                    live.answer(Event::Screen {
                        session,
                        lines: snapshot.lines,
                        wrapped: snapshot.wrapped,
                        cursor_row: snapshot.cursor_row,
                        cursor_column: snapshot.cursor_column,
                        alt_screen: snapshot.alt_screen,
                    });
                    Ok(())
                }
                Err(refusal) => Err((Some(session), refusal)),
            }
        }
        Request::Owns { session, pid } => match live.owns(&session, pid) {
            Ok(owned) => {
                live.answer(Event::Owns { session, pid, owned });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::Facts { session } => {
            // What the shell said and what the pty measures, one after the other: nothing holds the
            // process still between them, and nothing could
            match live.handle(&session).and_then(|handle| Ok((handle.facts()?, handle.foreground()))) {
                Ok(((facts, vouching), foreground)) => {
                    live.answer(Event::Facts {
                        session,
                        cwd: facts.cwd,
                        command: facts.command,
                        running: facts.running,
                        outcome: facts.outcome.map(Into::into),
                        finished: facts.finished,
                        vouched: vouching.keyed,
                        refused: vouching.refused,
                        foreground,
                    });
                    Ok(())
                }
                Err(refusal) => Err((Some(session), refusal)),
            }
        }
        Request::Search { session, text, history } => {
            match live.handle(&session).and_then(|handle| handle.search(&text, history)) {
                Ok(found) => {
                    live.answer(Event::Search {
                        session,
                        matches: found
                            .into_iter()
                            .map(|found| SearchMatch { line: found.line, column: found.column, text: found.text })
                            .collect(),
                    });
                    Ok(())
                }
                Err(refusal) => Err((Some(session), refusal)),
            }
        }
        Request::Commands { session } => match live.handle(&session).and_then(|handle| handle.commands()) {
            Ok((_, commands)) => {
                live.answer(Event::Commands {
                    session,
                    commands: commands
                        .into_iter()
                        .map(|one| CommandRecord {
                            id: u32::try_from(one.id).unwrap_or(u32::MAX),
                            command: one.command,
                            cwd: one.cwd,
                            started_ms: one.started_ms,
                            finished_ms: one.finished_ms,
                            outcome: one.outcome.into(),
                        })
                        .collect(),
                });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::CommandOutput { session, command } => {
            match live.handle(&session).and_then(|handle| handle.command_output(u64::from(command))) {
                Ok(text) => {
                    live.answer(Event::CommandOutput { session, command, text });
                    Ok(())
                }
                Err(refusal) => Err((Some(session), refusal)),
            }
        }
        // The answer is the `Waited` the wait itself sends, whenever it ends — so unlike every other
        // request this one is accepted here and answered later
        Request::Wait { session, until, timeout_ms } => {
            live.wait(&session, until, timeout_ms).map_err(|refusal| (Some(session), refusal))
        }
        Request::Keep { session, kept } => match live.keep(&session, kept) {
            Ok(()) => {
                live.answer(Event::Kept { session, kept });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::Sessions => {
            let sessions = live.summaries();
            live.answer(Event::Sessions { sessions });
            Ok(())
        }
        Request::Rewind { recording, cols, rows } => match live.rewind(connection, &recording, cols, rows) {
            Ok((session, first, last)) => {
                live.answer(Event::Rewound { session, first, last });
                Ok(())
            }
            Err(refusal) => Err((None, refusal)),
        },
        Request::Seek { session, at } => match live.handle(&session).and_then(|handle| handle.seek(at)) {
            Ok(at) => {
                live.answer(Event::Sought { session, at });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::Kill { session } => live.kill(&session).map_err(|refusal| (Some(session), refusal)),
    };
    if let Err((session, refusal)) = outcome {
        live.answer(Event::Failed { session, code: refusal.code, message: refusal.message });
    }
}
