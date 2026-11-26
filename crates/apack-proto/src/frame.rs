//! Streaming binary frame encoder and decoder.
//!
//! Wire layout from byte 0:
//! - `body_len == 0` (`[0, 0, 0, 0]`) → universal clean `FIN` (4 bytes total, no inner header).
//! - `10 <= body_len <= 10 + MAX_FRAME_PAYLOAD` →
//!   `[body_len: u32 LE][stream_id: u32 LE][msg_type: u16 LE][req_id: u32 LE][payload: body_len - 10]`.
//! - `1 <= body_len < 10` → malformed (`WireError::InvalidValue("frame shorter than header")`).
//!
//! Empty-payload messages (e.g., `Bell`, `ListSessions`, `Metrics`) carry `body_len == 10` and
//! never collide with `[0, 0, 0, 0]` `FIN`. The outer 4-byte length prefix is identical across
//! transports and zero-sniff proxies.

use crate::codec::WireError;
use crate::types::{RequestId, StreamId};
use crate::{FIN_FRAME, FRAME_HEADER_LEN, MAX_FRAME_PAYLOAD};

/// Size of the fixed metadata inside a frame body: `stream_id(4) + msg_type(2) + req_id(4)`.
pub const INNER_HEADER_LEN: usize = FRAME_HEADER_LEN - 4;

/// Maximum valid outer `body_len` (`INNER_HEADER_LEN + MAX_FRAME_PAYLOAD`).
pub const MAX_BODY_LEN: usize = INNER_HEADER_LEN + MAX_FRAME_PAYLOAD;

/// Decoded header of a binary protocol frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Multiplexed stream (`StreamId::CONTROL` for control plane, monotonic IDs for sessions).
    pub stream_id: StreamId,
    /// Message type discriminant.
    pub msg_type: u16,
    /// Correlation ID (`RequestId::PUSH` for unsolicited events).
    pub req_id: RequestId,
    /// Payload length in bytes (`0..=MAX_FRAME_PAYLOAD`).
    pub payload_len: u32,
}

/// Output of [`FrameDecoder::next_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedFrame {
    /// Clean universal end-of-input sentinel (`[0, 0, 0, 0]`).
    Fin,
    /// Complete protocol frame ready for message decoding.
    Frame {
        /// Parsed frame header.
        header: FrameHeader,
        /// Owned payload bytes (`payload.len() == header.payload_len as usize`).
        payload: Vec<u8>,
    },
}

/// Builds the 14-byte outer + inner frame header for scatter-gather (`writev`) transmission.
///
/// Returns `Err(WireError::FrameTooLarge)` if `payload_len > MAX_FRAME_PAYLOAD`.
pub fn encode_header(
    stream_id: StreamId,
    msg_type: u16,
    req_id: RequestId,
    payload_len: usize,
) -> Result<[u8; FRAME_HEADER_LEN], WireError> {
    if payload_len > MAX_FRAME_PAYLOAD {
        let reported = u32::try_from(payload_len).unwrap_or(u32::MAX);
        return Err(WireError::FrameTooLarge(reported));
    }
    let body_len = (INNER_HEADER_LEN + payload_len) as u32;
    let mut header = [0u8; FRAME_HEADER_LEN];
    header[0..4].copy_from_slice(&body_len.to_le_bytes());
    header[4..8].copy_from_slice(&stream_id.raw().to_le_bytes());
    header[8..10].copy_from_slice(&msg_type.to_le_bytes());
    header[10..14].copy_from_slice(&req_id.raw().to_le_bytes());
    Ok(header)
}

/// Encodes a complete frame (header + payload) into `out`.
pub fn encode_frame(
    stream_id: StreamId,
    msg_type: u16,
    req_id: RequestId,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), WireError> {
    let header = encode_header(stream_id, msg_type, req_id, payload.len())?;
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    Ok(())
}

/// Appends the universal 4-byte `FIN` sentinel to `out`.
pub fn encode_fin(out: &mut Vec<u8>) {
    out.extend_from_slice(&FIN_FRAME);
}

#[derive(Debug)]
enum State {
    /// Accumulating the 4-byte outer `body_len`.
    Length { buf: [u8; 4], have: usize },
    /// Accumulating `body_len` bytes (`INNER_HEADER_LEN..=MAX_BODY_LEN`).
    Body { expected: usize, buf: Vec<u8> },
    /// Saw `FIN` or a fatal framing error; no further frames can be decoded.
    Closed,
}

/// Total, streaming, memory-bounded binary frame decoder invariant across arbitrary chunk splits.
#[derive(Debug)]
pub struct FrameDecoder {
    state: State,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder {
    /// Creates a new frame decoder at the start of a stream.
    pub fn new() -> Self {
        Self { state: State::Length { buf: [0; 4], have: 0 } }
    }

    /// Feeds bytes from `input` into the decoder, returning either:
    /// - `Ok((consumed, Some(frame)))`: a complete frame (or `Fin`) was decoded after consuming `consumed` bytes.
    /// - `Ok((consumed, None))`: all of `input` was consumed and more bytes are needed.
    /// - `Err(err)`: framing protocol violation (length > `MAX_BODY_LEN` or `1..10`); decoder enters `Closed`.
    ///
    /// Callers loop while `!input.is_empty()` to drain multiple frames from a single read chunk.
    pub fn step(&mut self, input: &[u8]) -> Result<(usize, Option<DecodedFrame>), WireError> {
        let mut offset = 0usize;
        while offset < input.len() {
            match &mut self.state {
                State::Closed => return Ok((input.len(), None)),
                State::Length { buf, have } => {
                    let take = (4 - *have).min(input.len());
                    buf[*have..*have + take].copy_from_slice(&input[..take]);
                    *have += take;
                    offset += take;
                    if *have == 4 {
                        let body_len = u32::from_le_bytes(*buf);
                        if body_len == 0 {
                            self.state = State::Closed;
                            return Ok((offset, Some(DecodedFrame::Fin)));
                        }
                        let expected = usize::try_from(body_len).map_err(|_| {
                            self.state = State::Closed;
                            WireError::FrameTooLarge(body_len)
                        })?;
                        if expected < INNER_HEADER_LEN {
                            self.state = State::Closed;
                            return Err(WireError::InvalidValue("frame shorter than header"));
                        }
                        if expected > MAX_BODY_LEN {
                            self.state = State::Closed;
                            return Err(WireError::FrameTooLarge(body_len - INNER_HEADER_LEN as u32));
                        }
                        // Start with an empty buffer so a huge declared length on a tiny stalled
                        // chunk allocates nothing until body bytes actually arrive.
                        self.state = State::Body { expected, buf: Vec::new() };
                    }
                }
                State::Body { expected, buf } => {
                    let need = *expected - buf.len();
                    let take = need.min(input.len() - offset);
                    buf.extend_from_slice(&input[offset..offset + take]);
                    offset += take;
                    if buf.len() == *expected {
                        let body = std::mem::take(buf);
                        self.state = State::Length { buf: [0; 4], have: 0 };
                        let stream_id = StreamId::from_raw(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
                        let msg_type = u16::from_le_bytes([body[4], body[5]]);
                        let req_id = RequestId::from_raw(u32::from_le_bytes([body[6], body[7], body[8], body[9]]));
                        let payload = body[INNER_HEADER_LEN..].to_vec();
                        let payload_len = payload.len() as u32;
                        return Ok((
                            offset,
                            Some(DecodedFrame::Frame {
                                header: FrameHeader { stream_id, msg_type, req_id, payload_len },
                                payload,
                            }),
                        ));
                    }
                }
            }
        }
        Ok((offset, None))
    }

    /// Returns `true` if the decoder has seen `Fin` or a fatal framing error.
    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed)
    }
}
