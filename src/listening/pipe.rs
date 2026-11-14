//! Hanging up a named pipe: wholly Windows code, in a file of its own.
//!
//! The Linux mutation run mutates every changed file and cannot compile this one, so each mutant here would
//! be reported missed there. `.github/workflows/mutants.yml` names this file among the Windows-only ones: the
//! Linux run leaves it out and the Windows run mutates it.

#![cfg(windows)]

use std::os::windows::io::{AsHandle, AsRawHandle};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::IO::CancelIoEx;

/// Cancels a pipe's pending reads and writes, from any thread, and answers whether the cancel was made.
///
/// A named pipe cannot be shut down one way, and a synchronous call on the pipe's handle waits behind a read
/// parked on it, so a disconnect or a close does not end that read. Cancelling its I/O does.
pub fn hang_up(pipe: &impl AsHandle) -> bool {
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    // SAFETY: `handle` is the pipe's own handle, which lives for the length of this borrow. A null overlapped
    // pointer cancels every pending operation on the handle.
    unsafe { CancelIoEx(handle, std::ptr::null()) != 0 }
}
