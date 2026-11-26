//! Domain-separated mutual HMAC-SHA256 handshake authentication.
//!
//! Prevents both unauthorized clients and impersonating brokers when connecting across shared or
//! forwarded transports. All secret comparisons are constant-time.

use core::fmt;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const CLIENT_DOMAIN: &[u8] = b"apack-v2-client";
const SERVER_DOMAIN: &[u8] = b"apack-v2-server";

/// 32-byte shared HMAC authentication secret with redacted [`fmt::Debug`].
#[derive(Clone, PartialEq, Eq)]
pub struct AuthSecret([u8; 32]);

impl AuthSecret {
    /// Wraps a 32-byte secret key.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw key bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for AuthSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthSecret([REDACTED])")
    }
}

/// Broker authentication policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthConfig {
    /// No wire authentication required (relies on OS user-private socket/pipe ACLs).
    None,
    /// Mutual HMAC-SHA256 challenge-response required before any control or stream request.
    Required(AuthSecret),
}

impl AuthConfig {
    /// Returns `true` when clients must complete `AuthProof` after `HelloOk`.
    pub const fn is_required(&self) -> bool {
        matches!(self, Self::Required(_))
    }
}

/// Computes the client's HMAC-SHA256 proof over `CLIENT_DOMAIN || client_nonce || server_nonce`.
pub fn compute_client_proof(secret: &AuthSecret, client_nonce: &[u8; 32], server_nonce: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts 32-byte keys");
    mac.update(CLIENT_DOMAIN);
    mac.update(client_nonce);
    mac.update(server_nonce);
    let out = mac.finalize().into_bytes();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

/// Computes the broker's mutual HMAC-SHA256 proof over `SERVER_DOMAIN || server_nonce || client_nonce`.
pub fn compute_server_proof(secret: &AuthSecret, server_nonce: &[u8; 32], client_nonce: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts 32-byte keys");
    mac.update(SERVER_DOMAIN);
    mac.update(server_nonce);
    mac.update(client_nonce);
    let out = mac.finalize().into_bytes();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

/// Constant-time verification of a client's HMAC-SHA256 proof.
pub fn verify_client_proof(
    secret: &AuthSecret,
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
    candidate: &[u8; 32],
) -> bool {
    let expected = compute_client_proof(secret, client_nonce, server_nonce);
    ct_eq_32(&expected, candidate)
}

/// Constant-time verification of a broker's mutual HMAC-SHA256 proof.
pub fn verify_server_proof(
    secret: &AuthSecret,
    server_nonce: &[u8; 32],
    client_nonce: &[u8; 32],
    candidate: &[u8; 32],
) -> bool {
    let expected = compute_server_proof(secret, server_nonce, client_nonce);
    ct_eq_32(&expected, candidate)
}

/// Constant-time equality over two 32-byte arrays.
pub fn ct_eq_32(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}
