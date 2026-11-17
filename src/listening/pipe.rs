//! What a named pipe needs that a socket does not: hanging one up, asking whose one is, and the name a user's
//! broker has when nobody names one. Wholly Windows code, in a file of its own.
//!
//! The Linux mutation run mutates every changed file and cannot compile this one, so each mutant here would
//! be reported missed there. `.github/workflows/mutants.yml` names this file among the Windows-only ones: the
//! Linux run leaves it out and the Windows run mutates it.

#![cfg(windows)]

use std::os::windows::io::{AsHandle, AsRawHandle};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;

/// Hangs up a pipe: cancels its pending reads and writes, then disconnects it, and answers whether the cancel was made.
///
/// Cancelling comes first because a disconnect is a synchronous call on the same handle, and it waits behind a
/// read parked on it; once that read is cancelled nothing waits, and the disconnect ends the connection for the
/// far end, as a socket's shutdown does on POSIX. The far end's next write or read then fails.
pub fn hang_up(pipe: &impl AsHandle) -> bool {
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    // SAFETY: `handle` is the pipe's own handle, which lives for the length of this borrow. A null overlapped
    // pointer cancels every pending operation on the handle, and the disconnect is made on that same handle once
    // no operation is pending on it.
    unsafe {
        let cancelled = CancelIoEx(handle, std::ptr::null()) != 0;
        DisconnectNamedPipe(handle);
        cancelled
    }
}

/// Lets a pipe go once everything has been written to it: cancels its pending reads and writes, and answers
/// whether one was cancelled. **It does not disconnect**, because a disconnect throws away whatever the far end
/// has not read yet, answers included. The pipe ends for the far end when its handle closes, after it has read
/// what was written.
///
/// **And its handle closes when it is dropped, whether or not the far end has read.** interprocess otherwise
/// hands a pipe written to since its last flush to one thread that flushes such pipes in turn before closing
/// them, and a flush waits for the far end to read. A caller that finished and stopped reading would hold that
/// thread, and every pipe dropped after it would wait behind it with its far end never told the connection
/// ended. Closing the handle unflushed loses nothing: what was written stays for the far end to read.
pub fn let_go(pipe: &interprocess::os::windows::named_pipe::local_socket::Stream) -> bool {
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    // SAFETY: `handle` is the pipe's own handle, which lives for the length of this borrow, and a null
    // overlapped pointer cancels every pending operation on it
    let cancelled = unsafe { CancelIoEx(handle, std::ptr::null()) != 0 };
    // After the cancel, so a write it ended or that finished just before it leaves nothing to flush
    pipe.inner().assume_flushed();
    cancelled
}

/// Where a user's broker is when nobody says where: a pipe named for the user, by their SID.
///
/// **A pipe's name is one namespace for the whole machine**, so it is the SID that keeps two users' brokers apart,
/// as each user's own directory does on POSIX — and unlike a directory, it keeps nobody else from taking the name
/// first. What does that is asking whose a pipe is before carrying a byte to it (`owned_by_this_user`).
pub fn default_name() -> Option<String> {
    let sid = crate::security::current_user_sid().ok()?;
    Some(format!(r"\\.\pipe\apack-terminal-{}", sid.to_string_lossy()))
}

/// Whether the pipe `pipe` is connected to belongs to the user this process runs as.
///
/// **Its owner, asked of the kernel.** A pipe's name can be taken by any account first, and a client that connects
/// to it by name reaches whoever did. The owner a pipe is created with is its creator, or a SID the creator holds:
/// naming another user takes a privilege an ordinary account does not have. So a pipe this user did not make
/// answers with someone else's SID, and is refused before it is handed anything (`proxy.rs`). The broker's own
/// pipe names its owner outright (`security::access_for`).
pub fn owned_by_this_user(pipe: &impl AsHandle) -> std::io::Result<bool> {
    owned_by(pipe, &crate::security::current_user_sid()?)
}

/// Whether `pipe` is owned by the account `sid` names, as SDDL spells a SID (`S-1-5-…`).
fn owned_by(pipe: &impl AsHandle, sid: &widestring::U16CStr) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};

    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    // SAFETY: `handle` is the pipe's own handle, which lives for the length of this borrow. `GetSecurityInfo` is
    // asked for the owner alone, which points into the descriptor it allocates; that descriptor is freed on every
    // path after the owner has been read, and the string `ConvertSidToStringSidW` allocates is freed once copied.
    unsafe {
        let mut owner: PSID = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let asked = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        if asked != 0 {
            return Err(std::io::Error::from_raw_os_error(asked as i32));
        }
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        let converted = ConvertSidToStringSidW(owner, &mut text);
        LocalFree(descriptor as HLOCAL);
        if converted == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let found = widestring::U16CStr::from_ptr_str(text) == sid;
        LocalFree(text.cast::<core::ffi::c_void>() as HLOCAL);
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName, prelude::*};

    use crate::listening::{hang_up, let_go};

    use super::{owned_by, owned_by_this_user};

    /// Letting a connection go ends a read parked on it, says that it did, and **leaves what was written for the
    /// far end to read**: a disconnect would have thrown it away, which is the difference from hanging up.
    ///
    /// Let go until the read ends, since one made before the read is parked cancels nothing and says so.
    #[test]
    fn letting_go_ends_a_parked_read_and_leaves_what_was_written_to_be_read() {
        let name = format!(r"\\.\pipe\apack-let-go-test-{}", std::process::id());
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
        (&*server).write_all(b"answered\n").expect("the answer is written");

        let (outcome_tx, outcome_rx) = channel();
        std::thread::spawn({
            let server = Arc::clone(&server);
            move || {
                let mut buffer = [0u8; 16];
                let mut reader: &Stream = &server;
                let _ = outcome_tx.send(reader.read(&mut buffer).map_err(|error| error.raw_os_error()));
            }
        });
        let mut cancelled = false;
        let started = std::time::Instant::now();
        let outcome = loop {
            assert!(started.elapsed() < Duration::from_secs(10), "the read stayed parked after being let go");
            cancelled |= let_go(&server);
            match outcome_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(outcome) => break outcome,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(gone) => panic!("the reading thread ended without saying how: {gone}"),
            }
        };
        assert!(cancelled, "letting go reported that it cancelled nothing");
        assert_eq!(outcome, Err(Some(995)), "the read did not end as a cancelled one");

        drop(server);
        let mut read = String::new();
        let _ = (&client).read_to_string(&mut read);
        assert_eq!(read, "answered\n", "what was written before letting go was not there to read");
    }

    /// A pipe that was let go closes when it is dropped, however little of it the far end has read: the far end's
    /// next write fails at once rather than once a flush it never answers is over. What was written before is
    /// still there to read.
    #[test]
    fn a_pipe_let_go_closes_when_dropped_and_leaves_what_was_written_to_be_read() {
        let name = format!(r"\\.\pipe\apack-let-go-close-test-{}", std::process::id());
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
        let server = listener.accept().expect("a connection is accepted");
        let client = connecting.join().expect("the client connects");
        (&server).write_all(b"answered\n").expect("the answer is written");

        let_go(&server);
        drop(server);
        assert!((&client).write_all(b"more\n").is_err(), "the pipe was still open after it was let go and dropped");
        let mut read = String::new();
        let _ = (&client).read_to_string(&mut read);
        assert_eq!(read, "answered\n", "what was written before letting go was not there to read");
    }

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
        // The far end sees the connection end: its read returns, with end of file or an error, rather than waiting
        let (far_tx, far_rx) = channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 16];
            let mut reader: &Stream = &client;
            let _ = far_tx.send(reader.read(&mut buffer).is_ok());
        });
        far_rx.recv_timeout(Duration::from_secs(10)).expect("the far end's read ends once hung up");
    }

    /// A pipe made as the broker makes its own is this user's, and is not another account's: asked from the
    /// connecting end, which is where a proxy asks. The other account is `SYSTEM`, whose SID is the same on every
    /// machine; a pipe an ordinary user cannot make is the case that needs another account, and was made by hand
    /// (`docs/architecture/windows-pipe-access.md`).
    #[test]
    fn a_pipe_made_as_the_brokers_is_this_users_and_no_one_elses() {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;

        let name = format!(r"\\.\pipe\apack-owner-test-{}", std::process::id());
        let listener = ListenerOptions::new()
            .name(name.clone().to_fs_name::<GenericFilePath>().expect("a pipe name"))
            .security_descriptor(crate::security::owner_only().expect("the broker's descriptor"))
            .create_sync()
            .expect("the pipe is created");
        let connecting = std::thread::spawn(move || {
            Stream::connect(name.to_fs_name::<GenericFilePath>().expect("a pipe name")).expect("the pipe accepts")
        });
        let _server = listener.accept().expect("a connection is accepted");
        let Stream::NamedPipe(client) = connecting.join().expect("the client connects");

        assert!(owned_by_this_user(&client).expect("the owner is read"), "the broker's pipe is not its user's");
        let system = widestring::U16CString::from_str("S-1-5-18").expect("no interior nul");
        assert!(!owned_by(&client, &system).expect("the owner is read"), "the broker's pipe is SYSTEM's");
    }
}
