//! Combined high-load concurrency stress test: fast PTY output flood + concurrent resizes +
//! view attach/acknowledge/detach/reattach + clean session exit.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use apack_terminal::protocol::{Event, PROTOCOL_VERSION, Request, Role};
use apack_terminal::view::wire::{Incoming, ToBroker, ToView, read_message};
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName, prelude::*};

struct Broker {
    child: Child,
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Broker {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("stress.sock");
        let mut child = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
            .arg(&socket)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("broker spawns");
        let mut line = String::new();
        let stdout = child.stdout.take().expect("stdout");
        BufReader::new(stdout).read_line(&mut line).expect("listening line");
        assert!(line.starts_with("listening "), "unexpected startup line: {line:?}");
        Self { child, socket, _dir: dir }
    }

    fn connect(&self) -> Stream {
        let name = self.socket.to_str().expect("utf-8").to_fs_name::<GenericFilePath>().expect("fs name");
        Stream::connect(name).expect("connects")
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn send_json(stream: &Stream, request: &Request) {
    let mut line = serde_json::to_string(request).expect("serializes");
    line.push('\n');
    (&*stream).write_all(line.as_bytes()).expect("writes");
}

/// **Fast PTY output + concurrent resizes + repeated view attach/ack/detach/reattach finishes
/// cleanly with no deadlock, panic, or lost exit event.**
#[test]
fn fast_pty_concurrent_resizes_and_view_churn_completes_cleanly() {
    let broker = Broker::start();
    let client = Arc::new(broker.connect());
    send_json(&client, &Request::Hello { protocol: PROTOCOL_VERSION, role: Role::Client, challenge: None });

    let (events_tx, events_rx) = mpsc::channel::<Event>();
    std::thread::spawn({
        let client = Arc::clone(&client);
        move || {
            let mut reader = BufReader::new(&*client);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(event) = serde_json::from_str::<Event>(&line)
                            && events_tx.send(event).is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    });

    let greeted = events_rx.recv_timeout(Duration::from_secs(10)).expect("hello");
    assert!(matches!(greeted, Event::Hello { .. }));

    // Spawn a fast producer that emits ~40,000 lines with ANSI styles and exits cleanly.
    send_json(
        &client,
        &Request::Spawn {
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".to_owned()),
            args: vec![
                "-c".to_owned(),
                "i=0; while [ $i -lt 20000 ]; do printf '\\033[32mline-%d-abcdefghijklmnopqrstuvwxyz\\033[0m\\n' $i; i=$((i+1)); done"
                    .to_owned(),
            ],
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
        },
    );

    let session = loop {
        match events_rx.recv_timeout(Duration::from_secs(10)).expect("spawned") {
            Event::Spawned { session, .. } => break session,
            Event::Failed { message, .. } => panic!("spawn failed: {message}"),
            _ => {}
        }
    };

    let done = Arc::new(AtomicBool::new(false));

    // Resize thrasher: alternates grid sizes concurrently while the PTY floods
    let resizer = std::thread::spawn({
        let client = Arc::clone(&client);
        let session = session.clone();
        let done = Arc::clone(&done);
        move || {
            let sizes = [(40u16, 12u16), (120, 36), (80, 24), (60, 18)];
            let mut idx = 0usize;
            while !done.load(Ordering::Relaxed) {
                let (cols, rows) = sizes[idx % sizes.len()];
                idx += 1;
                send_json(
                    &client,
                    &Request::Resize { session: session.clone(), cols, rows, cell_width: 9, cell_height: 18 },
                );
                std::thread::sleep(Duration::from_millis(15));
            }
        }
    });

    // View churner: repeatedly attaches, receives and acknowledges a few frames, detaches, and reconnects
    let view_stream = broker.connect();
    send_json(&view_stream, &Request::Hello { protocol: PROTOCOL_VERSION, role: Role::View, challenge: None });

    let viewer = std::thread::spawn({
        let session = session.clone();
        let done = Arc::clone(&done);
        move || {
            let mut view_reader = BufReader::new(&view_stream);
            let mut hello_line = String::new();
            view_reader.read_line(&mut hello_line).expect("view hello");
            assert!(matches!(serde_json::from_str::<Event>(&hello_line), Ok(Event::Hello { .. })));

            let mut channel = 1u32;
            let mut frames_seen = 0usize;
            while !done.load(Ordering::Relaxed) {
                let attach = ToBroker::Attach { channel, session: session.clone() }.encode();
                if (&view_stream).write_all(&attach).is_err() {
                    break;
                }
                for _ in 0..3 {
                    match read_message(&mut view_reader) {
                        Incoming::Message(body) => match ToView::decode(&body) {
                            Ok(ToView::Frame(frame)) => {
                                frames_seen += 1;
                                let ack =
                                    ToBroker::Acknowledge { channel: frame.channel, sequence: frame.sequence }.encode();
                                if (&view_stream).write_all(&ack).is_err() {
                                    return frames_seen;
                                }
                            }
                            Ok(ToView::Ended { .. }) => return frames_seen,
                            Ok(_) => {}
                            Err(err) => panic!("malformed ToView under load: {err}"),
                        },
                        Incoming::TooLong => panic!("oversized ToView frame under load"),
                        Incoming::Ended => return frames_seen,
                    }
                }
                let detach = ToBroker::Detach { channel }.encode();
                if (&view_stream).write_all(&detach).is_err() {
                    break;
                }
                channel += 1;
            }
            frames_seen
        }
    });

    // Wait for the flood command to exit cleanly
    let exit_code = loop {
        match events_rx.recv_timeout(Duration::from_secs(30)).expect("session must exit within 30s") {
            Event::Exited { session: ended, code, .. } if ended == session => break code,
            _ => {}
        }
    };
    done.store(true, Ordering::Relaxed);
    resizer.join().expect("resizer exits");
    let _ = viewer.join().expect("viewer exits cleanly");
    assert_eq!(exit_code, Some(0), "flood command exited cleanly");
}
