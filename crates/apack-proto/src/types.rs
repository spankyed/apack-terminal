//! Correct-by-construction domain types shared across the broker core and wire protocol.

use alloc::string::String;
use alloc::vec::Vec;
use core::num::{NonZeroI32, NonZeroU16, NonZeroU32, NonZeroU64};

use crate::codec::{Reader, WireError, Writer};
use crate::{MAX_GRID_COLS, MAX_GRID_ROWS, MAX_LEASE_GRACE_MS};

/// Non-zero session identifier assigned by the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(NonZeroU64);

impl SessionId {
    /// Creates a `SessionId` from a raw `u64`, returning `None` for `0`.
    pub fn new(raw: u64) -> Option<Self> {
        NonZeroU64::new(raw).map(Self)
    }

    /// Returns the underlying non-zero `u64`.
    pub fn get(self) -> u64 {
        self.0.get()
    }

    pub(crate) fn encode(self, w: &mut Writer) {
        w.var_u64(self.0.get());
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Self::new(r.var_u64()?).ok_or(WireError::InvalidValue("zero session id"))
    }
}

/// Multiplexed stream identifier (`0` is the control stream; `>= 1` are per-session attachments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u32);

impl StreamId {
    /// Stream `0`: broker-wide control plane (handshake, leases, spawn/kill/list/rewind/metrics).
    pub const CONTROL: Self = Self(0);

    /// Wraps a raw wire `u32`.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw `u32`.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns `true` for the control stream (`0`).
    pub const fn is_control(self) -> bool {
        self.0 == 0
    }
}

/// Non-zero multiplexed stream identifier (`>= 1`), making [`StreamId::CONTROL`] unrepresentable
/// in session-bound messages (`Attach`, `Detach`, `Attached`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NonZeroStreamId(NonZeroU32);

impl NonZeroStreamId {
    /// Creates a `NonZeroStreamId`, returning `None` for `0` (`StreamId::CONTROL`).
    pub fn new(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(Self)
    }

    /// Returns the raw non-zero `u32`.
    pub const fn raw(self) -> u32 {
        self.0.get()
    }

    /// Widens into a [`StreamId`].
    pub const fn as_stream_id(self) -> StreamId {
        StreamId(self.0.get())
    }

    pub(crate) fn encode(self, w: &mut Writer) {
        w.u32(self.0.get());
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Self::new(r.u32()?).ok_or(WireError::InvalidValue("stream id must be non-zero"))
    }
}

/// Per-connection request correlation identifier (`0` marks unsolicited push events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u32);

impl RequestId {
    /// Unsolicited server push event (`req_id == 0`).
    pub const PUSH: Self = Self(0);

    /// Wraps a raw wire `u32`.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw `u32`.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Returns `true` when this ID represents a push event (`0`).
    pub const fn is_push(self) -> bool {
        self.0 == 0
    }
}

/// Strictly positive POSIX process ID (`> 0`), making `kill(0)` and `kill(-1)` unrepresentable.
///
/// The inner field is private and there is no `Default`, so neither caller input nor mutation testing
/// can fabricate a broadcast target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SignalTarget(NonZeroI32);

impl SignalTarget {
    /// Constructs a `SignalTarget` iff `pid >= 1`.
    pub fn new(pid: i32) -> Option<Self> {
        if pid < 1 {
            return None;
        }
        NonZeroI32::new(pid).map(Self)
    }

    /// Returns the strictly positive process ID.
    pub fn pid(self) -> i32 {
        self.0.get()
    }

    pub(crate) fn encode(self, w: &mut Writer) {
        w.i32(self.0.get());
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Self::new(r.i32()?).ok_or(WireError::InvalidValue("signal target pid must be > 0"))
    }
}

/// POSIX signal kinds accepted by `Request::Signal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SignalKind {
    /// `SIGHUP`
    Hup = 1,
    /// `SIGINT`
    Int = 2,
    /// `SIGQUIT`
    Quit = 3,
    /// `SIGTERM`
    Term = 4,
    /// `SIGKILL`
    Kill = 5,
    /// `SIGUSR1`
    Usr1 = 6,
    /// `SIGUSR2`
    Usr2 = 7,
    /// `SIGSTOP`
    Stop = 8,
    /// `SIGCONT`
    Cont = 9,
}

impl SignalKind {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            1 => Ok(Self::Hup),
            2 => Ok(Self::Int),
            3 => Ok(Self::Quit),
            4 => Ok(Self::Term),
            5 => Ok(Self::Kill),
            6 => Ok(Self::Usr1),
            7 => Ok(Self::Usr2),
            8 => Ok(Self::Stop),
            9 => Ok(Self::Cont),
            other => Err(WireError::InvalidTag { kind: "SignalKind", tag: u16::from(other) }),
        }
    }
}

/// Which process(es) inside a session receive a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalScope {
    /// The session leader process itself.
    Leader,
    /// The leader's process group (`kill(-leader_pid)`).
    Group,
    /// One specific process, verified to belong to the session's process tree.
    Pid(SignalTarget),
}

impl SignalScope {
    pub(crate) fn encode(self, w: &mut Writer) {
        match self {
            Self::Leader => w.u8(0),
            Self::Group => w.u8(1),
            Self::Pid(target) => {
                w.u8(2);
                target.encode(w);
            }
        }
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Leader),
            1 => Ok(Self::Group),
            2 => Ok(Self::Pid(SignalTarget::decode(r)?)),
            other => Err(WireError::InvalidTag { kind: "SignalScope", tag: u16::from(other) }),
        }
    }
}

/// Validated 32-byte lowercase ASCII hex key used to vouch for shell marks (`OSC 633;V`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarksKey([u8; 32]);

impl MarksKey {
    /// Parses a 32-character lowercase hexadecimal key.
    pub fn parse(input: &[u8]) -> Option<Self> {
        if input.len() != 32 {
            return None;
        }
        if !input.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return None;
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(input);
        Some(Self(key))
    }

    /// Returns the 32 lowercase hex ASCII bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the key as a `&str`.
    pub fn as_str(&self) -> &str {
        // Every byte was validated as ASCII hex in `parse`.
        core::str::from_utf8(&self.0).expect("MarksKey is valid ASCII hex")
    }

    /// Encodes the 32-byte key into `w`.
    pub fn encode(&self, w: &mut Writer) {
        w.raw(&self.0);
    }

    /// Decodes and validates a 32-byte lowercase hex key from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let bytes = r.fixed::<32>()?;
        Self::parse(&bytes).ok_or(WireError::InvalidValue("marks key must be 32 lowercase hex chars"))
    }
}

impl core::fmt::Debug for MarksKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MarksKey([REDACTED])")
    }
}

/// Exit status proven by a keyed mark (`MarksKey`), distinct in type from untrusted claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VouchedExitCode(i32);

impl VouchedExitCode {
    /// Constructs a vouched exit code from a verified keyed mark or wire `Exited` variant.
    pub const fn from_vouched(code: i32) -> Self {
        Self(code)
    }

    /// Returns the numeric exit status.
    pub const fn code(self) -> i32 {
        self.0
    }
}

/// Why a keyed session's command ended without a vouched exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum UnknownReason {
    /// Shell emitted a finish mark without a numeric exit code.
    NoStatus = 0,
    /// A newer keyed command started while this command was still marked running.
    Superseded = 1,
    /// The session exited while this command was still marked running.
    SessionEnded = 2,
}

impl UnknownReason {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::NoStatus),
            1 => Ok(Self::Superseded),
            2 => Ok(Self::SessionEnded),
            other => Err(WireError::InvalidTag { kind: "UnknownReason", tag: u16::from(other) }),
        }
    }
}

/// How a shell command ended — tagged on the wire so `Claimed` can never masquerade as `Exited`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Command has not ended.
    Running,
    /// Command exited with a status vouched by the session's `MarksKey`.
    Exited(VouchedExitCode),
    /// Command ended in a keyed session without a vouched exit code.
    Unknown(UnknownReason),
    /// Command ended in an unkeyed session; code is an untrusted claim.
    Claimed(Option<i32>),
}

impl CommandOutcome {
    pub(crate) fn encode(self, w: &mut Writer) {
        match self {
            Self::Running => w.u8(0),
            Self::Exited(code) => {
                w.u8(1);
                w.i32(code.code());
            }
            Self::Unknown(why) => {
                w.u8(2);
                why.encode(w);
            }
            Self::Claimed(code) => {
                w.u8(3);
                w.opt(code.as_ref(), |w, c| w.i32(*c));
            }
        }
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Running),
            1 => Ok(Self::Exited(VouchedExitCode::from_vouched(r.i32()?))),
            2 => Ok(Self::Unknown(UnknownReason::decode(r)?)),
            3 => Ok(Self::Claimed(r.opt(|r| r.i32())?)),
            other => Err(WireError::InvalidTag { kind: "CommandOutcome", tag: u16::from(other) }),
        }
    }
}

/// Closed set of structured error codes returned by the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    /// Handshake failed (wrong order, version mismatch, duplicate `Hello`).
    Handshake = 1,
    /// Mutual HMAC authentication failed or was missing when required.
    Unauthorized = 2,
    /// Malformed request or invalid field value.
    Malformed = 3,
    /// Referenced session does not exist or has already exited.
    NoSuchSession = 4,
    /// Referenced stream is not attached on this connection.
    NoSuchStream = 5,
    /// Query referenced an older epoch after reflow or alt-screen transition.
    StaleEpoch = 6,
    /// OS PTY operation failed or input queue full.
    Pty = 7,
    /// Targeted PID does not belong to the session's process tree.
    OutsideSession = 8,
    /// Operation is unsupported on this platform (e.g., POSIX signals on Windows).
    Unsupported = 9,
    /// Broker or session resource budget was exceeded.
    ResourceLimit = 10,
    /// Resource is temporarily busy (e.g., recording still settling).
    Busy = 11,
}

impl ErrorCode {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u16(self as u16);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u16()? {
            1 => Ok(Self::Handshake),
            2 => Ok(Self::Unauthorized),
            3 => Ok(Self::Malformed),
            4 => Ok(Self::NoSuchSession),
            5 => Ok(Self::NoSuchStream),
            6 => Ok(Self::StaleEpoch),
            7 => Ok(Self::Pty),
            8 => Ok(Self::OutsideSession),
            9 => Ok(Self::Unsupported),
            10 => Ok(Self::ResourceLimit),
            11 => Ok(Self::Busy),
            other => Err(WireError::InvalidTag { kind: "ErrorCode", tag: other }),
        }
    }
}

/// Non-zero terminal grid dimensions (`cols >= 1`, `rows >= 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    /// Column count (`>= 1`).
    pub cols: NonZeroU16,
    /// Row count (`>= 1`).
    pub rows: NonZeroU16,
}

impl GridSize {
    /// Constructs a `GridSize`, returning `None` if either dimension is zero or exceeds protocol bounds.
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        if cols > MAX_GRID_COLS || rows > MAX_GRID_ROWS {
            return None;
        }
        Some(Self { cols: NonZeroU16::new(cols)?, rows: NonZeroU16::new(rows)? })
    }

    /// Encodes the grid dimensions into `w`.
    pub fn encode(self, w: &mut Writer) {
        w.u16(self.cols.get());
        w.u16(self.rows.get());
    }

    /// Decodes and validates non-zero bounded grid dimensions from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let cols = r.u16()?;
        let rows = r.u16()?;
        Self::new(cols, rows).ok_or(WireError::InvalidValue("grid dimensions out of range"))
    }
}

/// Cell size in physical pixels (`0` indicates unspecified/headless).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellPx {
    /// Cell width in pixels.
    pub width: u16,
    /// Cell height in pixels.
    pub height: u16,
}

impl CellPx {
    /// Encodes the pixel dimensions into `w`.
    pub fn encode(self, w: &mut Writer) {
        w.u16(self.width);
        w.u16(self.height);
    }

    /// Decodes pixel dimensions from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self { width: r.u16()?, height: r.u16()? })
    }
}

/// Subscription capability bitmask passed to `Attach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SubFlags(u8);

impl SubFlags {
    /// Receive differential view frames and issue view RPCs/input/viewport hints.
    pub const VIEW: u8 = 0x01;
    /// Receive control side-effect events (`Title`, `Bell`, `Clipboard`, `ProgramStatus`, `Exited`, ...).
    pub const EVENTS: u8 = 0x02;
    /// Receive raw coalesced UTF-8 output stream with per-stream byte credit.
    pub const RAW_OUTPUT: u8 = 0x04;
    /// When `RAW_OUTPUT` credit hits 0, pause PTY reads rather than shedding with `OutputDropped`.
    pub const BACKPRESSURE_PTY: u8 = 0x08;
    /// Mask of all defined bits.
    pub const ALL_MASK: u8 = 0x0F;

    /// Constructs `SubFlags`, rejecting unknown reserved bits and `BACKPRESSURE_PTY` without `RAW_OUTPUT`.
    pub fn from_bits(bits: u8) -> Option<Self> {
        if bits > Self::ALL_MASK {
            return None;
        }
        if bits & Self::BACKPRESSURE_PTY != 0 && bits & Self::RAW_OUTPUT == 0 {
            return None;
        }
        Some(Self(bits))
    }

    /// Returns raw flag bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns `true` if all bits in `flag` are set.
    pub const fn contains(self, flag: u8) -> bool {
        (self.0 & flag) == flag
    }

    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self.0);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Self::from_bits(r.u8()?).ok_or(WireError::InvalidValue("unknown SubFlags bits"))
    }
}

/// Validated non-zero lease reconnect grace duration in milliseconds (`1..=MAX_LEASE_GRACE_MS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseGraceMs(NonZeroU32);

impl LeaseGraceMs {
    /// Constructs a `LeaseGraceMs` iff `1 <= ms <= MAX_LEASE_GRACE_MS`.
    pub fn new(ms: u32) -> Option<Self> {
        if ms > MAX_LEASE_GRACE_MS {
            return None;
        }
        NonZeroU32::new(ms).map(Self)
    }

    /// Returns the grace duration in milliseconds.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Opaque 32-byte bearer token used to resume an in-grace remote broker lease.
///
/// Redacts itself in [`core::fmt::Debug`] so bearer secrets cannot leak into diagnostic logs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResumeToken([u8; 32]);

impl ResumeToken {
    /// Wraps a 32-byte reconnect token.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw 32-byte token.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for ResumeToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ResumeToken([REDACTED])")
    }
}

/// Lifetime ownership lease held by a connection — invalid combinations are unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseKind {
    /// Holds all non-kept broker sessions alive; tears down immediately on connection EOF (local default).
    BrokerImmediate,
    /// Holds all non-kept broker sessions alive with opt-in reconnect grace (e.g. remote SSH).
    BrokerGrace {
        /// Milliseconds to hold the lease after disconnect before teardown (`1..=MAX_LEASE_GRACE_MS`).
        grace_ms: LeaseGraceMs,
        /// 32-byte opaque reconnect bearer token.
        resume_token: ResumeToken,
    },
    /// Pins one specific session (e.g., `Rewind` or ephemeral tool) to this connection.
    Session(SessionId),
}

impl LeaseKind {
    pub(crate) fn encode(&self, w: &mut Writer) {
        match *self {
            Self::BrokerImmediate => w.u8(0),
            Self::BrokerGrace { grace_ms, resume_token } => {
                w.u8(1);
                w.var_u32(grace_ms.get());
                w.raw(resume_token.as_bytes());
            }
            Self::Session(id) => {
                w.u8(2);
                id.encode(w);
            }
        }
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::BrokerImmediate),
            1 => {
                let raw_ms = r.var_u32()?;
                let grace_ms = LeaseGraceMs::new(raw_ms).ok_or(WireError::LimitExceeded("lease grace_ms"))?;
                let resume_token = ResumeToken::new(r.fixed::<32>()?);
                Ok(Self::BrokerGrace { grace_ms, resume_token })
            }
            2 => Ok(Self::Session(SessionId::decode(r)?)),
            other => Err(WireError::InvalidTag { kind: "LeaseKind", tag: u16::from(other) }),
        }
    }
}

/// How the broker arbitrates PTY geometry when multiple views attach to one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ResizePolicy {
    /// The most recently focused visible view determines PTY size (default).
    #[default]
    LatestActive = 0,
    /// Minimum cols and rows across all visible attached views.
    MinFit = 1,
    /// Ignore view hints; only explicit control-plane `Resize` moves the PTY.
    ExplicitOverride = 2,
}

impl ResizePolicy {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::LatestActive),
            1 => Ok(Self::MinFit),
            2 => Ok(Self::ExplicitOverride),
            other => Err(WireError::InvalidTag { kind: "ResizePolicy", tag: u16::from(other) }),
        }
    }
}

/// Declarative viewport geometry reported by a `SUB_VIEW` stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportHint {
    /// Desired grid size.
    pub size: GridSize,
    /// Measured cell size in physical pixels.
    pub cell_px: CellPx,
    /// Whether the view is currently mounted and visible on screen.
    pub visible: bool,
    /// Whether the view currently has keyboard focus.
    pub focused: bool,
}

impl ViewportHint {
    pub(crate) fn encode(self, w: &mut Writer) {
        self.size.encode(w);
        self.cell_px.encode(w);
        w.bool(self.visible);
        w.bool(self.focused);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self { size: GridSize::decode(r)?, cell_px: CellPx::decode(r)?, visible: r.bool()?, focused: r.bool()? })
    }
}

/// Byte-bounded BDP frame pacing credit granted by a view in `FrameAck`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCredit {
    /// Maximum unacknowledged frames allowed in flight (`0` pauses delivery when hidden).
    pub max_frames: u16,
    /// Maximum unacknowledged frame payload bytes allowed in flight.
    pub max_bytes: u32,
}

impl FrameCredit {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u16(self.max_frames);
        w.var_u32(self.max_bytes);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self { max_frames: r.u16()?, max_bytes: r.var_u32()? })
    }
}

/// Cursor glyph shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CursorShape {
    /// Solid block cursor.
    Block = 0,
    /// Vertical bar cursor.
    Bar = 1,
    /// Horizontal underline cursor.
    Underline = 2,
    /// Hollow block outline.
    Hollow = 3,
}

impl CursorShape {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Block),
            1 => Ok(Self::Bar),
            2 => Ok(Self::Underline),
            3 => Ok(Self::Hollow),
            other => Err(WireError::InvalidTag { kind: "CursorShape", tag: u16::from(other) }),
        }
    }
}

/// Cursor position, style, blink, and visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    /// 0-based column inside the active viewport.
    pub col: u16,
    /// 0-based row inside the active viewport.
    pub row: u16,
    /// Cursor shape.
    pub shape: CursorShape,
    /// Whether the cursor blinks when focused.
    pub blinking: bool,
    /// Whether the program hid the cursor (DEC mode 25); position is still sent for IME placement.
    pub hidden: bool,
}

impl CursorState {
    /// Encodes the cursor state into `w`.
    pub fn encode(self, w: &mut Writer) {
        w.u16(self.col);
        w.u16(self.row);
        self.shape.encode(w);
        let mut flags = 0u8;
        if self.blinking {
            flags += 1;
        }
        if self.hidden {
            flags += 2;
        }
        w.u8(flags);
    }

    /// Decodes and validates a cursor state from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let col = r.u16()?;
        let row = r.u16()?;
        let shape = CursorShape::decode(r)?;
        let flags = r.u8()?;
        if flags > 0x03 {
            return Err(WireError::InvalidValue("unknown CursorState flag bits"));
        }
        Ok(Self { col, row, shape, blinking: flags & 1 != 0, hidden: flags & 2 != 0 })
    }
}

/// 24-bit RGB colour triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub [u8; 3]);

impl Rgb {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.raw(&self.0);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.fixed::<3>()?))
    }
}

/// Program-overridden dynamic colours (OSC 4/10/11/12).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaletteOverrides {
    /// Overridden default foreground (OSC 10).
    pub fg: Option<Rgb>,
    /// Overridden default background (OSC 11).
    pub bg: Option<Rgb>,
    /// Overridden cursor colour (OSC 12).
    pub cursor: Option<Rgb>,
    /// Overridden palette slots `(index, rgb)` (OSC 4).
    pub palette: Vec<(u8, Rgb)>,
}

impl PaletteOverrides {
    /// Encodes palette overrides into `w`.
    pub fn encode(&self, w: &mut Writer) {
        w.opt(self.fg.as_ref(), |w, c| c.encode(w));
        w.opt(self.bg.as_ref(), |w, c| c.encode(w));
        w.opt(self.cursor.as_ref(), |w, c| c.encode(w));
        w.slice(&self.palette, |w, (idx, rgb)| {
            w.u8(*idx);
            rgb.encode(w);
        });
    }

    /// Decodes palette overrides from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            fg: r.opt(Rgb::decode)?,
            bg: r.opt(Rgb::decode)?,
            cursor: r.opt(Rgb::decode)?,
            palette: r.vec_bounded(256, "palette overrides", |r| Ok((r.u8()?, Rgb::decode(r)?)))?,
        })
    }
}

/// View-relevant terminal modes packed in a frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewModes {
    /// Alternate screen buffer is active.
    pub alt_screen: bool,
    /// Program enabled mouse reporting.
    pub mouse: bool,
    /// Screen reverse video (DECSCNM).
    pub reverse_video: bool,
    /// Bracketed paste mode is active.
    pub bracketed_paste: bool,
}

impl ViewModes {
    /// Encodes view modes into `w`.
    pub fn encode(self, w: &mut Writer) {
        let mut bits = 0u8;
        if self.alt_screen {
            bits += 1;
        }
        if self.mouse {
            bits += 2;
        }
        if self.reverse_video {
            bits += 4;
        }
        if self.bracketed_paste {
            bits += 8;
        }
        w.u8(bits);
    }

    /// Decodes view modes from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let bits = r.u8()?;
        if bits > 0x0F {
            return Err(WireError::InvalidValue("unknown ViewModes flag bits"));
        }
        Ok(Self {
            alt_screen: bits & 1 != 0,
            mouse: bits & 2 != 0,
            reverse_video: bits & 4 != 0,
            bracketed_paste: bits & 8 != 0,
        })
    }
}

/// 32-byte BLAKE3 content hash identifying an immutable image blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobHash(pub [u8; 32]);

impl BlobHash {
    /// Encodes the 32-byte hash into `w`.
    pub fn encode(self, w: &mut Writer) {
        w.raw(&self.0);
    }

    /// Decodes a 32-byte hash from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self(r.fixed::<32>()?))
    }
}

/// Wire & disk compression format of an image blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ImageEncoding {
    /// Validated single-frame PNG bytes (decoded off-main-thread by browser `createImageBitmap`).
    Png = 0,
    /// Zstd-compressed 4-channel RGBA pixels.
    ZstdRgba = 1,
}

impl ImageEncoding {
    /// Encodes the encoding tag into `w`.
    pub fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    /// Decodes an image encoding tag from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Png),
            1 => Ok(Self::ZstdRgba),
            other => Err(WireError::InvalidTag { kind: "ImageEncoding", tag: u16::from(other) }),
        }
    }
}

/// Image slice rendered inside a row via Kitty Unicode placeholder cells (`U+10EEEE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePiece {
    /// Content-addressed image blob hash.
    pub blob: BlobHash,
    /// Starting cell column on the row.
    pub col_start: u16,
    /// Number of cells spanned on the row.
    pub col_len: u16,
    /// Source crop rectangle in pixels `[x, y, w, h]`.
    pub src_rect: [u32; 4],
    /// Destination size in pixels `[w, h]`.
    pub dst_px: [u32; 2],
}

impl ImagePiece {
    pub(crate) fn encode(&self, w: &mut Writer) {
        self.blob.encode(w);
        w.u16(self.col_start);
        w.u16(self.col_len);
        for v in self.src_rect {
            w.var_u32(v);
        }
        for v in self.dst_px {
            w.var_u32(v);
        }
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let blob = BlobHash::decode(r)?;
        let col_start = r.u16()?;
        let col_len = r.u16()?;
        if col_len == 0 || col_start.checked_add(col_len).is_none() {
            return Err(WireError::InvalidValue("invalid ImagePiece column span"));
        }
        let src_rect = [r.var_u32()?, r.var_u32()?, r.var_u32()?, r.var_u32()?];
        if src_rect[2] == 0
            || src_rect[3] == 0
            || src_rect[0].checked_add(src_rect[2]).is_none()
            || src_rect[1].checked_add(src_rect[3]).is_none()
        {
            return Err(WireError::InvalidValue("invalid ImagePiece src_rect"));
        }
        let dst_px = [r.var_u32()?, r.var_u32()?];
        if dst_px[0] == 0 || dst_px[1] == 0 {
            return Err(WireError::InvalidValue("invalid ImagePiece dst_px"));
        }
        Ok(Self { blob, col_start, col_len, src_rect, dst_px })
    }
}

/// One packed style-run descriptor inside a [`PackedRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedRun {
    /// Starting grid column.
    pub col_start: u16,
    /// Columns occupied by this run.
    pub col_len: u16,
    /// Byte offset into `PackedRow.text`.
    pub byte_offset: u32,
    /// Byte length inside `PackedRow.text`.
    pub byte_len: u32,
    /// Index into `PackedRow.styles` (`0` = plain default style).
    pub style_idx: u16,
    /// Index into `PackedRow.links` (`0` = no hyperlink).
    pub link_idx: u16,
    /// Run flags (bit 0: wide/grapheme cluster needing per-cell placement).
    pub flags: u8,
}

impl PackedRun {
    pub(crate) fn encode(&self, w: &mut Writer) {
        w.u16(self.col_start);
        w.u16(self.col_len);
        w.var_u32(self.byte_offset);
        w.var_u32(self.byte_len);
        w.u16(self.style_idx);
        w.u16(self.link_idx);
        w.u8(self.flags);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let col_start = r.u16()?;
        let col_len = r.u16()?;
        if col_len == 0 || col_start.checked_add(col_len).is_none() {
            return Err(WireError::InvalidValue("invalid PackedRun column span"));
        }
        let byte_offset = r.var_u32()?;
        let byte_len = r.var_u32()?;
        let style_idx = r.u16()?;
        let link_idx = r.u16()?;
        let flags = r.u8()?;
        if flags > 0x01 {
            return Err(WireError::InvalidValue("unknown PackedRun flag bits"));
        }
        Ok(Self { col_start, col_len, byte_offset, byte_len, style_idx, link_idx, flags })
    }
}

/// Compact 16-byte packed cell style (`fg`, `bg`, `underline_color`, `flags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PackedStyle {
    /// Packed foreground colour (`0` = default, `1..=256` = palette+1, `0x01_RR_GG_BB` = RGB).
    pub fg: u32,
    /// Packed background colour.
    pub bg: u32,
    /// Packed underline colour.
    pub underline_color: u32,
    /// Style attribute bitmask (bold, italic, underline, strikethrough, blink, inverse, invisible, faint).
    pub flags: u32,
}

impl PackedStyle {
    /// Bold attribute bit.
    pub const FLAG_BOLD: u32 = 0x0001;
    /// Italic attribute bit.
    pub const FLAG_ITALIC: u32 = 0x0002;
    /// 3-bit underline style field (bits 2..=4, values `0..=5`: none, single, double, curly, dotted, dashed).
    pub const FLAG_UNDERLINE_MASK: u32 = 0x001C;
    /// Maximum valid shifted underline value (`5 << 2 = 0x14`).
    pub const FLAG_UNDERLINE_MAX: u32 = 0x0014;
    /// Strikethrough attribute bit.
    pub const FLAG_STRIKETHROUGH: u32 = 0x0020;
    /// Blink attribute bit.
    pub const FLAG_BLINK: u32 = 0x0040;
    /// Inverse video attribute bit.
    pub const FLAG_INVERSE: u32 = 0x0080;
    /// Invisible text attribute bit.
    pub const FLAG_INVISIBLE: u32 = 0x0100;
    /// Faint/dim attribute bit.
    pub const FLAG_FAINT: u32 = 0x0200;
    /// Mask of all defined style attribute bits.
    pub const ALL_FLAGS_MASK: u32 = 0x03FF;

    /// Returns `true` iff `color` is a valid packed colour (`0` default, `1..=256` palette+1,
    /// or `0x01_00_00_00..=0x01_FF_FF_FF` RGB).
    pub const fn is_valid_color(color: u32) -> bool {
        color <= 256 || (color >= 0x01_00_00_00 && color <= 0x01_FF_FF_FF)
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        w.u32(self.fg);
        w.u32(self.bg);
        w.u32(self.underline_color);
        w.u32(self.flags);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let fg = r.u32()?;
        let bg = r.u32()?;
        let underline_color = r.u32()?;
        let flags = r.u32()?;
        if !Self::is_valid_color(fg) || !Self::is_valid_color(bg) || !Self::is_valid_color(underline_color) {
            return Err(WireError::InvalidValue("invalid PackedStyle colour encoding"));
        }
        if flags & !Self::ALL_FLAGS_MASK != 0 || (flags & Self::FLAG_UNDERLINE_MASK) > Self::FLAG_UNDERLINE_MAX {
            return Err(WireError::InvalidValue("invalid PackedStyle flags"));
        }
        Ok(Self { fg, bg, underline_color, flags })
    }
}

/// Self-contained, style-run encoded terminal row (1 UTF-8 string + flat runs/styles/links).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedRow {
    /// Absolute epoch-scoped line number.
    pub line: u64,
    /// Row flags (bit 0: wrapped, bit 1: prompt, bit 2: has_blink, bit 3: has_placeholder).
    pub flags: u8,
    /// Single contiguous UTF-8 text buffer referenced by `runs`.
    pub text: String,
    /// Style-run descriptors covering non-blank cells on the row.
    pub runs: Vec<PackedRun>,
    /// Row-local style table (`styles[0]` is plain default).
    pub styles: Vec<PackedStyle>,
    /// Row-local hyperlink URI table (`links[0]` is `""` / none).
    pub links: Vec<String>,
    /// Optional Kitty placeholder image pieces on this row.
    pub pieces: Vec<ImagePiece>,
}

impl PackedRow {
    /// Row wraps into the following line.
    pub const FLAG_WRAPPED: u8 = 0x01;
    /// Row begins a shell prompt (OSC 133/633).
    pub const FLAG_PROMPT: u8 = 0x02;
    /// Row contains at least one blinking cell.
    pub const FLAG_HAS_BLINK: u8 = 0x04;
    /// Row contains Kitty Unicode placeholder cells.
    pub const FLAG_HAS_PLACEHOLDER: u8 = 0x08;

    /// Safely resolves `style_idx`, returning [`PackedStyle::default`] for index `0` even when `styles` is omitted.
    pub fn style(&self, idx: u16) -> PackedStyle {
        self.styles.get(usize::from(idx)).copied().unwrap_or_default()
    }

    /// Safely resolves `link_idx`, returning `""` for index `0` even when `links` is omitted.
    pub fn link(&self, idx: u16) -> &str {
        self.links.get(usize::from(idx)).map(String::as_str).unwrap_or("")
    }

    /// Encodes the packed row into `w`.
    pub fn encode(&self, w: &mut Writer) {
        w.var_u64(self.line);
        w.u8(self.flags);
        w.str(&self.text);
        w.slice(&self.runs, |w, r| r.encode(w));
        w.slice(&self.styles, |w, s| s.encode(w));
        w.slice(&self.links, |w, l| w.str(l));
        w.slice(&self.pieces, |w, p| p.encode(w));
    }

    /// Decodes and validates a packed row from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let line = r.var_u64()?;
        let flags = r.u8()?;
        if flags > 0x0F {
            return Err(WireError::InvalidValue("unknown PackedRow flag bits"));
        }
        let text = r.str()?;
        let runs = r.vec_bounded(4096, "packed runs", PackedRun::decode)?;
        let styles = r.vec_bounded(4096, "packed styles", PackedStyle::decode)?;
        if styles.first().is_some_and(|s| *s != PackedStyle::default()) {
            return Err(WireError::InvalidValue("styles[0] must be default style"));
        }
        let links = r.vec_bounded(1024, "packed links", Reader::str)?;
        if links.first().is_some_and(|l| !l.is_empty()) {
            return Err(WireError::InvalidValue("links[0] must be empty"));
        }
        let pieces = r.vec_bounded(1024, "image pieces", ImagePiece::decode)?;
        if !pieces.is_empty() && (flags & Self::FLAG_HAS_PLACEHOLDER == 0) {
            return Err(WireError::InvalidValue("pieces require FLAG_HAS_PLACEHOLDER"));
        }
        // Validate every run's byte range and table indices up front (parse, don't validate later!).
        for run in &runs {
            let start = run.byte_offset as usize;
            let len = run.byte_len as usize;
            let end = start.checked_add(len).ok_or(WireError::InvalidValue("run range overflow"))?;
            if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                return Err(WireError::InvalidValue("run byte range out of bounds or mid-UTF-8"));
            }
            if usize::from(run.style_idx) >= styles.len() && run.style_idx != 0 {
                return Err(WireError::InvalidValue("run style_idx out of bounds"));
            }
            if usize::from(run.link_idx) >= links.len() && run.link_idx != 0 {
                return Err(WireError::InvalidValue("run link_idx out of bounds"));
            }
        }
        Ok(Self { line, flags, text, runs, styles, links, pieces })
    }
}

/// Inclusive cell coordinate on an epoch-numbered line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPos {
    /// Absolute line number within the current epoch.
    pub line: u64,
    /// 0-based column.
    pub col: u16,
}

impl CellPos {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.var_u64(self.line);
        w.u16(self.col);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self { line: r.var_u64()?, col: r.u16()? })
    }
}

/// Inclusive cell range `[start, end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSpan {
    /// First cell in the span.
    pub start: CellPos,
    /// Last cell in the span (inclusive).
    pub end: CellPos,
}

impl CellSpan {
    pub(crate) fn encode(self, w: &mut Writer) {
        self.start.encode(w);
        self.end.encode(w);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let start = CellPos::decode(r)?;
        let end = CellPos::decode(r)?;
        if end < start {
            return Err(WireError::InvalidValue("span end before start"));
        }
        Ok(Self { start, end })
    }
}

/// Absolute image placement in a session (anchored to an epoch line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImagePlacement {
    /// Content-addressed image blob hash.
    pub blob: BlobHash,
    /// Epoch-numbered top line of the placement.
    pub line: u64,
    /// Starting column (signed for clipped relative placements).
    pub col: i32,
    /// Natural image width & height in pixels.
    pub pixel_size: [u32; 2],
    /// Draw width & height in cells.
    pub cell_span: [u16; 2],
    /// Z-index (`< 0` behind text, `>= 0` above text).
    pub z_index: i32,
}

impl ImagePlacement {
    /// Encodes the image placement into `w`.
    pub fn encode(&self, w: &mut Writer) {
        self.blob.encode(w);
        w.var_u64(self.line);
        w.i32(self.col);
        w.var_u32(self.pixel_size[0]);
        w.var_u32(self.pixel_size[1]);
        w.u16(self.cell_span[0]);
        w.u16(self.cell_span[1]);
        w.i32(self.z_index);
    }

    /// Decodes an image placement from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            blob: BlobHash::decode(r)?,
            line: r.var_u64()?,
            col: r.i32()?,
            pixel_size: [r.var_u32()?, r.var_u32()?],
            cell_span: [r.u16()?, r.u16()?],
            z_index: r.i32()?,
        })
    }
}

/// Unified shell command record shared identically by control and view subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    /// Monotonic 64-bit command ID within the session.
    pub id: u64,
    /// Claimed or vouched command line text.
    pub command: Option<String>,
    /// Claimed working directory.
    pub cwd: Option<String>,
    /// Start timestamp (ms since Unix epoch), if started.
    pub started_ms: Option<u64>,
    /// Finish timestamp (ms since Unix epoch), if finished.
    pub finished_ms: Option<u64>,
    /// How the command ended (or `Running`).
    pub outcome: CommandOutcome,
    /// Optional line positions in the current view epoch: `(prompt_line, output_line, end_line)`.
    pub prompt_line: Option<u64>,
    /// Optional output start line in the current view epoch.
    pub output_line: Option<u64>,
    /// Optional command end line in the current view epoch.
    pub end_line: Option<u64>,
}

impl CommandRecord {
    /// Encodes the command record into `w`.
    pub fn encode(&self, w: &mut Writer) {
        w.var_u64(self.id);
        w.opt(self.command.as_ref(), |w, s| w.str(s));
        w.opt(self.cwd.as_ref(), |w, s| w.str(s));
        w.opt(self.started_ms.as_ref(), |w, t| w.var_u64(*t));
        w.opt(self.finished_ms.as_ref(), |w, t| w.var_u64(*t));
        self.outcome.encode(w);
        w.opt(self.prompt_line.as_ref(), |w, l| w.var_u64(*l));
        w.opt(self.output_line.as_ref(), |w, l| w.var_u64(*l));
        w.opt(self.end_line.as_ref(), |w, l| w.var_u64(*l));
    }

    /// Decodes a command record from `r`.
    pub fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            id: r.var_u64()?,
            command: r.opt(Reader::str)?,
            cwd: r.opt(Reader::str)?,
            started_ms: r.opt(Reader::var_u64)?,
            finished_ms: r.opt(Reader::var_u64)?,
            outcome: CommandOutcome::decode(r)?,
            prompt_line: r.opt(Reader::var_u64)?,
            output_line: r.opt(Reader::var_u64)?,
            end_line: r.opt(Reader::var_u64)?,
        })
    }
}

/// OSC 7501 program status state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StatusState {
    /// Program is idle.
    Idle = 0,
    /// Program is actively working.
    Working = 1,
    /// Program is blocked awaiting user input/approval.
    Blocked = 2,
    /// Program completed its work.
    Done = 3,
    /// Program encountered an error.
    Error = 4,
}

impl StatusState {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Idle),
            1 => Ok(Self::Working),
            2 => Ok(Self::Blocked),
            3 => Ok(Self::Done),
            4 => Ok(Self::Error),
            other => Err(WireError::InvalidTag { kind: "StatusState", tag: u16::from(other) }),
        }
    }
}

/// Sub-kind for [`StatusState::Blocked`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockedKind {
    /// Permission prompt.
    Permission = 0,
    /// User question.
    Question = 1,
    /// Authentication prompt.
    Auth = 2,
}

impl BlockedKind {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Permission),
            1 => Ok(Self::Question),
            2 => Ok(Self::Auth),
            other => Err(WireError::InvalidTag { kind: "BlockedKind", tag: u16::from(other) }),
        }
    }
}

/// Whether an OSC 7501 status claim matched the OS foreground process group leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Confirmed {
    /// Foreground process matched the claimed `app`.
    Matched = 0,
    /// Foreground process was a different program.
    Mismatched = 1,
    /// Could not verify (e.g., Windows ConPTY or unnamed app).
    Unknown = 2,
}

impl Confirmed {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Matched),
            1 => Ok(Self::Mismatched),
            2 => Ok(Self::Unknown),
            other => Err(WireError::InvalidTag { kind: "Confirmed", tag: u16::from(other) }),
        }
    }
}

/// One OSC 7501 program-status record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRecord {
    /// Record hierarchy ID (`""` for the root program).
    pub id: String,
    /// Current state.
    pub state: StatusState,
    /// Optional blocked sub-kind.
    pub kind: Option<BlockedKind>,
    /// Optional progress percentage (`0..=100`).
    pub progress: Option<u8>,
    /// Claimed application name.
    pub app: Option<String>,
    /// Short title text.
    pub title: Option<String>,
    /// One-line status message.
    pub message: Option<String>,
    /// Monotonic revision counter.
    pub changed: u64,
    /// Timestamp when state was entered (ms since Unix epoch).
    pub since_ms: u64,
    /// Result of foreground-process verification.
    pub confirmed: Confirmed,
}

impl StatusRecord {
    pub(crate) fn encode(&self, w: &mut Writer) {
        w.str(&self.id);
        self.state.encode(w);
        w.opt(self.kind.as_ref(), |w, k| k.encode(w));
        w.opt(self.progress.as_ref(), |w, p| w.u8(*p));
        w.opt(self.app.as_ref(), |w, s| w.str(s));
        w.opt(self.title.as_ref(), |w, s| w.str(s));
        w.opt(self.message.as_ref(), |w, s| w.str(s));
        w.var_u64(self.changed);
        w.var_u64(self.since_ms);
        self.confirmed.encode(w);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let id = r.str()?;
        let state = StatusState::decode(r)?;
        let kind = r.opt(BlockedKind::decode)?;
        if state != StatusState::Blocked && kind.is_some() {
            return Err(WireError::InvalidValue("status kind requires Blocked state"));
        }
        let progress = r.opt(|r| {
            let p = r.u8()?;
            if p > 100 { Err(WireError::InvalidValue("progress must be 0..=100")) } else { Ok(p) }
        })?;
        Ok(Self {
            id,
            state,
            kind,
            progress,
            app: r.opt(Reader::str)?,
            title: r.opt(Reader::str)?,
            message: r.opt(Reader::str)?,
            changed: r.var_u64()?,
            since_ms: r.var_u64()?,
            confirmed: Confirmed::decode(r)?,
        })
    }
}

/// Progress bar state (OSC 9;4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProgressState {
    /// Cleared / hidden.
    None = 0,
    /// Normal running progress.
    Running = 1,
    /// Error progress state.
    Error = 2,
    /// Indeterminate spinner.
    Indeterminate = 3,
    /// Paused progress state.
    Paused = 4,
}

impl ProgressState {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::None),
            1 => Ok(Self::Running),
            2 => Ok(Self::Error),
            3 => Ok(Self::Indeterminate),
            4 => Ok(Self::Paused),
            other => Err(WireError::InvalidTag { kind: "ProgressState", tag: u16::from(other) }),
        }
    }
}

/// Key action for structured keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyAction {
    /// Initial key press.
    Press = 0,
    /// Auto-repeat while held.
    Repeat = 1,
    /// Key release.
    Release = 2,
}

impl KeyAction {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Press),
            1 => Ok(Self::Repeat),
            2 => Ok(Self::Release),
            other => Err(WireError::InvalidTag { kind: "KeyAction", tag: u16::from(other) }),
        }
    }
}

/// Structured keyboard input sent by a view for broker-side VT encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    /// Physical W3C `KeyboardEvent.code` name (e.g., `"KeyA"`, `"ArrowUp"`).
    pub code: String,
    /// Press / Repeat / Release.
    pub action: KeyAction,
    /// Modifier bitmask (`shift=1, ctrl=2, alt=4, super=8, caps=16, num=32`).
    pub modifiers: u8,
    /// Text typed by the key, if any.
    pub text: String,
    /// Unshifted Unicode codepoint for layout-independent shortcuts (`0` if unknown).
    pub unshifted_codepoint: u32,
    /// Whether Alt/Option composed `text` rather than modifying the key.
    pub composed: bool,
}

impl KeyInput {
    pub(crate) fn encode(&self, w: &mut Writer) {
        w.str(&self.code);
        self.action.encode(w);
        w.u8(self.modifiers);
        w.str(&self.text);
        w.var_u32(self.unshifted_codepoint);
        w.bool(self.composed);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let code = r.str()?;
        let action = KeyAction::decode(r)?;
        let modifiers = r.u8()?;
        if modifiers > 0x3F {
            return Err(WireError::InvalidValue("unknown KeyInput modifier bits"));
        }
        let text = r.str()?;
        let unshifted_codepoint = r.var_u32()?;
        if unshifted_codepoint != 0 && char::from_u32(unshifted_codepoint).is_none() {
            return Err(WireError::InvalidValue("invalid Unicode scalar codepoint"));
        }
        let composed = r.bool()?;
        Ok(Self { code, action, modifiers, text, unshifted_codepoint, composed })
    }
}

/// Pointer action for structured mouse reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PointerAction {
    /// Mouse button pressed.
    Press = 0,
    /// Mouse button released.
    Release = 1,
    /// Pointer moved.
    Motion = 2,
    /// Wheel scrolled up.
    WheelUp = 3,
    /// Wheel scrolled down.
    WheelDown = 4,
    /// Wheel scrolled left.
    WheelLeft = 5,
    /// Wheel scrolled right.
    WheelRight = 6,
}

impl PointerAction {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Press),
            1 => Ok(Self::Release),
            2 => Ok(Self::Motion),
            3 => Ok(Self::WheelUp),
            4 => Ok(Self::WheelDown),
            5 => Ok(Self::WheelLeft),
            6 => Ok(Self::WheelRight),
            other => Err(WireError::InvalidTag { kind: "PointerAction", tag: u16::from(other) }),
        }
    }
}

/// Structured pointer event sent by a view for broker-side mouse encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerInput {
    /// Event kind.
    pub action: PointerAction,
    /// Button index (`0` = left, `1` = middle, `2` = right, `255` = none).
    pub button: u8,
    /// Modifier bitmask.
    pub modifiers: u8,
    /// Grid-relative X coordinate in pixels (clamped non-negative integer).
    pub x_px: u32,
    /// Grid-relative Y coordinate in pixels.
    pub y_px: u32,
    /// Cell dimensions in pixels.
    pub cell_px: CellPx,
}

impl PointerInput {
    pub(crate) fn encode(self, w: &mut Writer) {
        self.action.encode(w);
        w.u8(self.button);
        w.u8(self.modifiers);
        w.var_u32(self.x_px);
        w.var_u32(self.y_px);
        self.cell_px.encode(w);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        let action = PointerAction::decode(r)?;
        let button = r.u8()?;
        if button > 2 && button != 255 {
            return Err(WireError::InvalidValue("invalid PointerInput button"));
        }
        let modifiers = r.u8()?;
        if modifiers > 0x3F {
            return Err(WireError::InvalidValue("unknown PointerInput modifier bits"));
        }
        Ok(Self { action, button, modifiers, x_px: r.var_u32()?, y_px: r.var_u32()?, cell_px: CellPx::decode(r)? })
    }
}

/// Outcome of a structured `Paste` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PasteOutcome {
    /// Paste was queued to the PTY.
    Pasted = 0,
    /// Paste was held because it contained a command-executing sequence; resend with `allow_unsafe = true`.
    Unsafe = 1,
}

impl PasteOutcome {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Pasted),
            1 => Ok(Self::Unsafe),
            other => Err(WireError::InvalidTag { kind: "PasteOutcome", tag: u16::from(other) }),
        }
    }
}

/// Condition evaluated reactively by `Request::Wait`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCondition {
    /// Wait until shell reports idle (keyed finish in keyed sessions; prompt/finish in unkeyed).
    Idle,
    /// Wait until substring appears on screen or in scrollback.
    Text(String),
    /// Wait until the session's root OSC 7501 status enters any of the listed states.
    Status(Vec<StatusState>),
    /// Wait until the session process exits.
    Exit,
}

impl WaitCondition {
    pub(crate) fn encode(&self, w: &mut Writer) {
        match self {
            Self::Idle => w.u8(0),
            Self::Text(needle) => {
                w.u8(1);
                w.str(needle);
            }
            Self::Status(states) => {
                w.u8(2);
                w.slice(states, |w, s| s.encode(w));
            }
            Self::Exit => w.u8(3),
        }
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Idle),
            1 => {
                let needle = r.str_bounded(crate::MAX_SEARCH_NEEDLE_BYTES, "wait text needle")?;
                if needle.is_empty() {
                    return Err(WireError::InvalidValue("wait text needle must be non-empty"));
                }
                Ok(Self::Text(needle))
            }
            2 => {
                let states = r.vec_bounded(8, "wait states", StatusState::decode)?;
                if states.is_empty() {
                    return Err(WireError::InvalidValue("wait status list must be non-empty"));
                }
                Ok(Self::Status(states))
            }
            3 => Ok(Self::Exit),
            other => Err(WireError::InvalidTag { kind: "WaitCondition", tag: u16::from(other) }),
        }
    }
}

/// Result of a `Wait` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WaitOutcome {
    /// Condition was satisfied.
    Met = 0,
    /// Caller deadline expired first.
    Timeout = 1,
    /// Session exited before the condition was met.
    Exited = 2,
}

impl WaitOutcome {
    pub(crate) fn encode(self, w: &mut Writer) {
        w.u8(self as u8);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => Ok(Self::Met),
            1 => Ok(Self::Timeout),
            2 => Ok(Self::Exited),
            other => Err(WireError::InvalidTag { kind: "WaitOutcome", tag: u16::from(other) }),
        }
    }
}

/// Foreground process summary (POSIX only; `None` on Windows ConPTY).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    /// Process group leader PID.
    pub pid: SignalTarget,
    /// Best-effort process name reported by the OS.
    pub name: Option<String>,
}

impl ForegroundProcess {
    pub(crate) fn encode(&self, w: &mut Writer) {
        self.pid.encode(w);
        w.opt(self.name.as_ref(), |w, n| w.str(n));
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self { pid: SignalTarget::decode(r)?, name: r.opt(Reader::str)? })
    }
}

/// Built-in broker telemetry snapshot returned by `Metrics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Number of live sessions.
    pub live_sessions: u32,
    /// Number of open rewound recording sessions.
    pub rewound_sessions: u32,
    /// Active connections.
    pub connections: u32,
    /// Active scheduled timers in the unified wheel.
    pub active_timers: u32,
    /// Aggregate scrollback bytes held across all sessions.
    pub scrollback_bytes: u64,
    /// Aggregate bytes held in the global deduplicated image store.
    pub image_store_bytes: u64,
    /// Cumulative PTY bytes parsed since broker startup.
    pub total_pty_bytes: u64,
    /// Cumulative microseconds spent throttled on output credit.
    pub total_throttled_us: u64,
}

impl MetricsSnapshot {
    pub(crate) fn encode(&self, w: &mut Writer) {
        w.var_u32(self.live_sessions);
        w.var_u32(self.rewound_sessions);
        w.var_u32(self.connections);
        w.var_u32(self.active_timers);
        w.var_u64(self.scrollback_bytes);
        w.var_u64(self.image_store_bytes);
        w.var_u64(self.total_pty_bytes);
        w.var_u64(self.total_throttled_us);
    }

    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            live_sessions: r.var_u32()?,
            rewound_sessions: r.var_u32()?,
            connections: r.var_u32()?,
            active_timers: r.var_u32()?,
            scrollback_bytes: r.var_u64()?,
            image_store_bytes: r.var_u64()?,
            total_pty_bytes: r.var_u64()?,
            total_throttled_us: r.var_u64()?,
        })
    }
}
