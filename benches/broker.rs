//! P1: the broker's own numbers, measured as a client drives it: output throughput for `seq 1 1000000`, the
//! keystroke round trip into a shell back to back and spaced 60 ms apart, and the broker's resident memory with
//! 1, 10 and 50 idle sessions. Run with `cargo bench --bench broker`.
//!
//! It reports and never fails. A measurement is taken on the machine it names and read against that machine's
//! load, which the caller records (`CLAUDE.md`, "Load inflates a duration and cannot deflate it").
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: u32 = 1;
const SEQ_LAST: u32 = 1_000_000;
const ROUND_TRIPS: usize = 300;
const SPACING: Duration = Duration::from_millis(60);

struct Broker {
    process: Child,
    socket: PathBuf,
    _directory: tempfile::TempDir,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Starts the built broker and waits for its ready line, which the broker prints before its first accept.
fn start() -> Broker {
    let directory = tempfile::tempdir().expect("a directory for the socket");
    let socket = directory.path().join("broker.sock");
    let mut process = Command::new(env!("CARGO_BIN_EXE_apack-terminal"))
        .arg(&socket)
        .stdout(Stdio::piped())
        .spawn()
        .expect("the broker starts");
    let mut ready = String::new();
    BufReader::new(process.stdout.as_mut().expect("piped stdout"))
        .read_line(&mut ready)
        .expect("the broker says where it is listening");
    assert!(ready.starts_with("listening "), "{ready:?}");
    Broker { process, socket, _directory: directory }
}

/// One client connection: its events arrive as JSON lines, read in order.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(broker: &Broker) -> Client {
        let stream = UnixStream::connect(&broker.socket).expect("the broker accepts");
        let reader = BufReader::new(stream.try_clone().expect("a second handle"));
        let mut client = Client { reader, writer: stream };
        client.send(&format!(r#"{{"kind":"hello","protocol":{PROTOCOL_VERSION}}}"#));
        assert_eq!(client.next()["kind"], "hello");
        client
    }

    fn send(&mut self, frame: &str) {
        self.writer.write_all(frame.as_bytes()).expect("the request is written");
        self.writer.write_all(b"\n").expect("the frame ends");
    }

    fn next(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("an event arrives");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
    }

    /// Spawns a shell and returns its session id once the broker says it started.
    fn spawn(&mut self, shell: &str) -> String {
        let frame = serde_json::json!({
            "kind": "spawn", "cols": 120, "rows": 40, "shell": shell,
            "args": [], "env": [], "unset": [], "cwd": null,
        });
        self.send(&frame.to_string());
        loop {
            let event = self.next();
            if event["kind"] == "spawned" {
                return event["session"].as_str().expect("a session id").to_owned();
            }
        }
    }

    fn write(&mut self, session: &str, text: &str) {
        let frame = serde_json::json!({ "kind": "write", "session": session, "text": text });
        self.send(&frame.to_string());
    }

    /// Reads output for a session until `needle` appears in what it has printed since the call, and returns how
    /// many bytes of output came with it.
    fn output_until(&mut self, session: &str, needle: &str) -> usize {
        let mut seen = String::new();
        let mut bytes = 0;
        while !seen.contains(needle) {
            let event = self.next();
            if event["kind"] == "output" && event["session"] == session {
                let text = event["text"].as_str().unwrap_or_default();
                bytes += text.len();
                seen.push_str(text);
            }
        }
        bytes
    }
}

/// The resident memory of a process in megabytes, from `ps` (macOS and Linux both print `rss` in kilobytes).
fn resident_megabytes(pid: u32) -> f64 {
    let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().expect("ps runs");
    let kilobytes: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().expect("an rss figure");
    kilobytes / 1024.0
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn throughput(broker: &Broker) {
    let mut client = Client::connect(broker);
    let session = client.spawn("/bin/sh");
    // The end marker is built from two pieces, so its echo does not contain it: only its output does
    let command = format!("seq 1 {SEQ_LAST}; printf 'BENCH''_END\\n'\n");
    let started = Instant::now();
    client.write(&session, &command);
    let bytes = client.output_until(&session, "BENCH_END");
    let elapsed = started.elapsed();
    let megabytes = bytes as f64 / 1_000_000.0;
    println!(
        "throughput: seq 1 {SEQ_LAST}: {megabytes:.1} MB of output in {:.2} s = {:.2} MB/s",
        elapsed.as_secs_f64(),
        megabytes / elapsed.as_secs_f64()
    );
}

fn keystrokes(broker: &Broker) {
    let mut client = Client::connect(broker);
    let session = client.spawn("/bin/sh");
    // Warm the session: the first echo waits on the shell's own start
    client.write(&session, "w");
    client.output_until(&session, "w");
    client.write(&session, "\u{15}");

    let mut back_to_back = Vec::with_capacity(ROUND_TRIPS);
    for _ in 0..ROUND_TRIPS {
        let started = Instant::now();
        client.write(&session, "x");
        client.output_until(&session, "x");
        back_to_back.push(started.elapsed());
        client.write(&session, "\u{15}");
    }

    let mut spaced = Vec::with_capacity(ROUND_TRIPS);
    for _ in 0..ROUND_TRIPS {
        std::thread::sleep(SPACING);
        let started = Instant::now();
        client.write(&session, "x");
        client.output_until(&session, "x");
        spaced.push(started.elapsed());
        client.write(&session, "\u{15}");
    }

    println!(
        "keystroke echo, back to back ({ROUND_TRIPS}): median {:.2} ms",
        median(back_to_back).as_secs_f64() * 1000.0
    );
    println!(
        "keystroke echo, spaced {} ms apart ({ROUND_TRIPS}): median {:.2} ms",
        SPACING.as_millis(),
        median(spaced).as_secs_f64() * 1000.0
    );
}

fn memory(broker: &Broker) {
    let mut client = Client::connect(broker);
    let mut sessions = 0;
    for target in [1, 10, 50] {
        while sessions < target {
            client.spawn("/bin/cat");
            sessions += 1;
        }
        println!(
            "memory with {target} idle session{}: broker resident {:.1} MB",
            if target == 1 { "" } else { "s" },
            resident_megabytes(broker.process.id())
        );
    }
}

fn main() {
    let broker = start();
    throughput(&broker);
    keystrokes(&broker);
    memory(&broker);
}
