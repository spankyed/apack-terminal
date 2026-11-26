//! Total, overflow-safe, allocation-bounded binary reader and writer primitives.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::{MAX_BLOB_BYTES, MAX_STRING_BYTES, MAX_VEC_ITEMS};

/// Error returned when decoding an untrusted wire frame or message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Input ended before the expected field completed.
    UnexpectedEof,
    /// Unconsumed trailing bytes remained after decoding a message.
    TrailingBytes(usize),
    /// Frame payload length exceeded [`crate::MAX_FRAME_PAYLOAD`].
    FrameTooLarge(u32),
    /// Discriminant tag did not match any known variant.
    InvalidTag {
        /// Human-readable enum name.
        kind: &'static str,
        /// Raw wire tag value.
        tag: u16,
    },
    /// String payload was not valid UTF-8.
    InvalidUtf8,
    /// Integer varint exceeded its bit width.
    VarintOverflow,
    /// Decoded count or byte length exceeded a protocol safety bound.
    LimitExceeded(&'static str),
    /// Decoded value violated a domain invariant (e.g., zero grid dimension, invalid hex key).
    InvalidValue(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "unexpected end of wire input"),
            Self::TrailingBytes(n) => write!(f, "{n} trailing wire bytes after message"),
            Self::FrameTooLarge(n) => write!(f, "wire frame payload {n} bytes exceeds limit"),
            Self::InvalidTag { kind, tag } => write!(f, "invalid {kind} wire tag {tag}"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8 in wire string"),
            Self::VarintOverflow => write!(f, "LEB128 varint overflow"),
            Self::LimitExceeded(what) => write!(f, "wire limit exceeded: {what}"),
            Self::InvalidValue(what) => write!(f, "invalid wire value: {what}"),
        }
    }
}

impl core::error::Error for WireError {}

/// Append-only binary wire encoder writing into an owned `Vec<u8>`.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Creates an empty writer.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Creates a writer with pre-allocated capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: Vec::with_capacity(cap) }
    }

    /// Returns the currently allocated capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Consumes the writer and returns the encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Borrows the encoded bytes so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Clears the buffer while preserving capacity.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Writes a single byte.
    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Writes a boolean as `0` or `1`.
    pub fn bool(&mut self, value: bool) {
        self.buf.push(u8::from(value));
    }

    /// Writes a little-endian `u16`.
    pub fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a little-endian `u32`.
    pub fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a little-endian `i32`.
    pub fn i32(&mut self, value: i32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes a little-endian `u64`.
    pub fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes an unsigned LEB128 `u32`.
    pub fn var_u32(&mut self, mut value: u32) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte + 0x80);
        }
    }

    /// Writes an unsigned LEB128 `u64`.
    pub fn var_u64(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte + 0x80);
        }
    }

    /// Writes raw fixed-size bytes with no length prefix.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a length-prefixed byte slice.
    pub fn bytes(&mut self, bytes: &[u8]) {
        let len = u32::try_from(bytes.len()).expect("wire byte slice exceeds u32::MAX");
        self.var_u32(len);
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a length-prefixed UTF-8 string.
    pub fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    /// Writes an optional value as a presence byte (`0`/`1`) followed by the encoded value.
    pub fn opt<T>(&mut self, value: Option<&T>, encode: impl FnOnce(&mut Self, &T)) {
        match value {
            None => self.u8(0),
            Some(inner) => {
                self.u8(1);
                encode(self, inner);
            }
        }
    }

    /// Writes a length-prefixed sequence using `encode` for each element.
    pub fn slice<T>(&mut self, items: &[T], mut encode: impl FnMut(&mut Self, &T)) {
        let len = u32::try_from(items.len()).expect("wire slice exceeds u32::MAX");
        self.var_u32(len);
        for item in items {
            encode(self, item);
        }
    }
}

/// Zero-copy, bounds-checked binary wire decoder over an untrusted byte slice.
#[derive(Debug)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    /// Creates a reader over `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    /// Returns the number of unread bytes remaining.
    pub fn remaining(&self) -> usize {
        self.rest.len()
    }

    /// Ensures no unconsumed trailing bytes remain.
    pub fn finish(self) -> Result<(), WireError> {
        if self.rest.is_empty() { Ok(()) } else { Err(WireError::TrailingBytes(self.rest.len())) }
    }

    /// Reads an exact slice of `len` bytes.
    pub fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let (head, tail) = self.rest.split_at_checked(len).ok_or(WireError::UnexpectedEof)?;
        self.rest = tail;
        Ok(head)
    }

    /// Reads a fixed-size array of `N` bytes.
    pub fn fixed<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let bytes = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    /// Reads a single byte.
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.fixed::<1>()?[0])
    }

    /// Reads a strict boolean (`0` or `1`; any other byte is rejected).
    pub fn bool(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(WireError::InvalidTag { kind: "bool", tag: u16::from(other) }),
        }
    }

    /// Reads a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.fixed::<2>()?))
    }

    /// Reads a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.fixed::<4>()?))
    }

    /// Reads a little-endian `i32`.
    pub fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_le_bytes(self.fixed::<4>()?))
    }

    /// Reads a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.fixed::<8>()?))
    }

    /// Reads a canonical unsigned LEB128 `u32`, rejecting overflow past 32 bits and overlong encodings.
    pub fn var_u32(&mut self) -> Result<u32, WireError> {
        let mut value = 0u32;
        let mut shift = 0u32;
        for _ in 0..5 {
            let byte = self.u8()?;
            if shift > 0 && byte == 0 {
                return Err(WireError::InvalidValue("non-canonical varint"));
            }
            let low = u32::from(byte & 0x7F);
            if shift == 28 && low > 0x0F {
                return Err(WireError::VarintOverflow);
            }
            value += low << shift;
            if byte < 0x80 {
                return Ok(value);
            }
            shift += 7;
        }
        Err(WireError::VarintOverflow)
    }

    /// Reads a canonical unsigned LEB128 `u64`, rejecting overflow past 64 bits and overlong encodings.
    pub fn var_u64(&mut self) -> Result<u64, WireError> {
        let mut value = 0u64;
        let mut shift = 0u32;
        for _ in 0..10 {
            let byte = self.u8()?;
            if shift > 0 && byte == 0 {
                return Err(WireError::InvalidValue("non-canonical varint"));
            }
            let low = u64::from(byte & 0x7F);
            if shift == 63 && low > 0x01 {
                return Err(WireError::VarintOverflow);
            }
            value += low << shift;
            if byte < 0x80 {
                return Ok(value);
            }
            shift += 7;
        }
        Err(WireError::VarintOverflow)
    }

    /// Reads a length-prefixed byte slice capped at `max_len` (and [`MAX_BLOB_BYTES`]).
    pub fn bytes_bounded(&mut self, max_len: usize, what: &'static str) -> Result<&'a [u8], WireError> {
        let len = usize::try_from(self.var_u32()?).map_err(|_| WireError::LimitExceeded(what))?;
        if len > max_len.min(MAX_BLOB_BYTES) {
            return Err(WireError::LimitExceeded(what));
        }
        self.take(len)
    }

    /// Reads a length-prefixed byte slice capped at [`MAX_BLOB_BYTES`].
    pub fn bytes(&mut self) -> Result<Vec<u8>, WireError> {
        Ok(self.bytes_bounded(MAX_BLOB_BYTES, "bytes")?.to_vec())
    }

    /// Reads a length-prefixed UTF-8 string capped at [`MAX_STRING_BYTES`].
    pub fn str(&mut self) -> Result<String, WireError> {
        let raw = self.bytes_bounded(MAX_STRING_BYTES, "string")?;
        core::str::from_utf8(raw).map(ToOwned::to_owned).map_err(|_| WireError::InvalidUtf8)
    }

    /// Reads an optional value encoded with a strict `0`/`1` presence tag.
    pub fn opt<T>(&mut self, decode: impl FnOnce(&mut Self) -> Result<T, WireError>) -> Result<Option<T>, WireError> {
        if self.bool()? { Ok(Some(decode(self)?)) } else { Ok(None) }
    }

    /// Reads a length-prefixed sequence capped at `max_items` (and [`MAX_VEC_ITEMS`]).
    ///
    /// Never pre-allocates from the untrusted count so a hostile length prefix on a short packet
    /// allocates zero bytes before returning `UnexpectedEof`.
    pub fn vec_bounded<T>(
        &mut self,
        max_items: usize,
        what: &'static str,
        mut decode: impl FnMut(&mut Self) -> Result<T, WireError>,
    ) -> Result<Vec<T>, WireError> {
        let count = usize::try_from(self.var_u32()?).map_err(|_| WireError::LimitExceeded(what))?;
        if count > max_items.min(MAX_VEC_ITEMS) {
            return Err(WireError::LimitExceeded(what));
        }
        let mut out = Vec::new();
        for _ in 0..count {
            out.push(decode(self)?);
        }
        Ok(out)
    }
}
