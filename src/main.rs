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

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName, prelude::*};

use apack_terminal::frames::{Frame, read_frame};
use apack_terminal::output::{Outgoing, channel_with_credit};
use apack_terminal::protocol::{CommandRecord, ErrorCode, Event, PROTOCOL_VERSION, Request, Role, SearchMatch};
use apack_terminal::screen::{Colors, HISTORY_LINES};
use apack_terminal::session::{Refusal, Sessions, Spawning};
use apack_terminal::view::publish::ViewId;
use apack_terminal::view::wire::{CommandRow, Incoming, ToBroker, ToView, read_message};
use apack_terminal::vt::input::{Key, Pointer};

fn main() -> std::io::Result<()> {
    let Some(socket) = std::env::args().nth(1) else {
        eprintln!("usage: apack-terminal <socket>");
        std::process::exit(2);
    };
    let name = socket.as_str().to_fs_name::<GenericFilePath>()?;
    // `try_overwrite` replaces the socket a previous run left behind, which would otherwise refuse the bind:
    // the app owns one broker per data dir, so the path is ours to replace rather than to share
    let options = ListenerOptions::new().name(name).try_overwrite(true);
    // **On Windows the pipe is created with a descriptor of its own**, because the default one grants read
    // access to Everyone and what crosses here is the user's terminal output (`security.rs`). On a Unix
    // system the socket is a file in the user's own data directory and the filesystem has already answered
    // this, so there is nothing to add. A descriptor that cannot be built refuses the listen: the
    // alternative is serving every local account a terminal because a token could not be read
    #[cfg(windows)]
    let options = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        options.security_descriptor(apack_terminal::security::owner_only()?)
    };
    let listener = options.create_sync()?;

    // Ready on stdout before the first accept, so a parent knows when to connect rather than retrying
    println!("listening {socket}");
    std::io::stdout().flush()?;

    let (events, outgoing, credit) = channel_with_credit();
    let broker = Broker {
        sessions: Arc::new(Mutex::new(Sessions::new(events, credit))),
        feed: Arc::new(Mutex::new(None)),
        owned: Arc::new(AtomicBool::new(false)),
        clients: Arc::new(AtomicUsize::new(0)),
        connections: Arc::new(AtomicUsize::new(0)),
        views: Arc::new(AtomicU64::new(0)),
    };
    // One writer for the broker's life: every event leaves on this thread, so nothing interleaves two
    // frames, and a client arriving or leaving changes where they go rather than who writes them
    std::thread::spawn({
        let broker = broker.clone();
        move || write_out(outgoing, broker)
    });

    for connection in listener.incoming() {
        match connection {
            // **A thread per connection**, because a lifetime connection and a client now coexist: serving
            // them in turn would mean the app's own hold on the broker blocked every session request
            Ok(stream) => {
                let broker = broker.clone();
                broker.connections.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || serve(stream, broker));
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

/// What every connection shares: the sessions, where their output currently goes, and who holds them.
#[derive(Clone)]
struct Broker {
    sessions: Arc<Mutex<Sessions>>,
    /// The client receiving the feed. A `lifetime` connection never installs itself here — it asks for
    /// nothing and is sent nothing, which is what makes holding one cost no output.
    feed: Arc<Mutex<Option<Arc<Stream>>>>,
    /// Whether a `lifetime` connection is open.
    owned: Arc<AtomicBool>,
    /// How many `client` connections are open.
    clients: Arc<AtomicUsize>,
    /// How many connections of any kind are open, **counted from the accept and not from the handshake**.
    /// That is what makes exiting at zero safe: a caller that has connected but not yet greeted is already
    /// counted, so it cannot be hung up on by a broker deciding nobody wants it.
    connections: Arc<AtomicUsize>,
    /// How many view connections there have been, which is what tells one's views from another's.
    views: Arc<AtomicU64>,
}

/// Writes every event to whichever client is attached, for as long as the broker runs.
fn write_out(outgoing: Receiver<Outgoing>, broker: Broker) {
    for message in outgoing {
        // **A shell that ended of its own accord is gone from here too.** Only `kill` used to remove a
        // session, so a user typing `exit` left its record behind: the map grew with every shell they
        // closed, and a later write answered `PTY` — the OS refusing a dead handle — where the true answer
        // is that there is no such session. On Windows that is X9's window, where ConPTY throws for about a
        // second after the console goes; the same wrong answer, louder.
        if let Event::Exited { session, .. } = &message.event
            && let Ok(mut live) = broker.sessions.lock()
        {
            live.forget(session);
        }
        let frame = serde_json::to_string(&message.event).ok();
        let attached = broker.feed.lock().ok().and_then(|feed| feed.clone());
        if let (Some(frame), Some(stream)) = (frame, attached.as_ref())
            && (&**stream).write_all(frame.as_bytes()).and_then(|()| (&**stream).write_all(b"\n")).is_err()
        {
            // That client is gone; the next one to greet installs itself. Nothing is ended here, because
            // whether a client leaving ends the sessions is `Role`'s question and is answered in `serve`
            detach(&broker, stream);
        }
        // **The credit returns whether or not anyone was listening.** It is what bounds the channel, so a
        // frame dropped for want of a client must not hold a slot — otherwise a restart of the service that
        // was attached would leave the broker permanently short of credit and its output would stall.
        message.written();
    }
}

/// Takes `stream` out of the feed, if it is still the one attached, **and lets every session be read
/// again**.
///
/// A pause belongs to the client that asked for one (`Request::Pause`). The next client to attach never
/// asked and has no way to find out, so a session nobody is reading would look like a shell that had
/// stopped writing. The client that paused it is gone; the pause goes with it.
///
/// **The two locks are taken one after the other and never together.** The order written down on
/// `Sessions` is sessions then a session's screen, and the writer loop releases its `Sessions` guard before
/// it touches the feed — so nothing holds sessions and wants the feed. Resuming inside the feed guard would
/// be the first place that went the other way.
fn detach(broker: &Broker, stream: &Arc<Stream>) {
    let was_attached = match broker.feed.lock() {
        Ok(mut feed) if feed.as_ref().is_some_and(|attached| Arc::ptr_eq(attached, stream)) => {
            *feed = None;
            true
        }
        _ => false,
    };
    if was_attached && let Ok(mut live) = broker.sessions.lock() {
        live.resume_all();
    }
}

/// Answers this connection directly, rather than through the feed.
///
/// **A handshake is between the broker and one caller**, so its answer — and a refusal of a frame that
/// never named a session — goes to whoever asked. Sending those through the shared channel would deliver a
/// refusal of the lifetime connection's malformed frame to whichever client happened to be attached.
fn say(stream: &Arc<Stream>, event: &Event) {
    if let Ok(frame) = serde_json::to_string(event) {
        let _ = (&**stream).write_all(frame.as_bytes()).and_then(|()| (&**stream).write_all(b"\n"));
    }
}

/// One connection, for as long as it is open.
///
/// What it does on the way out is the whole of the lifetime rule: a `lifetime` connection ending ends every
/// session, and a `client` ending does only when no lifetime connection is open and it was the last client.
fn serve(stream: Stream, broker: Broker) {
    // **One stream in an `Arc`, not two halves.** `&Stream` implements `Read` and `Write`, which is what the
    // crate's deprecated `split` says to do instead — and dropping a half there does not shut it down, so
    // the halves were a shape that reads like a socket's and is not one.
    let stream = Arc::new(stream);
    let reading = Arc::clone(&stream);
    let mut reader = BufReader::new(&*reading);
    let mut role: Option<Role> = None;
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
                say(&stream, &Event::Failed { session: None, code: ErrorCode::Malformed, message });
                continue;
            }
            Frame::TooLong(message) => {
                say(&stream, &Event::Failed { session: None, code: ErrorCode::Malformed, message });
                break;
            }
            Frame::Ended => break,
        };
        // **The handshake is first or the connection is refused**, because the monorepo pins a commit of this
        // repository: a stale pin has to be a clear refusal rather than a message that means something else.
        match (&request, role) {
            (Request::Hello { protocol, role: asked }, _) => {
                if *protocol != PROTOCOL_VERSION {
                    say(
                        &stream,
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
                role = Some(*asked);
                match asked {
                    Role::Client => {
                        broker.clients.fetch_add(1, Ordering::SeqCst);
                        // Last greeting wins the feed: one app, one broker per data dir, and a client that
                        // reconnects after its service restarted is the one that should be receiving
                        if let Ok(mut feed) = broker.feed.lock() {
                            *feed = Some(Arc::clone(&stream));
                        }
                    }
                    Role::Lifetime => broker.owned.store(true, Ordering::SeqCst),
                    Role::View => {}
                }
                say(&stream, &Event::Hello { protocol: PROTOCOL_VERSION, history: HISTORY_LINES as u32 });
                if *asked == Role::View {
                    // From here the connection speaks `view::wire`, on the reader that already holds
                    // whatever arrived behind the handshake
                    serve_view(&mut reader, &stream, &broker);
                    break;
                }
                continue;
            }
            (_, None) => {
                say(
                    &stream,
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
        carry_out(&broker.sessions, request);
    }

    match role {
        // **The app is gone, so its shells are.** The descriptor closed, which it does however the process
        // died — quit, crash or kill — so this is reached without anything having remembered to call it.
        Some(Role::Lifetime) => {
            broker.owned.store(false, Ordering::SeqCst);
            end_everything(&broker);
        }
        // **A client leaving ends nothing while an owner holds the broker**, which is what lets the service
        // that opened a session restart without taking it down. With no owner this is the behaviour the
        // broker has always had, and it is what keeps a standalone run — the suite, tooling — from leaving
        // shells behind.
        Some(Role::Client) => {
            detach(&broker, &stream);
            let left = broker.clients.fetch_sub(1, Ordering::SeqCst) - 1;
            if left == 0 && !broker.owned.load(Ordering::SeqCst) {
                end_everything(&broker);
            }
        }
        // A view asked for no session and ends none
        Some(Role::View) | None => {}
    }

    // **A broker nobody is connected to has nothing left to be**, and exiting is the whole of its cleanup:
    // the socket goes with the process, and so does the memory every screen was holding. It used to listen
    // for ever instead, so each run of the app left one behind — harmless on its own, since its sessions
    // were already ended, and a process per launch all the same.
    if broker.connections.fetch_sub(1, Ordering::SeqCst) == 1 {
        end_everything(&broker);
        std::process::exit(0);
    }
}

/// One view connection, from its handshake until it closes.
///
/// **Its own writer, and its own queue.** A client's events share one queue because one client receives
/// them; a view's frames are for that view, and a view that has stopped reading must not hold up another's
/// screen or a client's answer. The queue needs no bound of its own: a session sends a view one frame and
/// then waits to be told it was drawn, so what can be waiting is a frame per attachment and the answers to
/// what the view itself asked.
fn serve_view(reader: &mut BufReader<&Stream>, stream: &Arc<Stream>, broker: &Broker) {
    let connection = broker.views.fetch_add(1, Ordering::SeqCst);
    let (sink, outgoing) = channel::<Vec<u8>>();
    std::thread::spawn({
        let stream = Arc::clone(stream);
        move || {
            for message in outgoing {
                if (&*stream).write_all(&message).is_err() {
                    return;
                }
            }
        }
    });
    // The session each of this connection's channels is attached to
    let mut attached: HashMap<u32, String> = HashMap::new();
    // Until nothing more is coming, or a message so long that where the next one starts is not known
    while let Incoming::Message(body) = read_message(reader) {
        match ToBroker::decode(&body) {
            Ok(message) => carry_out_view(broker, connection, &sink, &mut attached, message),
            // Not the protocol: there is no telling what the rest of the stream is either
            Err(_) => break,
        }
    }
    if let Ok(live) = broker.sessions.lock() {
        live.detach_connection(connection);
    }
}

/// One message from a view. Whatever it asked, it is answered on its own connection.
fn carry_out_view(
    broker: &Broker,
    connection: u64,
    sink: &Sender<Vec<u8>>,
    attached: &mut HashMap<u32, String>,
    message: ToBroker,
) {
    let Ok(live) = broker.sessions.lock() else { return };
    let answer = |message: ToView| {
        let _ = sink.send(message.encode());
    };
    let failed = |channel: u32, request: u32, refusal: Refusal| {
        let _ = sink.send(ToView::Failed { channel, request, message: refusal.message }.encode());
    };
    let unattached =
        || Refusal { code: ErrorCode::NoSuchSession, message: "this channel is not attached to a session".to_owned() };
    let id = |channel: u32| ViewId { connection, channel };
    let session_of = |channel: u32| attached.get(&channel).cloned().ok_or_else(unattached);

    match message {
        ToBroker::Attach { channel, session } => {
            if let Some(previous) = attached.remove(&channel) {
                live.detach_view(&previous, id(channel));
            }
            match live.attach_view(&session, id(channel), sink.clone()) {
                Ok(()) => {
                    attached.insert(channel, session);
                }
                // A session that is not there is one that ended: the view is told what it would have been
                // told had it been attached when it did
                Err(_) => answer(ToView::Ended { channel }),
            }
        }
        ToBroker::Detach { channel } => {
            if let Some(session) = attached.remove(&channel) {
                live.detach_view(&session, id(channel));
            }
        }
        ToBroker::Acknowledge { channel, sequence } => {
            if let Ok(session) = session_of(channel) {
                let _ = live.acknowledge(&session, id(channel), sequence);
            }
        }
        ToBroker::Fetch { channel, request, epoch: _, first, count } => {
            match session_of(channel).and_then(|session| live.fetch(&session, first, count)) {
                Ok((epoch, block)) => answer(ToView::Rows { channel, request, epoch, block }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        // Input has no answer: a key that could not be sent is a session that has ended, which the view
        // is told about on its own
        ToBroker::Key { channel, action, modifiers, code, text, unshifted } => {
            if let Ok(session) = session_of(channel) {
                let _ = live.key(&session, &Key { action, code: &code, modifiers, text: &text, unshifted });
            }
        }
        ToBroker::Text { channel, text } => {
            if let Ok(session) = session_of(channel) {
                let _ = live.text(&session, &text);
            }
        }
        ToBroker::Pointer { channel, action, button, modifiers, x, y, cell_width, cell_height } => {
            if let Ok(session) = session_of(channel) {
                let _ = live.pointer(&session, &Pointer { action, button, modifiers, x, y, cell_width, cell_height });
            }
        }
        ToBroker::Focus { channel, gained } => {
            if let Ok(session) = session_of(channel) {
                let _ = live.focus(&session, gained);
            }
        }
        ToBroker::Paste { channel, request, allow_unsafe, text } => {
            match session_of(channel).and_then(|session| live.paste(&session, &text, allow_unsafe)) {
                Ok(pasted) => answer(ToView::Pasted { channel, request, outcome: u8::from(!pasted) }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::WordAt { channel, request, epoch: _, at } => {
            match session_of(channel).and_then(|session| live.word_at(&session, at)) {
                Ok((epoch, span)) => answer(ToView::Span { channel, request, epoch, span }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::LineAt { channel, request, epoch: _, at } => {
            match session_of(channel).and_then(|session| live.line_at(&session, at)) {
                Ok((epoch, span)) => answer(ToView::Span { channel, request, epoch, span }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::TextOf { channel, request, epoch: _, span } => {
            match session_of(channel).and_then(|session| live.text_of(&session, span)) {
                Ok(text) => answer(ToView::Text { channel, request, text }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::Search { channel, request, needle } => {
            match session_of(channel).and_then(|session| live.find(&session, &needle)) {
                Ok((epoch, spans)) => answer(ToView::Spans { channel, request, epoch, spans }),
                Err(refusal) => failed(channel, request, refusal),
            }
        }
        ToBroker::Commands { channel, request } => {
            match session_of(channel).and_then(|session| live.commands(&session)) {
                Ok((epoch, commands)) => {
                    let commands = commands
                        .into_iter()
                        .map(|one| CommandRow {
                            id: one.id,
                            command: one.command,
                            cwd: one.cwd,
                            started_ms: one.started_ms,
                            finished_ms: one.finished_ms,
                            exit_status: one.exit_status,
                            prompt: one.prompt,
                            output: one.output,
                            end: one.end,
                        })
                        .collect();
                    answer(ToView::Commands { channel, request, epoch, commands });
                }
                Err(refusal) => failed(channel, request, refusal),
            }
        }
    }
}

fn end_everything(broker: &Broker) {
    if let Ok(mut live) = broker.sessions.lock() {
        live.end_all();
    }
}

fn carry_out(sessions: &Arc<Mutex<Sessions>>, request: Request) {
    let mut live = match sessions.lock() {
        Ok(live) => live,
        Err(_) => return,
    };
    let outcome = match request {
        Request::Hello { .. } => return,
        Request::Spawn { cols, rows, shell, args, env, unset, cwd, colors, output } => {
            let colors = colors.map(|colors| Colors {
                foreground: colors.foreground,
                background: colors.background,
                cursor: colors.cursor,
                ansi: colors.ansi,
            });
            match live.spawn(Spawning { cols, rows, shell, args, env, unset, cwd, colors, output }) {
                Ok((session, pid)) => {
                    live.answer(Event::Spawned { session, pid });
                    Ok(())
                }
                Err(refusal) => Err((None, refusal)),
            }
        }
        Request::Write { session, text } => live.write(&session, &text).map_err(|refusal| (Some(session), refusal)),
        Request::Resize { session, cols, rows, cell_width, cell_height } => {
            live.resize(&session, cols, rows, cell_width, cell_height).map_err(|refusal| (Some(session), refusal))
        }
        Request::Pause { session } => live.pause(&session).map_err(|refusal| (Some(session), refusal)),
        Request::Resume { session } => live.resume(&session).map_err(|refusal| (Some(session), refusal)),
        Request::Signal { session, signal, scope } => {
            live.signal(&session, signal, scope).map_err(|refusal| (Some(session), refusal))
        }
        Request::Screen { session, history } => match live.snapshot(&session, history) {
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
        },
        Request::Owns { session, pid } => match live.owns(&session, pid) {
            Ok(owned) => {
                live.answer(Event::Owns { session, pid, owned });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::Facts { session } => match live.facts(&session) {
            Ok(facts) => {
                // The measurement is taken under the same `Sessions` guard as the claims, so the two
                // halves of one answer describe one moment rather than two
                let foreground = live.foreground(&session).unwrap_or(None);
                live.answer(Event::Facts {
                    session,
                    cwd: facts.cwd,
                    command: facts.command,
                    running: facts.running,
                    exit_status: facts.exit_status,
                    finished: facts.finished,
                    foreground,
                });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::Search { session, text, history } => match live.search(&session, &text, history) {
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
        },
        Request::Commands { session } => match live.commands(&session) {
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
                            exit_status: one.exit_status,
                        })
                        .collect(),
                });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        Request::CommandOutput { session, command } => match live.command_output(&session, u64::from(command)) {
            Ok(text) => {
                live.answer(Event::CommandOutput { session, command, text });
                Ok(())
            }
            Err(refusal) => Err((Some(session), refusal)),
        },
        // The answer is the `Waited` the wait itself sends, whenever it ends — so unlike every other
        // request this one is accepted here and answered later
        Request::Wait { session, until, timeout_ms } => {
            live.wait(&session, until, timeout_ms).map_err(|refusal| (Some(session), refusal))
        }
        Request::Kill { session } => live.kill(&session).map_err(|refusal| (Some(session), refusal)),
    };
    if let Err((session, refusal)) = outcome {
        live.answer(Event::Failed { session, code: refusal.code, message: refusal.message });
    }
}
