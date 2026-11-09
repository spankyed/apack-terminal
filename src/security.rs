//! Who may open the broker's socket, on the platform where that is not already answered.
//!
//! **On a Unix system the socket is a file and the filesystem answers this.** It lives in the app's own
//! data directory, which the user owns, so the permissions that govern a connection are already theirs and
//! this module has nothing to add.
//!
//! **A Windows named pipe is not a file in that sense.** It carries a security descriptor, and the default
//! one a pipe gets grants read access to Everyone — the anonymous logon included. What crosses this pipe is
//! every byte the user's shells print and every key they type into them, so the default is wrong here in a
//! way that is invisible: nothing fails, and any local account can read the lot.
//!
//! So on Windows the listener is created with a descriptor of its own, naming the user who started the
//! broker and `SYSTEM`, and protected so that nothing is inherited into it. A descriptor that cannot be
//! built is a refusal to listen rather than a fallback: falling back here would mean serving terminal
//! output to everyone because something about the token could not be read, which is the one outcome worth
//! refusing a terminal over.

/// The access the broker's pipe grants, as SDDL, and nothing else.
///
/// - `O:` — the user who started the broker owns it.
/// - `D:P` — a DACL that inherits nothing, so no container's ACEs are added to it.
/// - `(A;;GA;;;{sid})` — that user may do everything.
/// - `(A;;GA;;;SY)` — and so may `SYSTEM`, which could take ownership regardless; saying so keeps a service
///   context from being refused by a descriptor that merely forgot about it.
///
/// Everyone else is denied by absence, which is what makes the `P` load-bearing.
///
/// **No group.** Nothing here references one — no ACE names it and the pipe is inherited into by nothing —
/// and the object does not hold the one a descriptor asks for: read back off a live pipe, its group is the
/// starting token's primary group whatever this string says. So naming one was a claim about the object
/// that was not true of it (`docs/architecture/windows-pipe-access.md`).
///
/// **Not `#[cfg(windows)]`, deliberately.** This string is the whole of the access decision and the part
/// most easily got wrong, so it is built where every platform's suite can read it back. What needs Windows
/// is turning it into a descriptor and asking the OS to accept one, below.
///
/// **What the kernel reports is not this string back.** `GA` is a generic right, which the object resolves
/// against its own mapping, so a live pipe reads as `FA` — full access to a file object, the same grant
/// under the name that object type uses for it.
pub fn access_for(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
}

/// The security descriptor the broker's pipe is created with: the starting user, and nobody else.
#[cfg(windows)]
pub fn owner_only() -> std::io::Result<interprocess::os::windows::security_descriptor::SecurityDescriptor> {
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;

    let sid = current_user_sid()?;
    let sddl = widestring::U16CString::from_str(access_for(&sid.to_string_lossy()))
        .expect("an SDDL string holds no interior nul");
    SecurityDescriptor::deserialize(&sddl)
}

/// **The real SID of the user this process is running as.**
///
/// A well-known alias will not do. `OW` (owner rights) and `CO` (creator owner) are placeholders resolved
/// against a descriptor that already has an owner, so in a DACL built from nothing they name no account,
/// and `IU` (interactive users) is every account logged in at this machine — which on a shared or
/// terminal-services host is exactly who must not read this pipe. So the token is asked.
#[cfg(windows)]
fn current_user_sid() -> std::io::Result<widestring::U16CString> {
    use std::io::Error;
    use widestring::U16CStr;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Closes the token however this function leaves, there being three ways out of it.
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: opened by `OpenProcessToken` below and closed exactly once, here.
            unsafe { CloseHandle(self.0) };
        }
    }

    // SAFETY: every call is checked before its result is read, the buffer handed to the second
    // `GetTokenInformation` is the size the first one asked for, and each of the two things that must be
    // released — the token and the string `ConvertSidToStringSidW` allocates — is released on every path.
    unsafe {
        let mut handle: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) == 0 {
            return Err(Error::last_os_error());
        }
        let token = Token(handle);

        // The documented two-call shape: the first asks how much room the answer needs and is expected to
        // fail with `ERROR_INSUFFICIENT_BUFFER`, so its return value says nothing this cares about
        let mut needed: u32 = 0;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(Error::last_os_error());
        }

        // `u64` rather than `u8`: a `TOKEN_USER` holds a pointer, and a byte vector is aligned for bytes.
        // Reading a struct out of an under-aligned buffer is undefined whatever the pointer arithmetic says
        let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
        if GetTokenInformation(token.0, TokenUser, buffer.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return Err(Error::last_os_error());
        }

        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
            return Err(Error::last_os_error());
        }
        let sid = U16CStr::from_ptr_str(text).to_ucstring();
        LocalFree(text.cast::<core::ffi::c_void>() as HLOCAL);
        Ok(sid)
    }
}
