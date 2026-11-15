//! Reading a graphics command on its way to the core, and finding the ones that name their source.
//!
//! **Where a string begins is the core's to say, and what its body means is repeated here.** The first is
//! `Terminal::feed_to_string`: a pattern looked for in the bytes would be a second opinion about `ESC`,
//! and would be wrong about the five other ways the core begins such a string. The second cannot be
//! asked, because the core runs a command on the byte that ends it and a source has to be read before
//! then. So three of the core's rules are written out again, each as short as the rule: which bytes of a
//! string's body are content, are passed over, or end it (`kind`); how a graphics command's control data
//! is read into keys and values (`Control`, `Keys`); and how a value is read as a number (`number`).
//! `tests/image_sources.rs` holds all three to the core itself, over streams it makes up.
//!
//! **Nothing of a command is held but what the core keeps of it.** The core keeps a command's control data
//! as its keys and their values and nothing else, so a command that names no source is fed to the core
//! as those keys, in its own spelling, as soon as its control data ends; one the core would drop is let
//! go of whole, the core given none of it; and of one that names its source the name is held, decoded,
//! as the core's decoder would decode it (`Base64`). So what is held is a few hundred bytes and a path,
//! whatever a program writes.
//!
//! **And the core opens nothing**: a session's core takes no file and no shared memory object
//! (`Screen::allow_images`), so were this ever to pass a command naming one, the core would refuse it.

use super::{Load, Source};

/// The longest path read. No system this runs on opens a longer one, so a name past this is not read.
pub const PATH_BYTES: usize = 4096;

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
    /// The string is over, and its command is run
    Runs,
    /// The string is over, and its command is dropped
    Drops,
}

fn kind(byte: u8) -> Byte {
    match byte {
        // ESC, which begins the ST that ends a string, and the 8-bit ST
        0x1b | 0x9c => Byte::Runs,
        // CAN, SUB, and the 8-bit controls that leave for ground or for a sequence of another kind
        0x18 | 0x1a | 0x80..=0x97 | 0x99..=0x9b | 0x9d => Byte::Drops,
        // The 8-bit spellings of SOS, PM and APC, which begin nothing inside one, and every byte above them
        0x98 | 0x9e..=0xff => Byte::PassedOver,
        _ => Byte::Content,
    }
}

/// The keys of a command's control data and their values, as the core keeps them
/// (`kitty/graphics_command.zig`, `KV`): a key is a letter, and its value a character's code or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    values: [u32; 52],
    present: u64,
}

impl Keys {
    const fn new() -> Self {
        Self { values: [0; 52], present: 0 }
    }

    fn index(key: u8) -> Option<usize> {
        match key {
            b'a'..=b'z' => Some(usize::from(key - b'a')),
            b'A'..=b'Z' => Some(26 + usize::from(key - b'A')),
            _ => None,
        }
    }

    /// The value of `key`, if the command gave it one.
    pub fn get(&self, key: u8) -> Option<u32> {
        let index = Self::index(key)?;
        (self.present >> index & 1 == 1).then_some(self.values[index])
    }

    /// Gives `key` its value. A key that is not a letter is not one the core keeps, and nor is it here.
    fn put(&mut self, key: u8, value: u32) {
        if let Some(index) = Self::index(key) {
            self.values[index] = value;
            self.present |= 1 << index;
        }
    }

    /// Each key and its value in the core's own spelling — every value as its number, which is how the
    /// core reads a value of more than one character — leaving out the keys in `without`.
    pub fn spelled(&self, without: &[u8]) -> Vec<u8> {
        let mut spelled = Vec::new();
        for key in (b'a'..=b'z').chain(b'A'..=b'Z') {
            let Some(value) = self.get(key).filter(|_| !without.contains(&key)) else { continue };
            if !spelled.is_empty() {
                spelled.push(b',');
            }
            spelled.extend_from_slice(&[key, b'=']);
            // The three keys the core reads as signed numbers, kept as their bits
            let number = if SIGNED.contains(&key) { (value as i32).to_string() } else { value.to_string() };
            spelled.extend_from_slice(number.as_bytes());
        }
        spelled
    }
}

/// The keys whose values the core reads as signed numbers: a placement's `z` order and its `H` and `V`
/// offsets.
const SIGNED: [u8; 3] = *b"zHV";

/// `text` read as the core reads a number (`std.fmt.parseInt` in base ten), or `None` where it refuses
/// one: a sign, then digits that `_` may separate but not begin or end, no larger than the type holds.
fn number(text: &[u8], signed: bool) -> Option<u32> {
    let (negative, digits) = match text {
        [b'+', rest @ ..] => (false, rest),
        [b'-', rest @ ..] => (true, rest),
        _ => (false, text),
    };
    if digits.first().is_none_or(|first| *first == b'_') || digits.last() == Some(&b'_') {
        return None;
    }
    // Eleven characters at most (`PIECE_BYTES`), which an `i64` holds
    let mut value: i64 = 0;
    for digit in digits {
        match digit {
            b'_' => {}
            b'0'..=b'9' => value = value * 10 + i64::from(digit - b'0'),
            _ => return None,
        }
    }
    let value = if negative { -value } else { value };
    if signed { i32::try_from(value).ok().map(|value| value as u32) } else { u32::try_from(value).ok() }
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
    keys: Keys,
    /// A value the core does not read, on which it drops the command
    refused: bool,
}

impl Control {
    const fn new() -> Self {
        Self { at: Pair::Key, piece: [0; PIECE_BYTES], length: 0, key: 0, keys: Keys::new(), refused: false }
    }

    /// Reads one byte of content. Answers whether it was the `;` that ends the control data.
    fn take(&mut self, byte: u8) -> bool {
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
            (Pair::Key | Pair::ValueIgnored, b';') => return true,
            (Pair::Key, _) => self.keep(byte, Pair::KeyIgnored),
            (Pair::KeyIgnored, b'=') => self.at = Pair::ValueIgnored,
            (Pair::KeyIgnored, _) => {}
            (Pair::Value, b',' | b';') => {
                self.finish();
                self.at = Pair::Key;
                return byte == b';';
            }
            (Pair::Value, _) => self.keep(byte, Pair::ValueIgnored),
            (Pair::ValueIgnored, b',') => self.at = Pair::KeyIgnored,
            (Pair::ValueIgnored, _) => {}
        }
        false
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

    /// Gives the key in hand the value in hand: one character that is not a digit is that character, and
    /// anything else is a number (`finishValue`).
    fn finish(&mut self) {
        let value = match &self.piece[..self.length] {
            [character] if !character.is_ascii_digit() => Some(u32::from(*character)),
            digits => number(digits, SIGNED.contains(&self.key)),
        };
        match value {
            Some(value) => self.keys.put(self.key, value),
            None => self.refused = true,
        }
        self.length = 0;
    }

    /// The string has ended in the control data: whether the core completes the command, rather than
    /// dropping it (`Parser.complete`). A value in hand is finished as a `,` would finish it, and a key in
    /// hand is one the core refuses.
    fn end(&mut self) -> bool {
        match self.at {
            Pair::Key | Pair::KeyIgnored => false,
            Pair::Value => {
                self.finish();
                !self.refused
            }
            Pair::ValueIgnored => true,
        }
    }

    /// What the command asks to be read, at the `;` that ends its control data: `None` for one that
    /// names no source, or that the core would not run.
    fn names(&self) -> Option<Named> {
        if self.refused {
            return None;
        }
        // The actions that carry a transmission (`Command.transmission`), `t` where none is given
        let action = u8::try_from(self.keys.get(b'a').unwrap_or(u32::from(b't'))).ok()?;
        if !matches!(action, b't' | b'T' | b'q' | b'f') {
            return None;
        }
        let shared = match u8::try_from(self.keys.get(b't')?).ok()? {
            b'f' => false,
            b's' => true,
            _ => return None,
        };
        // The one other key whose value the core refuses a transmission for (`Transmission.parse`)
        if self.keys.get(b'o').is_some_and(|compression| compression != u32::from(b'z')) {
            return None;
        }
        Some(Named { keys: self.keys, action, shared })
    }
}

/// A command that names its source, as far as its control data says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Named {
    keys: Keys,
    /// `t`, `T`, `q` or `f`
    action: u8,
    /// The source is a shared memory object rather than a file
    shared: bool,
}

impl Named {
    /// What is to be read for the command, its name having been decoded as `name`.
    fn load(self, name: Option<Vec<u8>>) -> Load {
        let source = match name {
            Some(name) if self.shared => Source::SharedMemory(name),
            Some(path) => Source::File(path),
            None => Source::Unreadable,
        };
        Load::new(self.keys, self.action, source)
    }
}

/// Whether a byte is one the core's decoder passes over in base64: ASCII white space, but for the
/// vertical tab.
fn white(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// A name in base64, decoded a byte at a time as the core's decoder decodes it (simdutf's, in its loose
/// mode). White space anywhere is passed over; `=` may only end it, and no more than twice; padded, it is
/// whole groups of four, and unpadded it does not end on one character of a group; the bits a last
/// character holds past the last whole byte are dropped. Anything else — a character outside the
/// alphabet, `-` and `_` among them — and the core drops the command whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Base64 {
    /// What has been decoded, up to `PATH_BYTES` of it
    bytes: Vec<u8>,
    /// More was decoded than any path is
    long: bool,
    /// The bits read and not yet written, and how many of them there are
    bits: u32,
    held: u32,
    /// The characters read, padding among them and white space not
    length: usize,
    padding: usize,
    /// Something the core's decoder refuses
    refused: bool,
}

/// What a name decodes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// The name as bytes
    Name(Vec<u8>),
    /// A name longer than any path, which nothing could open
    TooLong,
    /// Not base64 as the core's decoder reads it: the core drops the command
    Refused,
}

impl Base64 {
    /// Reads one more character of a name.
    pub fn push(&mut self, character: u8) {
        if white(character) {
            return;
        }
        self.length += 1;
        if character == b'=' {
            self.padding += 1;
            return;
        }
        match ALPHABET.iter().position(|known| *known == character) {
            Some(value) if self.padding == 0 => {
                bits_in(self, value as u32);
            }
            _ => self.refused = true,
        }
    }

    /// What the characters read decode to.
    pub fn finish(self) -> Decoded {
        let whole = match self.padding {
            0 => self.length % 4 != 1,
            1 | 2 => self.length.is_multiple_of(4),
            _ => false,
        };
        match (self.refused || !whole, self.long) {
            (true, _) => Decoded::Refused,
            (false, true) => Decoded::TooLong,
            (false, false) => Decoded::Name(self.bytes),
        }
    }
}

/// Six more bits of a name, and the byte they complete if they complete one.
fn bits_in(name: &mut Base64, value: u32) {
    name.bits = (name.bits << 6) + value;
    name.held += 6;
    if name.held >= 8 {
        name.held -= 8;
        // The cast keeps the eight bits above the ones still held, and drops those written before
        let byte = (name.bits >> name.held) as u8;
        if name.bytes.len() < PATH_BYTES {
            name.bytes.push(byte);
        } else {
            name.long = true;
        }
    }
}

/// `bytes` as base64, padded.
pub fn encoded(bytes: &[u8]) -> Vec<u8> {
    let mut text = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
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

/// Where the next byte of a session's output falls.
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Outside a string, or in one let go of: nothing here reads it
    Away,
    /// In a string, before its first byte of content: `G` makes it a graphics command
    Begun,
    /// In a graphics command's control data
    Control(Control),
    /// In a command the core drops whole, all of which is passed over: the core is given none of it
    Dropped,
    /// In the payload of a command that names its source: its name
    Naming(Named, Base64),
}

/// Reads the body of each string the core says has begun, and takes out of it each command that names
/// its source, for the source to be read before the core is told of it.
///
/// **Its state is the core's, and stays so only while every byte the core is fed comes through it**:
/// the body of a string through `read`, the rest by the caller, who says with `begin` when the core
/// reports a string.
#[derive(Debug)]
pub struct Scanner {
    state: State,
}

impl Default for Scanner {
    fn default() -> Self {
        Self { state: State::Away }
    }
}

impl Scanner {
    /// The core has begun an APC, SOS or PM string: its body is `read`'s from the next byte.
    pub fn begin(&mut self) {
        self.state = State::Begun;
    }

    /// Whether the next byte is `read`'s.
    pub fn reading(&self) -> bool {
        self.state != State::Away
    }

    /// Reads a string's body from the front of `bytes`, feeding `core` what the core is to be fed of it.
    /// Answers how many bytes were read, which stops short of the byte that ends the string — that one is
    /// the caller's to feed, as any byte outside a string is — and, where that byte runs a command that
    /// named its source, what is to be read for it.
    ///
    /// **A command is decided at the end of its control data**: one that names no source is fed as the
    /// core read it (`Keys::spelled`) and let go of, its payload the core's as it comes; one the core
    /// would drop is passed over to its end; and one that names a source is answered by the `Load`, which
    /// says what to feed in its place once its source has been read. A string that is not a graphics
    /// command is let go of at its first byte of content.
    pub fn read(&mut self, bytes: &[u8], mut core: impl FnMut(&[u8])) -> (usize, Option<Load>) {
        let mut read = 0;
        while let Some(&byte) = bytes.get(read) {
            match (kind(byte), &mut self.state) {
                (_, State::Away) => break,
                // The core is given nothing of a string that ends here, and runs or drops nothing
                (Byte::Drops, _) | (Byte::Runs, State::Begun | State::Dropped) => {
                    self.state = State::Away;
                    break;
                }
                (Byte::Runs, State::Control(control)) => {
                    // A command that ends with its control data: the core finishes it as it would
                    let named = control.end().then(|| (control.names(), control.keys));
                    self.state = State::Away;
                    match named {
                        Some((Some(named), _)) => return (read, Some(named.load(Some(Vec::new())))),
                        // With no payload, which the core reads as one that is empty
                        Some((None, keys)) => core(&[b"G".as_slice(), &keys.spelled(b""), b";"].concat()),
                        None => {}
                    }
                    break;
                }
                (Byte::Runs, State::Naming(..)) => {
                    let State::Naming(named, name) = std::mem::replace(&mut self.state, State::Away) else {
                        unreachable!("matched as naming")
                    };
                    match name.finish() {
                        Decoded::Name(name) => return (read, Some(named.load(Some(name)))),
                        Decoded::TooLong => return (read, Some(named.load(None))),
                        // Dropped by the core, which has been given none of it
                        Decoded::Refused => break,
                    }
                }
                (Byte::PassedOver, _) | (Byte::Content, State::Dropped) => {}
                (Byte::Content, State::Begun) if byte == b'G' => self.state = State::Control(Control::new()),
                // Not a graphics command: the core's, from this byte on
                (Byte::Content, State::Begun) => {
                    self.state = State::Away;
                    break;
                }
                (Byte::Content, State::Control(control)) => {
                    let ended = control.take(byte);
                    if control.refused {
                        self.state = State::Dropped;
                    } else if ended {
                        let (named, keys) = (control.names(), control.keys);
                        self.state = match named {
                            Some(named) => State::Naming(named, Base64::default()),
                            None => {
                                core(&[b"G".as_slice(), &keys.spelled(b""), b";"].concat());
                                read += 1;
                                self.state = State::Away;
                                break;
                            }
                        };
                    }
                }
                (Byte::Content, State::Naming(_, name)) => name.push(byte),
            }
            read += 1;
        }
        (read, None)
    }
}
