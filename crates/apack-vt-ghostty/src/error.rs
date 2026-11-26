//! Typed error returned by `libghostty-vt` C FFI calls.

use std::fmt;

use ghostty_vt_rs_sys as sys;

/// Error returned when a `libghostty-vt` FFI call fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhosttyError {
    /// Name of the C API function that failed.
    pub call: &'static str,
    /// Raw `GhosttyResult` error code.
    pub code: sys::Result::Type,
}

impl GhosttyError {
    /// Converts a `GhosttyResult` into `Result<(), GhosttyError>`.
    pub fn check(call: &'static str, code: sys::Result::Type) -> Result<(), Self> {
        if code == sys::Result::SUCCESS { Ok(()) } else { Err(Self { call, code }) }
    }
}

impl fmt::Display for GhosttyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code == sys::Result::UNSUPPORTED {
            write!(f, "{} failed with {} (unsupported by linked libghostty-vt build)", self.call, self.code)
        } else {
            write!(f, "{} failed with {}", self.call, self.code)
        }
    }
}

impl std::error::Error for GhosttyError {}
