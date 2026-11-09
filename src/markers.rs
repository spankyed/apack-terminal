//! The markers a shell emits about itself, scanned out of the byte stream the screen parses.
//!
//! **Why these are not the grid's job.** `alacritty_terminal` implements the OSC codes a terminal acts on —
//! a title, a colour, the clipboard — and drops the rest, which is correct: a sequence the terminal has no
//! behaviour for must not disturb the screen. But shell integration *is* those dropped sequences, so the
//! facts they carry have to be taken before the grid ignores them. This scans the same bytes, in parallel,
//! and the stream reaches the grid untouched.
//!
//! **OSC 633, because VS Code's set is the one shells already emit.** Its rc scripts are MIT and readable,
//! other tools emit it, and anything a terminal invents here buys nothing and loses interop. OSC 7 and
//! iTerm2's 1337 are read too, for a user whose own configuration already emits one.
//!
//! **Everything here is untrusted.** A marker's payload is written by whatever is running in the pty, which
//! may be a hostile program rather than the shell, so a payload is length-capped while it is being
//! collected, carries no authority, and is a report of what was *claimed* rather than a fact about the
//! machine. Deciding what to do about a claimed path is the app's; refusing to accumulate an unbounded one
//! is this scanner's.

/// How much of one OSC payload is collected before the sequence is abandoned.
///
/// **A ceiling on what a hostile program can make the broker hold**, which is the whole reason it exists: an
/// OSC string ends with BEL or ST, and a program that emits neither would otherwise grow this buffer for as
/// long as it kept writing. Paths and command lines are the payloads with a real length, and both are far
/// inside this.
///
/// **Public because it is a contract rather than a tuning knob**: it says how long a payload a shell may
/// emit and still be read, so the case that proves a payload of exactly this length is taken has to be able
/// to name it rather than restate the number.
pub const PAYLOAD_CAP: usize = 4 * 1024;

/// One thing a shell said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// `633;A` — the shell is about to draw a prompt.
    PromptStart,
    /// `633;B` — the prompt is drawn; what follows is what the user types.
    PromptEnd,
    /// `633;C` — the command is about to run; what follows is its output.
    CommandStart,
    /// `633;D[;code]` — the command finished. `None` when the shell reported no status.
    CommandDone(Option<i32>),
    /// `633;E;<line>` — the command line the shell is about to run, as the shell read it.
    CommandLine(String),
    /// A working directory: `633;P;Cwd=`, OSC 7's `file://` URL, or 1337's `CurrentDir=`.
    Cwd(String),
}

/// Where the scanner is in the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    /// Ordinary output.
    Text,
    /// An `ESC` was seen and what follows decides whether this is an OSC at all.
    Escape,
    /// Inside an OSC payload.
    Payload,
    /// Inside a payload, having just seen an `ESC`: `\` ends the string, anything else is payload.
    PayloadEscape,
    /// The payload overran `PAYLOAD_CAP`; bytes are discarded until the string ends.
    Overrun,
    /// Inside an abandoned payload, having just seen an `ESC`. **Its own state rather than
    /// `PayloadEscape`**, so that an `ESC` which turns out not to terminate puts the scanner back to
    /// discarding instead of starting to collect the tail of a string it has already given up on.
    OverrunEscape,
}

/// Scans OSC strings out of a byte stream.
///
/// **State is carried across calls, because a chunk boundary falls wherever `read` returned.** A marker
/// split down the middle — and a 23-byte `ESC ] 6 3 3 ; P ; C w d = …` very often is — must be read the same
/// as one that arrived whole. `tests/markers.rs` feeds every stream at every boundary and demands the same
/// markers, which is the only way this is checkable.
#[derive(Debug, Default)]
pub struct Markers {
    state: Option<Scan>,
    payload: Vec<u8>,
}

impl Markers {
    /// Feeds bytes and answers the markers that completed in them.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Marker> {
        let mut found = Vec::new();
        for &byte in bytes {
            match self.state.unwrap_or(Scan::Text) {
                Scan::Text => {
                    if byte == 0x1b {
                        self.state = Some(Scan::Escape);
                    }
                }
                Scan::Escape => {
                    // `ESC ]` opens an OSC string; `ESC ESC` leaves us waiting on the next byte, and
                    // anything else is some other escape sequence that is none of this scanner's business
                    self.state = match byte {
                        b']' => {
                            self.payload.clear();
                            Some(Scan::Payload)
                        }
                        0x1b => Some(Scan::Escape),
                        _ => Some(Scan::Text),
                    };
                }
                Scan::Payload => match byte {
                    // BEL ends an OSC string, and so does ST (`ESC \`)
                    0x07 => {
                        if let Some(marker) = marker(&self.payload) {
                            found.push(marker);
                        }
                        self.payload.clear();
                        self.state = Some(Scan::Text);
                    }
                    0x1b => self.state = Some(Scan::PayloadEscape),
                    _ => {
                        self.payload.push(byte);
                        if self.payload.len() > PAYLOAD_CAP {
                            self.payload.clear();
                            self.state = Some(Scan::Overrun);
                        }
                    }
                },
                Scan::PayloadEscape => {
                    if byte == b'\\' {
                        if let Some(marker) = marker(&self.payload) {
                            found.push(marker);
                        }
                        self.payload.clear();
                        self.state = Some(Scan::Text);
                    } else {
                        // Not a string terminator, so the ESC was payload after all
                        self.payload.push(0x1b);
                        self.payload.push(byte);
                        self.state = Some(Scan::Payload);
                    }
                }
                Scan::Overrun => {
                    self.state = Some(match byte {
                        0x07 => Scan::Text,
                        0x1b => Scan::OverrunEscape,
                        _ => Scan::Overrun,
                    });
                }
                Scan::OverrunEscape => {
                    self.state = Some(match byte {
                        b'\\' => Scan::Text,
                        0x1b => Scan::OverrunEscape,
                        _ => Scan::Overrun,
                    });
                }
            }
        }
        found
    }
}

/// What one completed OSC payload says, or nothing when it says none of our business.
fn marker(payload: &[u8]) -> Option<Marker> {
    // Lossy on purpose: a payload is whatever a program wrote, and invalid UTF-8 in a path is a claim to be
    // read and reported rather than a reason to drop the marker
    let text = String::from_utf8_lossy(payload);

    if let Some(rest) = text.strip_prefix("633;") {
        let (code, argument) = match rest.split_once(';') {
            Some((code, argument)) => (code, Some(argument)),
            None => (rest, None),
        };
        return match (code, argument) {
            ("A", _) => Some(Marker::PromptStart),
            ("B", _) => Some(Marker::PromptEnd),
            ("C", _) => Some(Marker::CommandStart),
            // A shell that reports no status is reporting that it has none, which is not the same as zero
            ("D", status) => Some(Marker::CommandDone(status.and_then(|status| status.trim().parse().ok()))),
            ("E", Some(line)) => Some(Marker::CommandLine(decode(line))),
            // `P` carries `key=value` properties, of which the cwd is the one with a fact in it
            ("P", Some(property)) => property.strip_prefix("Cwd=").map(|path| Marker::Cwd(decode(path))),
            _ => None,
        };
    }

    if let Some(url) = text.strip_prefix("7;") {
        return cwd_from_url(url).map(Marker::Cwd);
    }

    if let Some(path) = text.strip_prefix("1337;CurrentDir=") {
        return Some(Marker::Cwd(decode(path)));
    }

    None
}

/// The path out of OSC 7's `file://<host>/<path>`.
///
/// **Shared with the `vt` path rather than written twice.** The core stores whatever bytes the shell
/// emitted for OSC 7 without parsing them, which its own documentation says, so this decode is the
/// broker's either way — one of the pieces of `markers.rs` that stays when the scanning goes.
///
/// The host is whatever the shell believes it is called and is not checked against anything: a path is a
/// claim either way, and a session is local by construction — the broker spawned the process itself.
pub(crate) fn cwd_from_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    Some(decode(&rest[slash..]))
}

/// Percent-decoding, because a path with a space in it arrives as `%20`.
///
/// **One decoder for every marker that carries a path**, rather than one per sequence: OSC 7 is a URL so it
/// is certainly encoded, and VS Code's own scripts percent-encode `P;Cwd=` too. A stray `%` that is not an
/// escape is kept as itself, which is what a shell that encoded nothing wrote.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let pair = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = pair.and_then(|pair| u8::from_str_radix(pair, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
