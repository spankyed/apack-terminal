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
//! **Everything here is untrusted, and one family can be vouched for.** A marker's content is written by
//! whatever is running in the pty — a file shown with `cat`, a log line, a program that is not the shell —
//! so it is length-capped by the core as it is collected (`PAYLOAD_CAP`) and is a report of what was
//! *claimed* rather than a fact about the machine. The exception is a mark that carries a key
//! (`MarksKey`): a session spawned with one gives it to its shell and to nothing else, so a mark carrying
//! it was written by something that was told it. That is proof against content printed into the terminal
//! by something that does not know the key, and against nothing else: code running as the user in that
//! shell can read the key and write any mark it likes.

/// How much of one sequence the core keeps for this to read.
///
/// **A ceiling on what a hostile program can make the broker hold.** Paths and command lines are the
/// contents with a real length, and both are far inside this; a longer one arrives cut short and marked
/// so, which `read` is told.
pub const PAYLOAD_CAP: usize = 4 * 1024;

/// One thing a shell said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// `633;A` — the shell is about to draw a prompt.
    PromptStart,
    /// `633;C` — the command is about to run; what follows is its output.
    CommandStart,
    /// `633;D[;code]` — the command finished. `None` when the shell reported no status.
    CommandDone(Option<i32>),
    /// `633;E;<line>` — the command line the shell is about to run, as the shell read it.
    CommandLine(String),
    /// `633;P;Cwd=<path>` — the shell's working directory.
    Cwd(String),
}

/// What vouches for a session's marks: 32 lowercase hexadecimal characters its shell was given and
/// nothing else was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarksKey(String);

impl MarksKey {
    /// How long a key is, in characters.
    pub const LENGTH: usize = 32;

    /// The key `text` is, or nothing when it is not one: any other length, or any character that is not
    /// a digit or a lowercase `a` to `f`.
    pub fn new(text: &str) -> Option<Self> {
        let hexadecimal = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
        (text.len() == Self::LENGTH && text.bytes().all(hexadecimal)).then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An exit status a mark carrying the session's key reported.
///
/// **Made by reading such a mark and by nothing else**, which is what lets a status with this type be
/// told from one a program merely printed: the field is private and there is no other way to one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(i32);

impl ExitCode {
    pub fn get(self) -> i32 {
        self.0
    }
}

/// A mark that carried the key of the session it was read in: a command beginning or ending, with the
/// number the shell gave that command.
///
/// **Made by `read` and by nothing else**, so holding one is having checked the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vouched {
    number: u64,
    said: Said,
}

/// What a vouched mark says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// `633;V;S;<number>;<key>;<line>` — the command of this number is beginning, with the line the
    /// shell read.
    Start { line: String },
    /// `633;V;F;<number>;<key>;<status>;<cwd>` — the command of this number ended. `status` is absent
    /// when the shell had none to report, and `cwd` when it named no directory.
    Finish { status: Option<ExitCode>, cwd: Option<String> },
}

impl Vouched {
    /// The number the shell gave the command this mark is about.
    pub fn number(&self) -> u64 {
        self.number
    }

    pub fn into_said(self) -> Said {
        self.said
    }
}

/// What one sequence the core did not read turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// One of the family anything can write: a claim.
    Claim(Marker),
    /// A mark carrying the key it was read with.
    Vouched(Vouched),
    /// Spelled as a keyed mark and not accepted as one: the wrong key, a number that is not one, a
    /// field missing, or no key to read it with. **Never read as a claim instead**: a mark that fails
    /// to be vouched for says nothing at all.
    Refused,
}

/// What one sequence the core did not read says, or nothing when it says none of our business. `content`
/// is everything between the sequence's introducer and its terminator, number included; `cut` says the
/// core kept only its beginning (`PAYLOAD_CAP`); `key` is the session's, when it was spawned with one.
///
/// **A claim cut short is not read**: half a path is not a claim about a directory. **A keyed mark cut
/// short still is**, because what vouches for it comes before anything of a length a program chooses —
/// a start keeps as much of its line as arrived, and a finish its status without its directory. A
/// command with a very long line is running all the same, and a session that dropped its start would
/// say nothing is.
pub fn read(content: &[u8], cut: bool, key: Option<&MarksKey>) -> Option<Read> {
    // Lossy on purpose: content is whatever a program wrote, and invalid UTF-8 in a path is a claim to be
    // read and reported rather than a reason to drop the marker
    let text = String::from_utf8_lossy(content);
    let rest = text.strip_prefix("633;")?;
    let (code, argument) = match rest.split_once(';') {
        Some((code, argument)) => (code, Some(argument)),
        None => (rest, None),
    };
    if code == "V" {
        let vouched = key.and_then(|key| vouched(argument?, cut, key));
        return Some(vouched.map_or(Read::Refused, Read::Vouched));
    }
    if cut {
        return None;
    }
    let marker = match (code, argument) {
        ("A", _) => Marker::PromptStart,
        ("C", _) => Marker::CommandStart,
        // A shell that reports no status is reporting that it has none, which is not the same as zero
        ("D", status) => Marker::CommandDone(status.and_then(|status| status.trim().parse().ok())),
        ("E", Some(line)) => Marker::CommandLine(decode(line)),
        // `P` carries `key=value` properties, of which the cwd is the one with a fact in it
        ("P", Some(property)) => Marker::Cwd(decode(property.strip_prefix("Cwd=")?)),
        _ => return None,
    };
    Some(Read::Claim(marker))
}

/// The keyed mark `fields` spells — everything after `633;V;` — when it carries `key`.
///
/// **The number and the key are read before anything a command line or a path could hold**, and the last
/// field is taken whole, separators and all: nothing a program can put in a line or a directory moves
/// where the key is looked for.
fn vouched(fields: &str, cut: bool, key: &MarksKey) -> Option<Vouched> {
    let mut fields = fields.splitn(4, ';');
    let (kind, number, carried, rest) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
    if carried != key.as_str() {
        return None;
    }
    let number = counted(number)?;
    let said = match kind {
        "S" => Said::Start { line: decode(rest) },
        "F" => {
            let (status, cwd) = rest.split_once(';')?;
            let status = if status.is_empty() { None } else { Some(ExitCode(exit_status(status)?)) };
            // A directory the core kept only the beginning of is not the directory
            let cwd = (!cut && !cwd.is_empty()).then(|| decode(cwd));
            Said::Finish { status, cwd }
        }
        _ => return None,
    };
    Some(Vouched { number, said })
}

/// A command's number as a shell writes one: decimal digits, no sign, and no zero in front.
///
/// **Exactly one spelling a number**, which a number parser does not give: it takes a sign and leading
/// zeros, and a mark is refused for anything about it that is not as the shell writes it.
fn counted(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits || text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

/// An exit status as a shell writes one: decimal digits, with a minus sign before them where the
/// platform has negative ones.
fn exit_status(text: &str) -> Option<i32> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
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
