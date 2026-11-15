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
//! **A command this does not understand is the core's, and the core opens nothing.** Every way this can
//! give up — control data longer than it holds, a value it does not read as the core would — hands the
//! command over as it came, and a session's core takes no file and no shared memory object
//! (`Screen::allow_images`). So a disagreement with the core is an image not shown, never a path opened.

use super::{Load, Source};

/// The most control data held while it is read. A command with more is handed over as it came.
pub const CONTROL_BYTES: usize = 4096;

/// The longest path read. No system this runs on opens a longer one, so a name past this is refused
/// without being looked at.
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
    /// What is to be read for the command, the name it gave being `name` as base64.
    fn load(self, name: Option<&[u8]>) -> Load {
        let source = match name.and_then(decoded) {
            Some(name) if self.shared => Source::SharedMemory(name),
            Some(path) => Source::File(path),
            None => Source::Unreadable,
        };
        Load::new(self.keys, self.action, source)
    }
}

/// The bytes base64 `text` stands for, or `None` where a character of it is not base64's. Padding may be
/// there or not.
///
/// **Stricter than the core**, whose decoder passes over characters this refuses: a name is the program's
/// to spell, and one spelled so that two decoders read two paths is not read at all.
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
    /// In the payload of a command that names its source: the name, which is held
    Naming(Named),
    /// In a name longer than any path, which is dropped as it comes
    TooLong(Named),
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
    /// What of the body has been read and not yet fed: control data, or a name as base64
    held: Vec<u8>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self { state: State::Away, held: Vec::new() }
    }
}

impl Scanner {
    /// The core has begun an APC, SOS or PM string: its body is `read`'s from the next byte.
    pub fn begin(&mut self) {
        self.state = State::Begun;
        self.held.clear();
    }

    /// Whether the next byte is `read`'s.
    pub fn reading(&self) -> bool {
        self.state != State::Away
    }

    /// Reads a string's body from the front of `bytes`, feeding `core` what of it the core is to be fed
    /// as it came. Answers how many bytes were read, which stops short of the byte that ends the string —
    /// that one is the caller's to feed, as any byte outside a string is — and, where that byte runs a
    /// command that named its source, what is to be read for it.
    ///
    /// **Held until it is decided, and only then.** A string that is not a graphics command is let go of
    /// at its first byte. A command's control data is held up to its `;`, and fed as it came unless it
    /// names a source; from there the rest is not read here at all. A name is held to the end of its
    /// string and never fed: the command it was in is answered by the `Load` instead, which says what to
    /// feed in its place once its source has been read.
    pub fn read(&mut self, bytes: &[u8], mut core: impl FnMut(&[u8])) -> (usize, Option<Load>) {
        let mut read = 0;
        while let Some(&byte) = bytes.get(read) {
            match (kind(byte), &mut self.state) {
                (_, State::Away) => break,
                (Byte::Runs | Byte::Drops, State::Begun | State::Control(_)) => {
                    // The core runs or drops what it was given as it would have: it was given all of it
                    core(&self.held);
                    self.let_go();
                    break;
                }
                (Byte::Runs, State::Naming(named) | State::TooLong(named)) => {
                    let named = *named;
                    let name = matches!(self.state, State::Naming(_)).then_some(self.held.as_slice());
                    let load = named.load(name);
                    self.let_go();
                    return (read, Some(load));
                }
                // Dropped: the core has been given none of the command, and the byte ending it drops nothing
                (Byte::Drops, State::Naming(_) | State::TooLong(_)) => {
                    self.let_go();
                    break;
                }
                (Byte::PassedOver, State::Begun | State::Control(_)) => self.held.push(byte),
                (Byte::PassedOver, _) => {}
                (Byte::Content, State::Begun) if byte == b'G' => {
                    self.held.push(byte);
                    self.state = State::Control(Control::new());
                }
                (Byte::Content, State::Begun) => {
                    core(&self.held);
                    self.let_go();
                    break;
                }
                (Byte::Content, State::Control(control)) => {
                    let ended = control.take(byte);
                    let named = if ended { control.names() } else { None };
                    self.held.push(byte);
                    match named {
                        Some(named) => {
                            self.held.clear();
                            self.state = State::Naming(named);
                        }
                        // Not one this reads, or longer than it holds: the core's, as it came
                        None if ended || self.held.len() > CONTROL_BYTES => {
                            core(&self.held);
                            self.let_go();
                            read += 1;
                            break;
                        }
                        None => {}
                    }
                }
                (Byte::Content, State::Naming(named)) => {
                    if self.held.len() < NAME_BYTES {
                        self.held.push(byte);
                    } else {
                        self.state = State::TooLong(*named);
                        self.held = Vec::new();
                    }
                }
                (Byte::Content, State::TooLong(_)) => {}
            }
            read += 1;
        }
        (read, None)
    }

    fn let_go(&mut self) {
        self.state = State::Away;
        self.held = Vec::new();
    }
}
