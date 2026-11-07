//! The apack terminal broker: one process that owns ptys so no session's lifetime is the app's.
//!
//! `apack-terminal <socket>` listens there and speaks newline-delimited JSON (`protocol`). The skeleton this
//! phase asks for serves **one connection at a time**: a second client is Phase B's, and the goal says to
//! build the thinnest path through every seam before either consumer exists, so that the protocol is derived
//! from its clients rather than proposed to them.

mod output;
mod protocol;
mod session;

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName, prelude::*};

use output::channel_with_credit;
use protocol::{ErrorCode, Event, PROTOCOL_VERSION, Request};
use session::Sessions;

fn main() -> std::io::Result<()> {
    let Some(socket) = std::env::args().nth(1) else {
        eprintln!("usage: apack-terminal <socket>");
        std::process::exit(2);
    };
    let name = socket.as_str().to_fs_name::<GenericFilePath>()?;
    // `try_overwrite` replaces the socket a previous run left behind, which would otherwise refuse the bind:
    // the app owns one broker per data dir, so the path is ours to replace rather than to share
    let listener = ListenerOptions::new().name(name).try_overwrite(true).create_sync()?;

    // Ready on stdout before the first accept, so a parent knows when to connect rather than retrying
    println!("listening {socket}");
    std::io::stdout().flush()?;

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => serve(stream),
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

/// One client: its sessions live as long as the connection, which is the skeleton's lifetime and not the
/// goal's — surviving a client is Phase B's, and nothing here is shaped to prevent it.
fn serve(stream: Stream) {
    let (events, outgoing, credit) = channel_with_credit();
    let sessions = Arc::new(Mutex::new(Sessions::new(events, credit)));

    // **One stream in an `Arc`, not two halves.** `&Stream` implements `Read` and `Write`, which is what the
    // crate's deprecated `split` says to do instead — and dropping a half there does not shut it down, so
    // the halves were a shape that reads like a socket's and is not one.
    let stream = Arc::new(stream);
    // One writer: every event leaves on this thread, so nothing interleaves two frames
    let writing = Arc::clone(&stream);
    let writer = std::thread::spawn(move || {
        for message in outgoing {
            let Ok(frame) = serde_json::to_string(&message.event) else { continue };
            let written = (&*writing).write_all(frame.as_bytes()).and_then(|()| (&*writing).write_all(b"\n"));
            // **The credit returns once the bytes are gone**, not when the message was queued: that is what
            // makes the bound mean "in flight to the window" rather than "allocated".
            message.written();
            if written.is_err() {
                break;
            }
        }
    });

    let reading = Arc::clone(&stream);
    let mut lines = BufReader::new(&*reading).lines();
    let mut greeted = false;
    while let Some(Ok(line)) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Request>(&line) {
            Ok(request) => request,
            Err(error) => {
                refuse(&sessions, None, ErrorCode::Malformed, format!("{error}"));
                continue;
            }
        };
        // **The handshake is first or the connection is refused**, because the monorepo pins a commit of this
        // repository: a stale pin has to be a clear refusal rather than a message that means something else.
        match (&request, greeted) {
            (Request::Hello { protocol }, _) => {
                if *protocol != PROTOCOL_VERSION {
                    refuse(&sessions, None, ErrorCode::Handshake,
                        format!("the app speaks protocol {protocol}; this broker speaks {PROTOCOL_VERSION}"));
                    break;
                }
                greeted = true;
                send(&sessions, Event::Hello { protocol: PROTOCOL_VERSION });
                continue;
            }
            (_, false) => {
                refuse(&sessions, None, ErrorCode::Handshake, "the first message must be hello".to_owned());
                break;
            }
            _ => {}
        }
        carry_out(&sessions, request);
    }

    // **A client that disconnects leaves no shells running.** Its sessions are its own for now, and the app
    // cannot end them on the way out: a pack's `onShutdown` is synchronous and runs in `process.on('exit')`,
    // where no socket write completes. So this is the backstop for an app that quits, crashes or is killed.
    if let Ok(mut live) = sessions.lock() {
        live.end_all();
    }
    drop(sessions);
    let _ = writer.join();
}

fn carry_out(sessions: &Arc<Mutex<Sessions>>, request: Request) {
    let mut live = match sessions.lock() {
        Ok(live) => live,
        Err(_) => return,
    };
    let outcome = match request {
        Request::Hello { .. } => return,
        Request::Spawn { cols, rows, shell, cwd } => match live.spawn(cols, rows, shell, cwd) {
            Ok((session, pid)) => {
                live.answer(Event::Spawned { session, pid });
                Ok(())
            }
            Err(refusal) => Err((None, refusal)),
        },
        Request::Write { session, data } => live.write(&session, &data).map_err(|refusal| (Some(session), refusal)),
        Request::Resize { session, cols, rows } => live.resize(&session, cols, rows).map_err(|refusal| (Some(session), refusal)),
        Request::Kill { session } => live.kill(&session).map_err(|refusal| (Some(session), refusal)),
    };
    if let Err((session, refusal)) = outcome {
        live.answer(Event::Failed { session, code: refusal.code, message: refusal.message });
    }
}

fn send(sessions: &Arc<Mutex<Sessions>>, event: Event) {
    if let Ok(live) = sessions.lock() {
        live.answer(event);
    }
}

fn refuse(sessions: &Arc<Mutex<Sessions>>, session: Option<String>, code: ErrorCode, message: String) {
    send(sessions, Event::Failed { session, code, message });
}
