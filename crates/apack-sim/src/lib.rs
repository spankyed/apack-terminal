//! Deterministic sans-IO simulation harness for `apack-terminal` v2.
//!
//! Connects simulated clients, [`apack_core::broker::BrokerCore`], and
//! [`apack_core::session::SessionMachine`] backed by [`mock_vt::MockVt`] over virtual time and
//! arbitrary chunk splits, asserting protocol, view-replication, security, and liveness invariants
//! continuously on every step.

#![forbid(unsafe_code)]

pub mod mock_vt;
pub mod world;

#[cfg(test)]
mod tests;
