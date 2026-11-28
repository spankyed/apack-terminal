//! Synchronous v2 binary protocol client used by `inspect` and real-OS integration tests.

use std::fmt;
use std::io::{self, Read, Write};

use apack_core::auth::{AuthConfig, compute_client_proof, verify_server_proof};
use apack_os::transport::{connect_local_stream, verify_peer_is_current_user};
use apack_proto::codec::WireError;
use apack_proto::frame::{DecodedFrame, FrameDecoder};
use apack_proto::messages::{BrokerMsg, ClientMsg, SessionSummary, SpawnSpec};
use apack_proto::types::{
    ErrorCode, FrameCredit, LeaseKind, MetricsSnapshot, NonZeroStreamId, RequestId, SessionId, SignalKind, SignalScope,
    SignalTarget, StreamId, SubFlags, ViewportHint, WaitCondition, WaitOutcome,
};
use apack_proto::{FIN_FRAME, PROTOCOL_VERSION};
use interprocess::local_socket::Stream;

use crate::fill_random;

/// Error returned by [`BrokerClient`].
#[derive(Debug)]
pub enum ClientError {
    /// Underlying transport I/O error.
    Io(io::Error),
    /// Wire framing or message decoding error.
    Wire(WireError),
    /// Peer verification failed (server not owned by current user).
    UntrustedServer,
    /// Mutual HMAC handshake failed or server sent an unexpected handshake message.
    AuthFailed(String),
    /// Broker returned a structured `BrokerMsg::Error`.
    Broker {
        /// Structured error code.
        code: ErrorCode,
        /// Human-readable message.
        message: String,
    },
    /// Broker sent an unexpected reply message type for a request.
    UnexpectedReply(&'static str),
    /// Connection closed before a reply arrived.
    Closed,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "client I/O error: {err}"),
            Self::Wire(err) => write!(f, "client wire error: {err}"),
            Self::UntrustedServer => write!(f, "broker endpoint is not owned by the current user"),
            Self::AuthFailed(msg) => write!(f, "authentication failed: {msg}"),
            Self::Broker { code, message } => write!(f, "broker error {code:?}: {message}"),
            Self::UnexpectedReply(what) => write!(f, "unexpected broker reply: {what}"),
            Self::Closed => write!(f, "broker connection closed"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<WireError> for ClientError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}

/// One decoded frame received from the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFrame {
    /// Wire stream ID (`0` for control, `>= 1` for attached session stream).
    pub stream_id: StreamId,
    /// Correlation request ID (`0` for push events).
    pub req_id: RequestId,
    /// Decoded broker message.
    pub msg: BrokerMsg,
}

/// Hard cap on buffered unsolicited push frames inside [`BrokerClient::pending`].
const MAX_PENDING_FRAMES: usize = 4096;

/// Blocking v2 client wrapping any bidirectional transport (local socket/pipe or stdio proxy).
pub struct BrokerClient<T> {
    transport: T,
    decoder: FrameDecoder,
    rx_buf: Vec<u8>,
    next_req_id: u32,
    pending: std::collections::VecDeque<ReceivedFrame>,
    word_delimiters: String,
    capabilities: u32,
}

impl BrokerClient<Stream> {
    /// Connects to a local broker socket/pipe, verifies server ownership, and completes the
    /// v2 `Hello` + optional mutual HMAC-SHA256 handshake.
    pub fn connect(socket_path: &str, auth: &AuthConfig, client_name: &str) -> Result<Self, ClientError> {
        let stream = connect_local_stream(socket_path)?;
        if !verify_peer_is_current_user(&stream)? {
            return Err(ClientError::UntrustedServer);
        }
        Self::handshake(stream, auth, client_name)
    }
}

impl<T: Read + Write> BrokerClient<T> {
    /// Completes the v2 `Hello` + optional mutual HMAC-SHA256 handshake over `transport`.
    pub fn handshake(transport: T, auth: &AuthConfig, client_name: &str) -> Result<Self, ClientError> {
        let mut client = Self {
            transport,
            decoder: FrameDecoder::new(),
            rx_buf: Vec::new(),
            next_req_id: 1,
            pending: std::collections::VecDeque::new(),
            word_delimiters: String::new(),
            capabilities: 0,
        };

        let mut client_nonce = [0u8; 32];
        fill_random(&mut client_nonce).map_err(ClientError::AuthFailed)?;

        let hello_req = client.alloc_req_id();
        client.send_raw(
            StreamId::CONTROL,
            hello_req,
            &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION, client_nonce, client_name: client_name.to_owned() },
        )?;

        let (auth_required, server_nonce, word_delimiters, capabilities) = match client.wait_reply_ok(hello_req)? {
            BrokerMsg::HelloOk { protocol_version, auth_required, server_nonce, word_delimiters, capabilities } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(ClientError::AuthFailed(format!("protocol version mismatch: {protocol_version}")));
                }
                (auth_required, server_nonce, word_delimiters, capabilities)
            }
            _ => return Err(ClientError::UnexpectedReply("expected HelloOk")),
        };
        client.word_delimiters = word_delimiters;
        client.capabilities = capabilities;

        if auth_required {
            let AuthConfig::Required(key) = auth else {
                return Err(ClientError::AuthFailed(String::from("broker requires auth but no key configured")));
            };
            let client_proof = compute_client_proof(key, &client_nonce, &server_nonce);
            let auth_req = client.alloc_req_id();
            client.send_raw(StreamId::CONTROL, auth_req, &ClientMsg::AuthProof { hmac: client_proof })?;
            match client.wait_reply_ok(auth_req)? {
                BrokerMsg::AuthOk { server_hmac } => {
                    if !verify_server_proof(key, &server_nonce, &client_nonce, &server_hmac) {
                        return Err(ClientError::AuthFailed(String::from("server HMAC verification failed")));
                    }
                }
                _ => return Err(ClientError::UnexpectedReply("expected AuthOk")),
            }
        } else if auth.is_required() {
            return Err(ClientError::AuthFailed(String::from(
                "client requires mutual authentication but server advertised auth_required=false",
            )));
        }

        Ok(client)
    }

    /// Returns the word delimiters advertised by the broker in `HelloOk`.
    pub fn word_delimiters(&self) -> &str {
        &self.word_delimiters
    }

    /// Returns the capability bitmask advertised by the broker in `HelloOk`.
    pub fn capabilities(&self) -> u32 {
        self.capabilities
    }

    /// Sends a clean `[0, 0, 0, 0]` `FIN` frame to end the connection gracefully.
    pub fn send_fin(&mut self) -> Result<(), ClientError> {
        self.transport.write_all(&FIN_FRAME)?;
        self.transport.flush()?;
        Ok(())
    }

    /// Sends an arbitrary framed [`ClientMsg`] on `(stream_id, req_id)`.
    pub fn send_raw(&mut self, stream_id: StreamId, req_id: RequestId, msg: &ClientMsg) -> Result<(), ClientError> {
        let mut buf = Vec::new();
        msg.encode_to_frame(stream_id, req_id, &mut buf)?;
        self.transport.write_all(&buf)?;
        self.transport.flush()?;
        Ok(())
    }

    /// Allocates the next non-zero request ID.
    pub fn alloc_req_id(&mut self) -> RequestId {
        let id = self.next_req_id;
        self.next_req_id = self.next_req_id.checked_add(1).unwrap_or(1);
        RequestId::from_raw(id)
    }

    /// Receives the next broker frame (either buffered push or read from the transport).
    pub fn recv_frame(&mut self) -> Result<ReceivedFrame, ClientError> {
        if let Some(frame) = self.pending.pop_front() {
            return Ok(frame);
        }
        self.read_one_from_wire()
    }

    /// Issues `AcquireLease` and returns `(lease_id, resumed)`.
    pub fn acquire_lease(&mut self, kind: LeaseKind) -> Result<(u64, bool), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::AcquireLease { kind })?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::LeaseAck { lease_id, resumed } => Ok((lease_id, resumed)),
            _ => Err(ClientError::UnexpectedReply("expected LeaseAck")),
        }
    }

    /// Issues `ReleaseLease`.
    pub fn release_lease(&mut self, lease_id: u64) -> Result<(), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::ReleaseLease { lease_id })?;
        expect_ok(self.wait_reply_ok(req)?)
    }

    /// Issues `Spawn` and returns `(session_id, pid)`.
    pub fn spawn(&mut self, spec: SpawnSpec) -> Result<(SessionId, Option<SignalTarget>), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Spawn(Box::new(spec)))?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::Spawned { session, pid } => Ok((session, pid)),
            _ => Err(ClientError::UnexpectedReply("expected Spawned")),
        }
    }

    /// Issues `Kill`.
    pub fn kill(&mut self, session: SessionId) -> Result<(), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Kill { session })?;
        expect_ok(self.wait_reply_ok(req)?)
    }

    /// Issues `Signal`.
    pub fn signal(&mut self, session: SessionId, signal: SignalKind, scope: SignalScope) -> Result<(), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Signal { session, signal, scope })?;
        expect_ok(self.wait_reply_ok(req)?)
    }

    /// Issues `Owns`.
    pub fn owns(&mut self, session: SessionId, pid: SignalTarget) -> Result<bool, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Owns { session, pid })?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::OwnsReply { owned } => Ok(owned),
            _ => Err(ClientError::UnexpectedReply("expected OwnsReply")),
        }
    }

    /// Issues `ListSessions`.
    pub fn list_sessions(&mut self) -> Result<Vec<SessionSummary>, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::ListSessions)?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::SessionsList { sessions } => Ok(sessions),
            _ => Err(ClientError::UnexpectedReply("expected SessionsList")),
        }
    }

    /// Issues `Rewind` and returns `(session, duration_ms, at_ms)`.
    pub fn rewind(&mut self, record_dir: String, at_ms: Option<u64>) -> Result<(SessionId, u64, u64), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Rewind { record_dir, at_ms })?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::Rewound { session, duration_ms, at_ms } => Ok((session, duration_ms, at_ms)),
            _ => Err(ClientError::UnexpectedReply("expected Rewound")),
        }
    }

    /// Issues `Seek` and returns actual timestamp reached.
    pub fn seek(&mut self, session: SessionId, at_ms: u64) -> Result<u64, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Seek { session, at_ms })?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::Sought { at_ms } => Ok(at_ms),
            _ => Err(ClientError::UnexpectedReply("expected Sought")),
        }
    }

    /// Issues `Metrics`.
    pub fn metrics(&mut self) -> Result<MetricsSnapshot, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Metrics)?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::MetricsReply(m) => Ok(m),
            _ => Err(ClientError::UnexpectedReply("expected MetricsReply")),
        }
    }

    /// Issues `Attach` on a non-zero `stream_id`.
    pub fn attach(
        &mut self,
        session: SessionId,
        stream_id: NonZeroStreamId,
        flags: SubFlags,
        initial_viewport: Option<ViewportHint>,
        initial_output_credit: u32,
        initial_frame_credit: FrameCredit,
    ) -> Result<BrokerMsg, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(
            StreamId::CONTROL,
            req,
            &ClientMsg::Attach {
                session,
                stream_id,
                flags,
                initial_viewport,
                initial_output_credit,
                initial_frame_credit,
            },
        )?;
        match self.wait_reply_ok(req)? {
            msg @ BrokerMsg::Attached { .. } => Ok(msg),
            _ => Err(ClientError::UnexpectedReply("expected Attached")),
        }
    }

    /// Issues `Detach`.
    pub fn detach(&mut self, stream_id: NonZeroStreamId) -> Result<(), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Detach { stream_id })?;
        expect_ok(self.wait_reply_ok(req)?)
    }

    /// Issues `Keep`.
    pub fn keep(&mut self, session: SessionId, kept: bool) -> Result<(), ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Keep { session, kept })?;
        expect_ok(self.wait_reply_ok(req)?)
    }

    /// Issues `Wait` and returns the [`WaitOutcome`].
    pub fn wait(
        &mut self,
        session: SessionId,
        condition: WaitCondition,
        timeout_ms: u32,
    ) -> Result<WaitOutcome, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(StreamId::CONTROL, req, &ClientMsg::Wait { session, condition, timeout_ms })?;
        match self.wait_reply_ok(req)? {
            BrokerMsg::Waited { outcome } => Ok(outcome),
            _ => Err(ClientError::UnexpectedReply("expected Waited")),
        }
    }

    /// Sends a stream-bound request and waits for its correlated reply, buffering any intervening pushes.
    pub fn stream_request(&mut self, stream_id: NonZeroStreamId, msg: &ClientMsg) -> Result<BrokerMsg, ClientError> {
        let req = self.alloc_req_id();
        self.send_raw(stream_id.as_stream_id(), req, msg)?;
        self.wait_reply(req)
    }

    /// Waits for a reply matching `req_id`, converting `BrokerMsg::Error` into [`ClientError::Broker`].
    fn wait_reply_ok(&mut self, req_id: RequestId) -> Result<BrokerMsg, ClientError> {
        match self.wait_reply(req_id)? {
            BrokerMsg::Error { code, message } => Err(ClientError::Broker { code, message }),
            other => Ok(other),
        }
    }

    /// Waits for a reply matching `req_id`, buffering unsolicited push frames in `self.pending`.
    pub fn wait_reply(&mut self, req_id: RequestId) -> Result<BrokerMsg, ClientError> {
        if let Some(pos) = self.pending.iter().position(|f| f.req_id == req_id) {
            return Ok(self.pending.remove(pos).expect("position in bounds").msg);
        }
        loop {
            let frame = self.read_one_from_wire()?;
            if frame.req_id == req_id {
                return Ok(frame.msg);
            }
            if self.pending.len() >= MAX_PENDING_FRAMES {
                return Err(ClientError::UnexpectedReply("too many buffered unsolicited frames"));
            }
            self.pending.push_back(frame);
        }
    }

    fn read_one_from_wire(&mut self) -> Result<ReceivedFrame, ClientError> {
        let mut scratch = [0u8; 8192];
        loop {
            if !self.rx_buf.is_empty() {
                let (consumed, decoded) = self.decoder.step(&self.rx_buf)?;
                self.rx_buf.drain(..consumed);
                if let Some(frame) = decoded {
                    return match frame {
                        DecodedFrame::Fin => Err(ClientError::Closed),
                        DecodedFrame::Frame { header, payload } => {
                            let msg = BrokerMsg::decode(header.msg_type, &payload)?;
                            Ok(ReceivedFrame { stream_id: header.stream_id, req_id: header.req_id, msg })
                        }
                    };
                }
            }
            let n = crate::read_retry_eintr(&mut self.transport, &mut scratch)?;
            if n == 0 {
                return Err(ClientError::Closed);
            }
            self.rx_buf.extend_from_slice(&scratch[..n]);
        }
    }
}

fn expect_ok(msg: BrokerMsg) -> Result<(), ClientError> {
    match msg {
        BrokerMsg::Ok => Ok(()),
        _ => Err(ClientError::UnexpectedReply("expected Ok")),
    }
}
