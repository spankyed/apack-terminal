//! The file a program names as an image's source, looked at before the core is told of it.
//!
//! A session started to take images by name (`Terminal::allow_images`) has the core open whatever path a
//! program's output names, and the core opens it before it looks at what it is. Opening a FIFO waits for
//! something to write to it, and the open is made inside the write that parses the session's output — so
//! one name stops the session being read. The core has no way to be handed a file it did not open
//! itself, so what can be done is done here: the name is kept from the core until the command it is in
//! has ended, asked about without being opened, and passed on only if it is a regular file. Any other is
//! left out, and the core answers the command as it answers one whose file it could not open.
//!
//! **Where a string begins is the core's to say, and what its body means is repeated here.** The first is
//! `Terminal::feed_to_string`: a pattern looked for in the bytes would be a second opinion about `ESC`,
//! and would be wrong about the five other ways the core begins such a string. The second cannot be
//! asked, because the core runs a command on the byte that ends it and a name has to be held back before
//! then. So three of the core's rules are written out again, each as short as the rule: which bytes of a
//! string's body are content, are passed over, or end it (`kind`); how a graphics command's control data
//! is read into keys and values (`Control`); and which values of its `t` key mean a file (`names_a_file`).
//! `tests/image_paths.rs` holds all three to the core itself, over streams it makes up.
//!
//! **Erring is towards reading a name that is not one.** Where the core gives up on a command part way
//! — a number that is not one, say — this reads on, and may hold a payload the core would never have
//! opened. What it then feeds is that payload again or nothing, to a core that is ignoring both.
//!
//! **What is not looked at.** A file changed between being asked about and being opened: whatever can do
//! that runs as the user and needs no terminal to open a FIFO. A shared memory object (`t=s`), which is
//! named to `shm_open` as it came. And a file that is slow to answer — on a mount that has gone away —
//! which is slow to be asked about too.

/// The longest path read. No system this runs on opens a longer one, so a name past this is left out
/// without being asked about.
pub const PATH_BYTES: usize = 4096;

/// How much of a name is held: `PATH_BYTES` as base64.
const NAME_BYTES: usize = PATH_BYTES.div_ceil(3) * 4;

/// How much of a key or a value the core keeps. One more byte and it ignores the pair, and every pair
/// after it.
const PIECE_BYTES: usize = 11;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// What a byte is to the core inside an APC, SOS or PM string (the `sos_pm_apc_string` column of its
/// parse table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Byte {
    /// Handed to whatever reads the string
    Content,
    /// Neither content nor an end: the string goes on as if it were not there
    PassedOver,
    /// The string is over. `ESC` and `0x9c` run its command; the rest drop it
    Ends,
}

fn kind(byte: u8) -> Byte {
    match byte {
        // CAN, SUB, ESC, and the 8-bit controls that leave for ground or for a sequence of another kind
        0x18 | 0x1a | 0x1b | 0x80..=0x97 | 0x99..=0x9d => Byte::Ends,
        // The 8-bit spellings of SOS, PM and APC, which begin nothing inside one, and every byte above them
        0x98 | 0x9e..=0xff => Byte::PassedOver,
        _ => Byte::Content,
    }
}

/// Where in a pair of a graphics command's control data the next byte falls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pair {
    Key,
    /// A key too long to be one. Nothing ends it but its `=`, not even the `;` that would begin a payload
    KeyIgnored,
    Value,
    /// The value of a pair that is ignored, or one too long to be a value
    ValueIgnored,
}

/// A graphics command's control data as far as it has been read, kept as the core keeps it
/// (`kitty/graphics_command.zig`, `Parser.feed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Control {
    at: Pair,
    /// The key or value in hand
    piece: [u8; PIECE_BYTES],
    length: usize,
    /// The key the value in hand is for
    key: u8,
    /// Whether the command's source, as the pairs so far leave it, is a file
    file: bool,
}

impl Control {
    const fn new() -> Self {
        Self { at: Pair::Key, piece: [0; PIECE_BYTES], length: 0, key: 0, file: false }
    }

    /// Reads one byte of content. Answers, at the `;` that ends the control data, whether what follows
    /// names a file.
    fn take(&mut self, byte: u8) -> Option<bool> {
        match (self.at, byte) {
            (Pair::Key, b'=') => {
                // A key is one character. Any other is ignored, and so is every pair after it
                if self.length == 1 {
                    self.key = self.piece[0];
                    self.at = Pair::Value;
                } else {
                    self.at = Pair::ValueIgnored;
                }
                self.length = 0;
            }
            (Pair::Key | Pair::ValueIgnored, b';') => return Some(self.file),
            (Pair::Key, _) => self.keep(byte, Pair::KeyIgnored),
            (Pair::KeyIgnored, b'=') => self.at = Pair::ValueIgnored,
            (Pair::KeyIgnored, _) => {}
            (Pair::Value, b',' | b';') => {
                // The last `t` is the one that counts
                if self.key == b't' {
                    self.file = names_a_file(&self.piece[..self.length]);
                }
                self.length = 0;
                self.at = Pair::Key;
                if byte == b';' {
                    return Some(self.file);
                }
            }
            (Pair::Value, _) => self.keep(byte, Pair::ValueIgnored),
            (Pair::ValueIgnored, b',') => self.at = Pair::KeyIgnored,
            (Pair::ValueIgnored, _) => {}
        }
        None
    }

    /// Adds a byte to the piece in hand, or — one past what a piece may be — goes on as `past`.
    fn keep(&mut self, byte: u8, past: Pair) {
        if self.length == PIECE_BYTES {
            self.at = past;
            self.length = 0;
        } else {
            self.piece[self.length] = byte;
            self.length += 1;
        }
    }
}

/// Whether a value of the `t` key is the one that names a file.
///
/// **`f`, or its number.** The core reads a value that is not one character other than a digit as a
/// number, and takes the medium from that number as a character — so `t=102` is `t=f`, and so is every
/// way its number parser reads 102: a `+` before it, zeros before it, `_` between its digits. A spelling
/// that parser refuses is taken here as well; the core has given up on such a command, and nothing is
/// opened for it.
fn names_a_file(value: &[u8]) -> bool {
    if let [character] = value
        && !character.is_ascii_digit()
    {
        return *character == b'f';
    }
    let mut number = 0u64;
    for digit in value.strip_prefix(b"+").unwrap_or(value) {
        match digit {
            b'_' => {}
            // Eleven digits at most (`PIECE_BYTES`), which a `u64` holds
            b'0'..=b'9' => number = number * 10 + u64::from(digit - b'0'),
            _ => return false,
        }
    }
    number == u64::from(b'f')
}

/// The bytes base64 `text` stands for, or `None` where a character of it is not base64's. Padding may be
/// there or not.
///
/// **Stricter than the core, which is safe because of `encoded`.** The core's decoder passes over
/// characters this refuses. Were its payload fed as it came, a name could be spelled so that the path
/// asked about here is not the path opened there; the core is instead fed what was asked about, spelled
/// the one way every decoder reads alike.
fn decoded(text: &[u8]) -> Option<Vec<u8>> {
    let text = text.strip_suffix(b"=").unwrap_or(text);
    let text = text.strip_suffix(b"=").unwrap_or(text);
    let mut bytes = Vec::new();
    // The bits read and not yet written, and how many of them there are
    let (mut bits, mut held) = (0u32, 0u32);
    for character in text {
        let value = ALPHABET.iter().position(|known| known == character)?;
        bits = (bits << 6) + value as u32;
        held += 6;
        if held >= 8 {
            held -= 8;
            // The cast keeps the eight bits above the ones still held, and drops those written before
            bytes.push((bits >> held) as u8);
        }
    }
    Some(bytes)
}

/// `bytes` as base64, padded.
fn encoded(bytes: &[u8]) -> Vec<u8> {
    let mut text = Vec::new();
    for chunk in bytes.chunks(3) {
        let mut group = [0u8; 4];
        group[1..=chunk.len()].copy_from_slice(chunk);
        let bits = u32::from_be_bytes(group);
        // A character for each byte and one more, and the rest of the four is padding
        for index in 0..4 {
            text.push(if index <= chunk.len() { ALPHABET[(bits >> (18 - 6 * index) & 63) as usize] } else { b'=' });
        }
    }
    text
}

/// Whether `path` is a regular file, as the system answers without opening it. A link is followed, as
/// opening it would; a path that cannot be asked about is not one.
#[cfg(unix)]
pub fn regular_file(path: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::fs::metadata(std::ffi::OsStr::from_bytes(path)).is_ok_and(|found| found.is_file())
}

/// Never asked: no session's names are read here where this is compiled (`Screen::allow_images`).
#[cfg(not(unix))]
pub fn regular_file(_path: &[u8]) -> bool {
    false
}

/// Where the next byte of a session's output falls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Outside a string, or in one let go of: nothing here reads it
    Away,
    /// In a string, before its first byte of content: `G` makes it a graphics command
    Begun,
    /// In a graphics command's control data
    Control(Control),
    /// In the payload of a command whose source is a file: its name, which is held
    Named,
    /// In a name longer than any path, which is dropped as it comes
    TooLong,
}

/// Reads the body of each string the core says has begun, and keeps a file's name from the core until
/// it has been asked about.
///
/// **Its state is the core's, and stays so only while every byte the core is fed comes through it**:
/// the body of a string through `read`, the rest by the caller, who says with `begin` when the core
/// reports a string. That holds for a replay too, which is why a replay is read here like anything
/// else rather than fed around this; it is the core that is set to take no file then.
#[derive(Debug)]
pub struct ImagePaths {
    state: State,
    /// The name so far, as base64
    name: Vec<u8>,
}

impl Default for ImagePaths {
    fn default() -> Self {
        Self { state: State::Away, name: Vec::new() }
    }
}

impl ImagePaths {
    /// The core has begun an APC, SOS or PM string: its body is `read`'s from the next byte.
    pub fn begin(&mut self) {
        self.state = State::Begun;
    }

    /// Whether the next byte is `read`'s.
    pub fn reading(&self) -> bool {
        self.state != State::Away
    }

    /// Reads a string's body from the front of `bytes`, feeding `core` what the core is to be fed in
    /// its place. Answers how many bytes were read, which stops short of the byte that ends the string:
    /// that one is the caller's to feed, as any byte outside a string is.
    ///
    /// **Nothing is held but a name.** A string that is not a graphics command is let go of at its first
    /// byte, and a command at the `;` after its control data unless its source is a file; up to there
    /// every byte is fed as it came, and from there the rest is not read here at all. A name is fed
    /// once, when the string ends, and only if `may_open` says so of the path it decodes to.
    pub fn read(&mut self, bytes: &[u8], may_open: impl Fn(&[u8]) -> bool, mut core: impl FnMut(&[u8])) -> usize {
        let mut read = 0;
        while let (State::Begun | State::Control(_), Some(byte)) = (self.state, bytes.get(read)) {
            match (kind(*byte), self.state) {
                (Byte::Ends, _) => {
                    self.state = State::Away;
                    break;
                }
                (Byte::Content, State::Begun) if *byte == b'G' => self.state = State::Control(Control::new()),
                (Byte::Content, State::Control(mut control)) => {
                    self.state = match control.take(*byte) {
                        None => State::Control(control),
                        Some(true) => State::Named,
                        Some(false) => State::Away,
                    };
                }
                (Byte::Content, _) => self.state = State::Away,
                (Byte::PassedOver, _) => {}
            }
            read += 1;
        }
        core(&bytes[..read]);
        while let (State::Named | State::TooLong, Some(byte)) = (self.state, bytes.get(read)) {
            match (kind(*byte), self.state) {
                (Byte::Ends, state) => {
                    // Whichever way it ended: a command that was dropped is fed a name it will not read
                    let name = std::mem::take(&mut self.name);
                    if state == State::Named
                        && let Some(path) = decoded(&name).filter(|path| may_open(path))
                    {
                        core(&encoded(&path));
                    }
                    self.state = State::Away;
                    break;
                }
                (Byte::Content, State::Named) if self.name.len() < NAME_BYTES => self.name.push(*byte),
                (Byte::Content, _) => {
                    self.name = Vec::new();
                    self.state = State::TooLong;
                }
                (Byte::PassedOver, _) => {}
            }
            read += 1;
        }
        read
    }
}
