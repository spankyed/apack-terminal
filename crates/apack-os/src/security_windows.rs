//! Windows process-token SID lookup and owner-only `SecurityDescriptor` construction.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and the Windows mutation job
//! tests it natively.

#![cfg(windows)]

use std::io;
use std::mem::size_of;

use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::{U16CStr, U16CString};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::security::windows_pipe_sddl;

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` was opened by `OpenProcessToken` and is closed exactly once here.
        unsafe { CloseHandle(self.0) };
    }
}

/// Builds the protected owner-only `SecurityDescriptor` for the broker's named pipe.
pub fn owner_only_security_descriptor() -> io::Result<SecurityDescriptor> {
    let sid = current_user_sid()?;
    let sddl = U16CString::from_str(windows_pipe_sddl(&sid.to_string_lossy()))
        .expect("SDDL string never contains an interior NUL");
    SecurityDescriptor::deserialize(&sddl)
}

/// Reads the current process token's user SID string (`S-1-5-...`).
pub fn current_user_sid() -> io::Result<U16CString> {
    // SAFETY: `GetCurrentProcess` returns a valid pseudo-handle requiring no close.
    user_sid_for_process(unsafe { GetCurrentProcess() })
}

/// Reads the user SID string (`S-1-5-...`) from the primary token of `process`.
pub(crate) fn user_sid_for_process(process: HANDLE) -> io::Result<U16CString> {
    // SAFETY: Every Win32 call is checked before reading output buffers, `buffer` is `u64`-aligned
    // to satisfy `TOKEN_USER`, and both the token handle and `ConvertSidToStringSidW` string are
    // freed on all paths.
    unsafe {
        let mut raw_token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut raw_token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = TokenHandle(raw_token);

        let mut needed: u32 = 0;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        if GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return Err(io::Error::last_os_error());
        }

        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = U16CStr::from_ptr_str(text).to_ucstring();
        LocalFree(text.cast::<core::ffi::c_void>() as HLOCAL);
        Ok(sid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_and_descriptor_succeed() {
        let sid = current_user_sid().expect("read current user SID");
        let text = sid.to_string_lossy();
        assert!(text.starts_with("S-1-"), "unexpected SID format: {text}");
        let _sd = owner_only_security_descriptor().expect("build owner-only descriptor");
    }
}
