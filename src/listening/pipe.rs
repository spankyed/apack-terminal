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

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName, prelude::*};

    use crate::listening::hang_up;

    /// Hanging up a connection ends a read that is parked on it, and says that it did.
    ///
    /// The read is parked on the server end, as the broker's reads are. Its error is `ERROR_OPERATION_ABORTED`
    /// (995), which is what a cancelled read reports. Both halves are asserted: a hang-up that does nothing
    /// leaves the read parked, and one that reports the wrong thing is refused.
    #[test]
    fn hanging_up_ends_a_read_parked_on_the_connection_and_says_it_did() {
        let name = format!(r"\\.\pipe\apack-hang-up-test-{}", std::process::id());
        let listener = ListenerOptions::new()
            .name(name.clone().to_fs_name::<GenericFilePath>().expect("a pipe name"))
            .create_sync()
            .expect("the pipe is created");
        let connecting = std::thread::spawn({
            let name = name.clone();
            move || {
                Stream::connect(name.to_fs_name::<GenericFilePath>().expect("a pipe name")).expect("the pipe accepts")
            }
        });
        let server = Arc::new(listener.accept().expect("a connection is accepted"));
        let client = connecting.join().expect("the client connects");

        let (outcome_tx, outcome_rx) = channel();
        std::thread::spawn({
            let server = Arc::clone(&server);
            move || {
                let mut buffer = [0u8; 16];
                let mut reader: &Stream = &server;
                let outcome = reader.read(&mut buffer).map_err(|error| error.raw_os_error());
                let _ = outcome_tx.send(outcome);
            }
        });
        // Long enough that the read is parked before the hang-up. A hang-up that lands before it is parked
        // cancels nothing, and reports so.
        std::thread::sleep(Duration::from_millis(300));

        let cancelled = hang_up(&server);
        let outcome = outcome_rx.recv_timeout(Duration::from_secs(10)).expect("the parked read ends once hung up");
        assert!(cancelled, "the hang-up reported that it cancelled nothing");
        assert_eq!(outcome, Err(Some(995)), "the read did not end as a cancelled one");
        drop(client);
    }
}
