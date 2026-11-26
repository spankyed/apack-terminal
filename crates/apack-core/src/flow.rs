//! Three-class flow control and strict-priority + WFQ outbound scheduler.
//!
//! - **Class A ([`FramePacer`]):** Byte-bounded BDP pacing (`max_frames` + `max_bytes` + RTT EWMA)
//!   for conflatable `SUB_VIEW` frames.
//! - **Class B ([`RawOutputFlow`]):** Per-stream `4 MiB` byte credit, `64 KiB` WFQ output chunking,
//!   UTF-8 boundary preservation with bounded `0..=3` byte debt for forward progress, and explicit
//!   `BACKPRESSURE_PTY` pause vs best-effort `OutputDropped` accounting.
//! - **Class C ([`OutboundQueue`]):** 5-tier priority scheduler (`Control > Frame > Event > Bulk WFQ > RawOutput WFQ`)
//!   with a hard `16 MiB` total byte cap per connection across all tiers.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroUsize;

use apack_proto::BULK_CHUNK_BYTES;
use apack_proto::messages::{BrokerMsg, FrameUpdate};
use apack_proto::types::{FrameCredit, RequestId, StreamId};

use crate::time::TimestampMs;
use crate::{MAX_BDP_FRAME_BYTES, MAX_CONTROL_QUEUE_BYTES, MAX_RAW_OUTPUT_CREDIT, MIN_BDP_FRAME_BYTES};

/// Error returned when a client violates a flow-control contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowError {
    /// `FrameAck.seq` regressed below the last ack, exceeded the last sent sequence, or was zero after frames were sent.
    InvalidAckSeq,
}

/// Error returned when pushing to an [`OutboundQueue`] would exceed its hard byte budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueOverflow;

#[derive(Debug, Clone, Copy)]
struct InFlightFrame {
    seq: u64,
    bytes: u32,
    sent_at: TimestampMs,
}

/// Class A: Byte-bounded BDP pacer for view frames on a single `SUB_VIEW` stream.
#[derive(Debug)]
pub struct FramePacer {
    credit: FrameCredit,
    in_flight: VecDeque<InFlightFrame>,
    in_flight_bytes: u32,
    next_seq: u64,
    last_acked_seq: u64,
    ewma_rtt_ms: Option<u32>,
}

impl FramePacer {
    /// Creates a new pacer with the client's initial `FrameCredit`.
    pub fn new(credit: FrameCredit) -> Self {
        Self {
            credit,
            in_flight: VecDeque::new(),
            in_flight_bytes: 0,
            next_seq: 1,
            last_acked_seq: 0,
            ewma_rtt_ms: None,
        }
    }

    /// Returns the effective clamped byte budget for in-flight frames.
    pub fn effective_max_bytes(&self) -> u32 {
        self.credit.max_bytes.clamp(MIN_BDP_FRAME_BYTES, MAX_BDP_FRAME_BYTES)
    }

    /// Returns the sequence number that will be assigned to the next sent frame.
    pub fn peek_next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Returns the current RTT EWMA in milliseconds, if at least one frame has been acked.
    pub fn ewma_rtt_ms(&self) -> Option<u32> {
        self.ewma_rtt_ms
    }

    /// Returns the number of currently unacknowledged frames in flight.
    pub fn in_flight_frames(&self) -> usize {
        self.in_flight.len()
    }

    /// Returns total bytes of currently unacknowledged frames in flight.
    pub fn in_flight_bytes(&self) -> u32 {
        self.in_flight_bytes
    }

    /// Returns `true` when the client paused delivery (`max_frames == 0`, e.g., hidden tab).
    pub fn is_paused(&self) -> bool {
        self.credit.max_frames == 0
    }

    /// Returns `true` if a frame of `next_frame_bytes` may be sent without violating BDP bounds.
    ///
    /// When `in_flight` is empty and `max_frames >= 1`, a single frame is always permitted so a
    /// large initial full-sync frame can never deadlock the stream.
    pub fn can_send(&self, next_frame_bytes: u32) -> bool {
        if self.credit.max_frames == 0 {
            return false;
        }
        if self.in_flight.len() >= usize::from(self.credit.max_frames) {
            return false;
        }
        if self.in_flight.is_empty() {
            return true;
        }
        self.in_flight_bytes.saturating_add(next_frame_bytes) <= self.effective_max_bytes()
    }

    /// Records a newly transmitted frame and returns its monotonic sequence number.
    pub fn on_send(&mut self, bytes: u32, now: TimestampMs) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.in_flight.push_back(InFlightFrame { seq, bytes, sent_at: now });
        self.in_flight_bytes = self.in_flight_bytes.saturating_add(bytes);
        seq
    }

    /// Applies a client `FrameAck`, updating in-flight accounting, RTT EWMA, and credit window.
    ///
    /// `seq == 0` is permitted iff no frames have been sent yet (`last_sent == 0`), allowing a
    /// hidden view attached with `max_frames == 0` to un-pause by granting credit.
    pub fn on_ack(&mut self, seq: u64, credit: FrameCredit, now: TimestampMs) -> Result<(), FlowError> {
        let last_sent = self.next_seq - 1;
        if (seq == 0 && last_sent > 0) || seq > last_sent || seq < self.last_acked_seq {
            return Err(FlowError::InvalidAckSeq);
        }
        let mut acked_sent_at = None;
        while let Some(front) = self.in_flight.front() {
            if front.seq > seq {
                break;
            }
            let entry = self.in_flight.pop_front().expect("checked front");
            self.in_flight_bytes = self.in_flight_bytes.saturating_sub(entry.bytes);
            if entry.seq == seq {
                acked_sent_at = Some(entry.sent_at);
            }
        }
        if let Some(sent_at) = acked_sent_at {
            let sample = now.saturating_sub(sent_at).min(u64::from(u32::MAX)) as u32;
            self.ewma_rtt_ms = Some(match self.ewma_rtt_ms {
                None => sample,
                Some(prev) => ((u64::from(prev) * 3 + u64::from(sample)) / 4) as u32,
            });
        }
        self.last_acked_seq = seq;
        self.credit = credit;
        Ok(())
    }
}

/// Result of feeding PTY output bytes or replenishing credit on a [`RawOutputFlow`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawOutputDelta {
    /// Valid UTF-8 chunks (`<= BULK_CHUNK_BYTES` each) ready to emit in `BrokerMsg::Output`.
    pub emit_chunks: Vec<String>,
    /// Microseconds spent throttled if backpressure just released (`BrokerMsg::Throttled`).
    pub throttled_us: Option<u64>,
    /// Explicitly accounted shed bytes in best-effort mode or on pending overflow (`BrokerMsg::OutputDropped`).
    pub dropped_bytes: u64,
    /// Current desired PTY pause state for this stream (`true` when backpressure is engaged).
    pub pty_paused: bool,
}

/// Class B: Per-stream `SUB_RAW_OUTPUT` byte-credit and UTF-8 boundary controller.
#[derive(Debug)]
pub struct RawOutputFlow {
    credit_bytes: u32,
    credit_debt: u8,
    max_pending_bytes: usize,
    max_chunk_bytes: usize,
    backpressure_pty: bool,
    utf8_tail: [u8; 3],
    utf8_tail_len: usize,
    pending_utf8: Vec<u8>,
    throttled_since: Option<TimestampMs>,
    total_throttled_us: u64,
}

impl RawOutputFlow {
    /// Creates a new per-stream raw-output controller, clamping initial credit to [`MAX_RAW_OUTPUT_CREDIT`].
    pub fn new(initial_credit: u32, backpressure_pty: bool) -> Self {
        Self::with_limits(initial_credit, backpressure_pty, MAX_RAW_OUTPUT_CREDIT as usize, BULK_CHUNK_BYTES)
    }

    /// Creates a raw-output controller with custom bounded pending and chunk caps (used in tests/sim).
    pub fn with_limits(
        initial_credit: u32,
        backpressure_pty: bool,
        max_pending_bytes: usize,
        max_chunk_bytes: usize,
    ) -> Self {
        Self {
            credit_bytes: initial_credit.min(MAX_RAW_OUTPUT_CREDIT),
            credit_debt: 0,
            max_pending_bytes: max_pending_bytes.min(MAX_RAW_OUTPUT_CREDIT as usize),
            max_chunk_bytes: max_chunk_bytes.clamp(4, BULK_CHUNK_BYTES),
            backpressure_pty,
            utf8_tail: [0; 3],
            utf8_tail_len: 0,
            pending_utf8: Vec::new(),
            throttled_since: None,
            total_throttled_us: 0,
        }
    }

    /// Remaining byte credit available on this stream.
    pub fn credit_bytes(&self) -> u32 {
        self.credit_bytes
    }

    /// Outstanding UTF-8 multi-byte credit debt (`0..=3`) paid down from the next `OutputCredit`.
    pub fn credit_debt(&self) -> u8 {
        self.credit_debt
    }

    /// Returns `true` when this stream is requesting PTY backpressure pause.
    pub fn is_pty_paused(&self) -> bool {
        self.backpressure_pty && self.throttled_since.is_some()
    }

    /// Cumulative microseconds spent throttled on this stream.
    pub fn total_throttled_us(&self) -> u64 {
        self.total_throttled_us
    }

    /// Replenishes byte credit (paying down any `0..=3` UTF-8 boundary debt first) and drains buffered output.
    pub fn grant_credit(&mut self, bytes: u32, now: TimestampMs) -> RawOutputDelta {
        let paid = bytes.min(u32::from(self.credit_debt));
        self.credit_debt -= paid as u8;
        let rem = bytes - paid;
        self.credit_bytes = self.credit_bytes.saturating_add(rem).min(MAX_RAW_OUTPUT_CREDIT);

        let mut delta = RawOutputDelta::default();
        self.drain_pending_chunks(&mut delta.emit_chunks);
        if self.credit_bytes > 0
            && self.pending_utf8.is_empty()
            && let Some(since) = self.throttled_since.take()
        {
            let waited_us = now.saturating_sub(since).saturating_mul(1_000);
            self.total_throttled_us = self.total_throttled_us.saturating_add(waited_us);
            delta.throttled_us = Some(waited_us);
        }
        delta.pty_paused = self.is_pty_paused();
        delta
    }

    /// Ingests raw PTY bytes, preserving split UTF-8 sequences and enforcing credit/backpressure.
    pub fn ingest(&mut self, raw: &[u8], now: TimestampMs) -> RawOutputDelta {
        let mut delta = RawOutputDelta::default();
        if raw.is_empty() {
            delta.pty_paused = self.is_pty_paused();
            return delta;
        }

        let mut combined = Vec::new();
        combined.extend_from_slice(&self.utf8_tail[..self.utf8_tail_len]);
        combined.extend_from_slice(raw);
        self.utf8_tail_len = 0;

        let decoded = decode_utf8_streaming(&combined, &mut self.utf8_tail, &mut self.utf8_tail_len);
        if decoded.is_empty() {
            delta.pty_paused = self.is_pty_paused();
            return delta;
        }

        if self.backpressure_pty {
            // Drain directly against available credit first when pending is empty so available credit
            // is never prematurely truncated by `max_pending_bytes`.
            let mut offset = 0usize;
            if self.pending_utf8.is_empty() {
                while offset < decoded.len() && self.credit_bytes > 0 {
                    let take = self.take_chunk_len(&decoded[offset..]).get();
                    delta.emit_chunks.push(String::from(&decoded[offset..offset + take]));
                    offset += take;
                }
            }
            let rest = &decoded[offset..];
            let room = self.max_pending_bytes.saturating_sub(self.pending_utf8.len());
            let keep = floor_char_boundary(rest, room);
            self.pending_utf8.extend_from_slice(&rest.as_bytes()[..keep]);
            delta.dropped_bytes = (rest.len() - keep) as u64;
            if (!self.pending_utf8.is_empty() || self.credit_bytes == 0) && self.throttled_since.is_none() {
                self.throttled_since = Some(now);
            }
        } else {
            let mut offset = 0usize;
            while offset < decoded.len() && self.credit_bytes > 0 {
                let take = self.take_chunk_len(&decoded[offset..]).get();
                delta.emit_chunks.push(String::from(&decoded[offset..offset + take]));
                offset += take;
            }
            delta.dropped_bytes = (decoded.len() - offset) as u64;
        }
        delta.pty_paused = self.is_pty_paused();
        delta
    }

    fn drain_pending_chunks(&mut self, out: &mut Vec<String>) {
        while !self.pending_utf8.is_empty() && self.credit_bytes > 0 {
            let text = core::str::from_utf8(&self.pending_utf8).expect("pending_utf8 always holds valid UTF-8");
            let take = take_chunk_len(&mut self.credit_bytes, &mut self.credit_debt, self.max_chunk_bytes, text).get();
            out.push(String::from(&text[..take]));
            self.pending_utf8.drain(..take);
        }
    }

    fn take_chunk_len(&mut self, text: &str) -> NonZeroUsize {
        take_chunk_len(&mut self.credit_bytes, &mut self.credit_debt, self.max_chunk_bytes, text)
    }
}

/// Consumes at least one complete UTF-8 scalar (up to `min(credit_bytes, max_chunk_bytes)`),
/// recording at most `3` bytes of `credit_debt` when `1 <= credit_bytes < first_char.len_utf8()`.
///
/// Returning [`NonZeroUsize`] makes a zero-progress infinite loop unrepresentable.
/// Caller guarantees `!text.is_empty()` and `*credit_bytes > 0`.
fn take_chunk_len(credit_bytes: &mut u32, credit_debt: &mut u8, max_chunk_bytes: usize, text: &str) -> NonZeroUsize {
    let limit = (*credit_bytes as usize).min(max_chunk_bytes);
    if let Some(take) = NonZeroUsize::new(floor_char_boundary(text, limit)) {
        *credit_bytes -= take.get() as u32;
        return take;
    }
    let ch_len = text.chars().next().expect("non-empty text").len_utf8();
    *credit_debt = (ch_len - *credit_bytes as usize) as u8;
    *credit_bytes = 0;
    NonZeroUsize::new(ch_len).expect("UTF-8 char len is 1..=4")
}

/// Decodes valid UTF-8 from `input`, replacing malformed interior sequences with U+FFFD and storing
/// any trailing incomplete multibyte sequence (`1..=3` bytes) into `tail`.
fn decode_utf8_streaming(mut input: &[u8], tail: &mut [u8; 3], tail_len: &mut usize) -> String {
    let mut out = String::new();
    while !input.is_empty() {
        match core::str::from_utf8(input) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                out.push_str(core::str::from_utf8(&input[..valid_len]).expect("validated prefix"));
                match err.error_len() {
                    Some(bad_len) => {
                        out.push('\u{FFFD}');
                        input = &input[valid_len + bad_len..];
                    }
                    None => {
                        let rem = &input[valid_len..];
                        *tail_len = rem.len();
                        tail[..rem.len()].copy_from_slice(rem);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// Largest UTF-8 character boundary index `<= max_bytes` inside `s`.
fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut i = max_bytes;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Strict scheduling priority for non-conflatable outbound messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundPriority {
    /// Priority 0: Control replies, handshake, auth, errors, stream lifecycle.
    Control,
    /// Priority 2: Ordered side-effect events (`Title`, `Bell`, `ProgramStatus`, `Exited`, `SearchDelta`).
    Event,
    /// Priority 3: Fair-queued 64 KiB bulk payloads (`ImageBlobChunk`, `Rows`, `SearchMatches`, `CommandsReply`).
    Bulk,
    /// Priority 4: Fair-queued `SUB_RAW_OUTPUT` chunks (`Output`, `Throttled`, `OutputDropped`, exit `StreamClosed`).
    RawOutput,
}

/// One scheduled outbound message ready to be encoded and written to a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedMessage {
    /// Target stream (`StreamId::CONTROL` or attached session stream).
    pub stream_id: StreamId,
    /// Request correlation ID (`RequestId::PUSH` for unsolicited pushes).
    pub req_id: RequestId,
    /// Message to send.
    pub msg: BrokerMsg,
    /// Encoded byte size accounted against queue limits.
    pub encoded_bytes: usize,
}

#[derive(Debug, Default)]
struct WfqQueues {
    order: VecDeque<StreamId>,
    by_stream: BTreeMap<StreamId, VecDeque<QueuedMessage>>,
}

impl WfqQueues {
    fn push(&mut self, msg: QueuedMessage) {
        let sid = msg.stream_id;
        let q = self.by_stream.entry(sid).or_default();
        if q.is_empty() {
            self.order.push_back(sid);
        }
        q.push_back(msg);
    }

    fn pop(&mut self) -> Option<QueuedMessage> {
        while let Some(sid) = self.order.pop_front() {
            if let Some(q) = self.by_stream.get_mut(&sid)
                && let Some(item) = q.pop_front()
            {
                if q.is_empty() {
                    self.by_stream.remove(&sid);
                } else {
                    self.order.push_back(sid);
                }
                return Some(item);
            }
        }
        None
    }

    fn remove_stream(&mut self, sid: StreamId) -> usize {
        let mut freed = 0usize;
        if let Some(q) = self.by_stream.remove(&sid) {
            for item in q {
                freed += item.encoded_bytes;
            }
        }
        self.order.retain(|s| *s != sid);
        freed
    }
}

/// Class C: 5-tier strict-priority + WFQ outbound queue enforcing a hard total byte cap per connection.
#[derive(Debug)]
pub struct OutboundQueue {
    max_total_bytes: usize,
    ordered_bytes: usize,
    frame_bytes: usize,
    p0_control: VecDeque<QueuedMessage>,
    p1_frame_order: VecDeque<StreamId>,
    p1_frames: BTreeMap<StreamId, QueuedMessage>,
    p2_events: VecDeque<QueuedMessage>,
    p3_bulk: WfqQueues,
    p4_raw: WfqQueues,
}

impl Default for OutboundQueue {
    fn default() -> Self {
        Self::new(MAX_CONTROL_QUEUE_BYTES)
    }
}

impl OutboundQueue {
    /// Creates an outbound queue capped at `max_total_bytes` across ordered and frame tiers.
    pub fn new(max_total_bytes: usize) -> Self {
        Self {
            max_total_bytes,
            ordered_bytes: 0,
            frame_bytes: 0,
            p0_control: VecDeque::new(),
            p1_frame_order: VecDeque::new(),
            p1_frames: BTreeMap::new(),
            p2_events: VecDeque::new(),
            p3_bulk: WfqQueues::default(),
            p4_raw: WfqQueues::default(),
        }
    }

    /// Returns bytes currently buffered across all non-conflatable ordered tiers.
    pub fn ordered_bytes(&self) -> usize {
        self.ordered_bytes
    }

    /// Returns bytes currently buffered in pending Priority 1 frames.
    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// Returns total bytes buffered across all tiers (`ordered_bytes + frame_bytes`).
    pub fn total_bytes(&self) -> usize {
        self.ordered_bytes + self.frame_bytes
    }

    /// Returns the number of pending view frames (at most 1 per stream).
    pub fn pending_frames(&self) -> usize {
        self.p1_frames.len()
    }

    /// Returns `true` if `stream_id` already has an unsent frame waiting in Priority 1.
    pub fn has_pending_frame(&self, stream_id: StreamId) -> bool {
        self.p1_frames.contains_key(&stream_id)
    }

    /// Returns `true` when no messages are queued in any tier.
    pub fn is_empty(&self) -> bool {
        self.p0_control.is_empty()
            && self.p1_frames.is_empty()
            && self.p2_events.is_empty()
            && self.p3_bulk.by_stream.is_empty()
            && self.p4_raw.by_stream.is_empty()
    }

    /// Returns `true` if inserting/replacing a frame of `encoded_bytes` on `stream_id` fits in the budget.
    pub fn can_fit_frame(&self, stream_id: StreamId, encoded_bytes: usize) -> bool {
        let prev_bytes = self.p1_frames.get(&stream_id).map_or(0, |m| m.encoded_bytes);
        (self.total_bytes() - prev_bytes).checked_add(encoded_bytes).is_some_and(|n| n <= self.max_total_bytes)
    }

    /// Enqueues a Priority 1 view frame for `stream_id`, enforcing the total per-connection byte cap.
    pub fn push_frame(
        &mut self,
        stream_id: StreamId,
        frame: Box<FrameUpdate>,
        encoded_bytes: usize,
    ) -> Result<(), QueueOverflow> {
        let prev_bytes = self.p1_frames.get(&stream_id).map_or(0, |m| m.encoded_bytes);
        let next_total = (self.total_bytes() - prev_bytes).checked_add(encoded_bytes).ok_or(QueueOverflow)?;
        if next_total > self.max_total_bytes {
            return Err(QueueOverflow);
        }
        self.frame_bytes = self.frame_bytes - prev_bytes + encoded_bytes;
        let item = QueuedMessage { stream_id, req_id: RequestId::PUSH, msg: BrokerMsg::Frame(frame), encoded_bytes };
        if self.p1_frames.insert(stream_id, item).is_none() {
            self.p1_frame_order.push_back(stream_id);
        }
        Ok(())
    }

    /// Enqueues a non-conflatable ordered message, returning [`QueueOverflow`] if the hard byte cap would be exceeded.
    pub fn push_ordered(
        &mut self,
        priority: OutboundPriority,
        stream_id: StreamId,
        req_id: RequestId,
        msg: BrokerMsg,
        encoded_bytes: usize,
    ) -> Result<(), QueueOverflow> {
        let next_total = self.total_bytes().checked_add(encoded_bytes).ok_or(QueueOverflow)?;
        if next_total > self.max_total_bytes {
            return Err(QueueOverflow);
        }
        self.ordered_bytes += encoded_bytes;
        let item = QueuedMessage { stream_id, req_id, msg, encoded_bytes };
        match priority {
            OutboundPriority::Control => self.p0_control.push_back(item),
            OutboundPriority::Event => self.p2_events.push_back(item),
            OutboundPriority::Bulk => self.p3_bulk.push(item),
            OutboundPriority::RawOutput => self.p4_raw.push(item),
        }
        Ok(())
    }

    /// Pops the next message in strict priority order: `Control > Frame > Event > Bulk (WFQ) > RawOutput (WFQ)`.
    pub fn pop_next(&mut self) -> Option<QueuedMessage> {
        if let Some(item) = self.p0_control.pop_front() {
            self.ordered_bytes -= item.encoded_bytes;
            return Some(item);
        }
        while let Some(sid) = self.p1_frame_order.pop_front() {
            if let Some(item) = self.p1_frames.remove(&sid) {
                self.frame_bytes -= item.encoded_bytes;
                return Some(item);
            }
        }
        if let Some(item) = self.p2_events.pop_front() {
            self.ordered_bytes -= item.encoded_bytes;
            return Some(item);
        }
        if let Some(item) = self.p3_bulk.pop() {
            self.ordered_bytes -= item.encoded_bytes;
            return Some(item);
        }
        if let Some(item) = self.p4_raw.pop() {
            self.ordered_bytes -= item.encoded_bytes;
            return Some(item);
        }
        None
    }

    /// Removes only pending Priority 1 frames for `stream_id` (preserving queued `Exited` / final `Output`).
    pub fn remove_stream_frames(&mut self, stream_id: StreamId) {
        if let Some(item) = self.p1_frames.remove(&stream_id) {
            self.frame_bytes -= item.encoded_bytes;
            self.p1_frame_order.retain(|s| *s != stream_id);
        }
    }

    /// Removes all pending frames, events, bulk chunks, and raw output queued for a detached stream.
    pub fn remove_stream(&mut self, stream_id: StreamId) {
        self.remove_stream_frames(stream_id);
        let mut freed = 0usize;
        self.p2_events.retain(|item| {
            if item.stream_id == stream_id {
                freed += item.encoded_bytes;
                false
            } else {
                true
            }
        });
        freed += self.p3_bulk.remove_stream(stream_id);
        freed += self.p4_raw.remove_stream(stream_id);
        self.ordered_bytes -= freed;
    }
}
