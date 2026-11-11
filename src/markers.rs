//! The markers a shell emits about itself that the core does not read.
//!
//! **The core reads the standard ones.** The prompt marks (OSC 133) and the working directory (OSC 7, and
//! iTerm2's 1337) arrive through callbacks of its own (`vt/mod.rs`). It does not read OSC 633, which is
//! VS Code's family and the one most shells already emit — its rc scripts are MIT and readable, other
//! tools emit it, and anything a terminal invented in its place would buy nothing and lose interop.
//!
//! **So the core hands those over whole, and this reads them.** A sequence it has no behaviour for is
//! reported through its unknown-sequence callback, already parsed out of the stream: where one starts and
//! ends, how it is split across reads and how long one may be are the parser's answers, once, rather than
//! a second scanner's beside it.
//!
//! **Everything here is untrusted.** A marker's content is written by whatever is running in the pty, which
//! may be a hostile program rather than the shell, so it is length-capped by the core as it is collected
//! (`PAYLOAD_CAP`), carries no authority, and is a report of what was *claimed* rather than a fact about
//! the machine. Deciding what to do about a claimed path is the app's.

/// How much of one sequence the core keeps for this to read.
///
/// **A ceiling on what a hostile program can make the broker hold.** Paths and command lines are the
/// contents with a real length, and both are far inside this; a longer one arrives cut short and marked
/// so, and is not read as a marker at all.
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
    /// `633;P;Cwd=<path>` — the shell's working directory.
    Cwd(String),
}

/// What one sequence the core did not read says, or nothing when it says none of our business. `content`
/// is everything between the sequence's introducer and its terminator, number included.
pub fn read(content: &[u8]) -> Option<Marker> {
    // Lossy on purpose: content is whatever a program wrote, and invalid UTF-8 in a path is a claim to be
    // read and reported rather than a reason to drop the marker
    let text = String::from_utf8_lossy(content);

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

    None
}

/// The path out of OSC 7's `file://<host>/<path>`.
///
/// The core stores whatever bytes the shell emitted for OSC 7 without parsing them, which its own
/// documentation says, so taking the path out of the URL is the broker's (`vt/mod.rs`, `note_pwd`).
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
        // **Exactly two hexadecimal digits**, asked of each: a number parser also takes a sign, and would
        // read `%+1` as the byte 0x01 where the shell wrote a percent, a plus and a one
        if bytes[index] == b'%'
            && let Some(&[high, low]) = bytes.get(index + 1..index + 3)
            && let (Some(high), Some(low)) = (char::from(high).to_digit(16), char::from(low).to_digit(16))
        {
            out.push((high * 16 + low) as u8);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
