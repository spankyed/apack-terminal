//! Streaming Kitty graphics APC scanner for intercepting named file / shared-memory sources
//! (`t=f` / `t=s`) while keeping bounded O(1) state.

use super::{Load, Source};

/// Maximum decoded path length accepted (4 KiB).
pub const PATH_BYTES: usize = 4096;

/// Maximum key/value token length retained by `libghostty-vt`'s Kitty graphics parser.
const PIECE_BYTES: usize = 11;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Byte {
    Content,
    PassedOver,
    Runs,
    Drops,
}

fn kind(byte: u8) -> Byte {
    match byte {
        0x1b | 0x9c => Byte::Runs,
        0x18 | 0x1a | 0x80..=0x97 | 0x99..=0x9b | 0x9d => Byte::Drops,
        0x98 | 0x9e..=0xff => Byte::PassedOver,
        _ => Byte::Content,
    }
}

/// Compact 52-slot map of Kitty graphics single-letter keys (`a`..=`z`, `A`..=`Z`) to `u32` values.
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

    /// Returns the parsed value for `key`, if present.
    pub fn get(&self, key: u8) -> Option<u32> {
        let index = Self::index(key)?;
        ((self.present >> index) & 1 == 1).then_some(self.values[index])
    }

    fn put(&mut self, key: u8, value: u32) {
        if let Some(index) = Self::index(key) {
            self.values[index] = value;
            self.present |= 1 << index;
        }
    }

    /// Re-serializes present keys in canonical numeric form, omitting any key listed in `without`.
    pub fn spelled(&self, without: &[u8]) -> Vec<u8> {
        let mut spelled = Vec::new();
        for key in (b'a'..=b'z').chain(b'A'..=b'Z') {
            let Some(value) = self.get(key).filter(|_| !without.contains(&key)) else {
                continue;
            };
            if !spelled.is_empty() {
                spelled.push(b',');
            }
            spelled.extend_from_slice(&[key, b'=']);
            let number = if SIGNED.contains(&key) { (value as i32).to_string() } else { value.to_string() };
            spelled.extend_from_slice(number.as_bytes());
        }
        spelled
    }
}

const SIGNED: [u8; 3] = *b"zHV";

fn number(text: &[u8], signed: bool) -> Option<u32> {
    let (negative, digits) = match text {
        [b'+', rest @ ..] => (false, rest),
        [b'-', rest @ ..] => (true, rest),
        _ => (false, text),
    };
    if digits.first().is_none_or(|first| *first == b'_') || digits.last() == Some(&b'_') {
        return None;
    }
    let mut value: i64 = 0;
    for digit in digits {
        match digit {
            b'_' => {}
            b'0'..=b'9' => value = value * 10 + i64::from(digit - b'0'),
            _ => return None,
        }
    }
    let value = if negative { -value } else { value };
    if signed { i32::try_from(value).ok().map(|v| v as u32) } else { u32::try_from(value).ok() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pair {
    Key,
    KeyIgnored,
    Value,
    ValueIgnored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Control {
    at: Pair,
    piece: [u8; PIECE_BYTES],
    length: usize,
    key: u8,
    keys: Keys,
    refused: bool,
}

impl Control {
    const fn new() -> Self {
        Self { at: Pair::Key, piece: [0; PIECE_BYTES], length: 0, key: 0, keys: Keys::new(), refused: false }
    }

    fn take(&mut self, byte: u8) -> bool {
        match (self.at, byte) {
            (Pair::Key, b'=') => {
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

    fn keep(&mut self, byte: u8, past: Pair) {
        if self.length == PIECE_BYTES {
            self.at = past;
            self.length = 0;
        } else {
            self.piece[self.length] = byte;
            self.length += 1;
        }
    }

    fn finish(&mut self) {
        let value = match &self.piece[..self.length] {
            [ch] if !ch.is_ascii_digit() => Some(u32::from(*ch)),
            digits => number(digits, SIGNED.contains(&self.key)),
        };
        match value {
            Some(v) => self.keys.put(self.key, v),
            None => self.refused = true,
        }
        self.length = 0;
    }

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

    fn names(&self) -> Option<Named> {
        if self.refused {
            return None;
        }
        let action = u8::try_from(self.keys.get(b'a').unwrap_or(u32::from(b't'))).ok()?;
        if !matches!(action, b't' | b'T' | b'q' | b'f') {
            return None;
        }
        let shared = match u8::try_from(self.keys.get(b't')?).ok()? {
            b'f' => false,
            b's' => true,
            _ => return None,
        };
        if self.keys.get(b'o').is_some_and(|compression| compression != u32::from(b'z')) {
            return None;
        }
        Some(Named { keys: self.keys, action, shared })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Named {
    keys: Keys,
    action: u8,
    shared: bool,
}

impl Named {
    fn load(self, name: Option<Vec<u8>>) -> Load {
        let source = match name {
            Some(name) if self.shared => match super::shared_memory_name(&name) {
                true => Source::SharedMemory(name),
                false => Source::Unreadable,
            },
            Some(path) => Source::File(path),
            None => Source::Unreadable,
        };
        Load::new(self.keys, self.action, source)
    }
}

fn white(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// Streaming loose base64 decoder bounded at [`PATH_BYTES`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Base64 {
    bytes: Vec<u8>,
    long: bool,
    bits: u32,
    held: u32,
    length: usize,
    padding: usize,
    refused: bool,
}

/// Decoded result of [`Base64::finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// Successfully decoded name bytes.
    Name(Vec<u8>),
    /// Valid base64 exceeding [`PATH_BYTES`].
    TooLong,
    /// Malformed base64 input.
    Refused,
}

impl Base64 {
    /// Feeds one character into the streaming base64 decoder.
    pub fn push(&mut self, ch: u8) {
        if white(ch) {
            return;
        }
        self.length += 1;
        if ch == b'=' {
            self.padding += 1;
            return;
        }
        match ALPHABET.iter().position(|known| *known == ch) {
            Some(value) if self.padding == 0 => {
                bits_in(self, value as u32);
            }
            _ => self.refused = true,
        }
    }

    /// Finalizes decoding and returns the bounded [`Decoded`] outcome.
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

fn bits_in(name: &mut Base64, value: u32) {
    name.bits = (name.bits << 6) + value;
    name.held += 6;
    if name.held >= 8 {
        name.held -= 8;
        let byte = (name.bits >> name.held) as u8;
        if name.bytes.len() < PATH_BYTES {
            name.bytes.push(byte);
        } else {
            name.long = true;
        }
    }
}

/// Encodes `bytes` as padded standard base64.
pub fn encoded(bytes: &[u8]) -> Vec<u8> {
    let mut text = vec![b'='; bytes.len().div_ceil(3) * 4];
    let (groups, rest) = bytes.as_chunks::<3>();
    for (group, chars) in groups.iter().zip(text.as_chunks_mut::<4>().0) {
        *chars = characters_of([0, group[0], group[1], group[2]]);
    }
    if !rest.is_empty() {
        let mut group = [0u8; 4];
        group[1..=rest.len()].copy_from_slice(rest);
        let at = groups.len() * 4;
        text[at..=at + rest.len()].copy_from_slice(&characters_of(group)[..=rest.len()]);
    }
    text
}

#[inline(always)]
fn characters_of(group: [u8; 4]) -> [u8; 4] {
    let bits = u32::from_be_bytes(group) as usize;
    [ALPHABET[(bits >> 18) & 63], ALPHABET[(bits >> 12) & 63], ALPHABET[(bits >> 6) & 63], ALPHABET[bits & 63]]
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Away,
    Begun,
    Control(Control),
    Dropped,
    Naming(Named, Base64),
}

/// Streaming scanner for APC strings carrying Kitty graphics commands.
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
    /// Signals that the VT core entered an APC/SOS/PM string.
    pub fn begin(&mut self) {
        self.state = State::Begun;
    }

    /// Returns `true` while the scanner is actively consuming a string body.
    pub fn reading(&self) -> bool {
        self.state != State::Away
    }

    /// Consumes bytes from the front of `bytes`, forwarding non-named Kitty graphics control data
    /// to `core` and returning `(bytes_consumed, Option<Load>)`.
    pub fn read(&mut self, bytes: &[u8], mut core: impl FnMut(&[u8])) -> (usize, Option<Load>) {
        for (idx, &byte) in bytes.iter().enumerate() {
            match (kind(byte), &mut self.state) {
                (_, State::Away) => return (idx, None),
                (Byte::Drops, _) | (Byte::Runs, State::Begun | State::Dropped) => {
                    self.state = State::Away;
                    return (idx, None);
                }
                (Byte::Runs, State::Control(control)) => {
                    let named = control.end().then(|| (control.names(), control.keys));
                    self.state = State::Away;
                    match named {
                        Some((Some(named), _)) => return (idx, Some(named.load(Some(Vec::new())))),
                        Some((None, keys)) => core(&[b"G".as_slice(), &keys.spelled(b""), b";"].concat()),
                        None => {}
                    }
                    return (idx, None);
                }
                (Byte::Runs, State::Naming(..)) => {
                    let State::Naming(named, name) = std::mem::replace(&mut self.state, State::Away) else {
                        unreachable!("matched as naming")
                    };
                    match name.finish() {
                        Decoded::Name(name) => return (idx, Some(named.load(Some(name)))),
                        Decoded::TooLong => return (idx, Some(named.load(None))),
                        Decoded::Refused => return (idx, None),
                    }
                }
                (Byte::PassedOver, _) | (Byte::Content, State::Dropped) => {}
                (Byte::Content, State::Begun) if byte == b'G' => {
                    self.state = State::Control(Control::new());
                }
                (Byte::Content, State::Begun) => {
                    self.state = State::Away;
                    return (idx, None);
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
                                self.state = State::Away;
                                return (idx + 1, None);
                            }
                        };
                    }
                }
                (Byte::Content, State::Naming(_, name)) => name.push(byte),
            }
        }
        (bytes.len(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encode_decode_roundtrip_whitespace_and_errors() {
        for input in [b"".as_slice(), b"a", b"ab", b"abc", b"abcd", b"/tmp/hello world.png"] {
            let enc = encoded(input);
            let mut dec = Base64::default();
            for &b in &enc {
                dec.push(b' ');
                dec.push(b);
                dec.push(b'\n');
            }
            assert_eq!(dec.finish(), Decoded::Name(input.to_vec()));
        }

        // Unpadded valid lengths (2 and 3) succeed; length 1 fails.
        let mut unpadded2 = Base64::default();
        for &b in b"YQ" {
            unpadded2.push(b);
        }
        assert_eq!(unpadded2.finish(), Decoded::Name(b"a".to_vec()));

        let mut bad_len1 = Base64::default();
        bad_len1.push(b'A');
        assert_eq!(bad_len1.finish(), Decoded::Refused);

        // Character after padding fails.
        let mut after_pad = Base64::default();
        for &b in b"YQ=A" {
            after_pad.push(b);
        }
        assert_eq!(after_pad.finish(), Decoded::Refused);

        // Triple padding fails.
        let mut tri_pad = Base64::default();
        for &b in b"A===" {
            tri_pad.push(b);
        }
        assert_eq!(tri_pad.finish(), Decoded::Refused);

        // Overlong payload returns TooLong.
        let huge = encoded(&vec![b'x'; PATH_BYTES + 1]);
        let mut dec_huge = Base64::default();
        for &b in &huge {
            dec_huge.push(b);
        }
        assert_eq!(dec_huge.finish(), Decoded::TooLong);
    }

    #[test]
    fn scanner_handles_passthrough_signed_keys_drops_and_edge_cases() {
        let mut scanner = Scanner::default();

        // Non-G APC string hands straight back to core.
        scanner.begin();
        let (n, load) = scanner.read(b"Xhello\x1b\\", |_| {});
        assert_eq!(n, 0);
        assert!(load.is_none());
        assert!(!scanner.reading());

        // Inline t=d passes control data through to core with signed z=-5 and numeric underscore.
        let mut forwarded = Vec::new();
        scanner.begin();
        let (n, load) = scanner.read(b"Ga=T,t=d,s=1_000,z=-5,H=+3;AAAA\x1b\\", |b| forwarded.extend_from_slice(b));
        assert!(load.is_none());
        assert_eq!(&b"Ga=T,t=d,s=1_000,z=-5,H=+3;AAAA\x1b\\"[n..], b"AAAA\x1b\\");
        let fwd_str = String::from_utf8_lossy(&forwarded);
        assert!(fwd_str.starts_with("Ga="), "spelled keys must not start with comma: {fwd_str:?}");
        assert!(fwd_str.contains("s=1000"), "underscore separator must be accepted: {fwd_str:?}");
        assert!(fwd_str.contains("z=-5"));
        assert!(fwd_str.contains("H=3"));

        // PassedOver high bytes (0x98, 0xff) inside a named payload are ignored rather than failing base64.
        scanner.begin();
        let (_, high_load) = scanner.read(b"Gt=f;\x98L2\xffE=\x1b\\", |_| {});
        assert_eq!(high_load.expect("high-byte load").source(), &Source::File(b"/a".to_vec()));

        // CAN (0x18) drops the string.
        scanner.begin();
        let (n, load) = scanner.read(b"Gt=f;L2E=\x18rest", |_| {});
        assert_eq!(n, 9);
        assert!(load.is_none());
        assert!(!scanner.reading());

        // Bad numeric value drops the command without forwarding.
        let mut dropped_fwd = Vec::new();
        scanner.begin();
        let (_, load) = scanner.read(b"Gt=f,s=1_2_;L2E=\x1b\\", |b| dropped_fwd.extend_from_slice(b));
        assert!(load.is_none());
        assert!(dropped_fwd.is_empty());

        // Named command ending directly at ST without `;` yields empty source.
        scanner.begin();
        let (_, load) = scanner.read(b"Gt=f\x1b\\", |_| {});
        let load = load.expect("empty named load");
        assert_eq!(load.source(), &Source::File(Vec::new()));

        // Shared memory command with invalid name "/" becomes Unreadable.
        scanner.begin();
        let (_, shm_bad) = scanner.read(b"Gt=s;Lw==\x1b\\", |_| {});
        assert_eq!(shm_bad.expect("load").source(), &Source::Unreadable);

        // Unsupported compression o=x is NOT treated as a named load (passed to core).
        let mut comp_fwd = Vec::new();
        scanner.begin();
        let (_, comp_load) = scanner.read(b"Gt=f,o=x;L2E=\x1b\\", |b| comp_fwd.extend_from_slice(b));
        assert!(comp_load.is_none());
        assert!(!comp_fwd.is_empty());

        // Single-byte chunk splitting produces identical result.
        let msg = b"Ga=T,t=f,i=42;L3RtcC9h\x1b\\";
        let mut chunked = Scanner::default();
        chunked.begin();
        let mut got = None;
        for byte in msg {
            let (_, l) = chunked.read(std::slice::from_ref(byte), |_| {});
            if l.is_some() {
                got = l;
            }
        }
        assert_eq!(got.expect("chunked load").source(), &Source::File(b"/tmp/a".to_vec()));
    }
}
