//! Windows named-pipe transport helpers: per-SID default name, `GetSecurityInfo` owner check,
//! `CancelIoEx` + `DisconnectNamedPipe` hang-up, and `assume_flushed` non-discarding let-go.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and Windows CI mutates it.

#![cfg(windows)]

use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle};

use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;

/// Maps Windows named-pipe bind errors indicating an existing instance to [`crate::OsError::AlreadyHeld`].
pub fn map_bind_error(err: io::Error) -> crate::OsError {
    if matches!(err.kind(), io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied) {
        crate::OsError::AlreadyHeld
    } else {
        crate::OsError::io("binding local listener", err)
    }
}

/// Cancels pending I/O on a named pipe and disconnects it abruptly.
pub fn hang_up(pipe: &impl AsHandle) -> bool {
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    // SAFETY: `handle` is borrowed from a live pipe; a null overlapped pointer cancels all pending
    // I/O on `handle`, followed by disconnecting the pipe instance.
    unsafe {
        let cancelled = CancelIoEx(handle, std::ptr::null()) != 0;
        let disconnected = DisconnectNamedPipe(handle) != 0;
        cancelled || disconnected
    }
}

/// Cancels pending I/O without disconnecting so unread written bytes stay available to the peer,
/// switches to non-blocking before cancelling, waits for writers via `writers_ended`, and marks
/// the pipe `assume_flushed` so drop closes the handle immediately without hanging on a silent peer.
pub fn let_go(
    pipe: &interprocess::os::windows::named_pipe::local_socket::Stream,
    writers_ended: impl FnOnce(),
) -> bool {
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    let _ = pipe.inner().set_nonblocking(true);
    // SAFETY: `handle` is borrowed from a live pipe; null overlapped cancels all pending I/O.
    let cancelled = unsafe { CancelIoEx(handle, std::ptr::null()) != 0 };
    writers_ended();
    pipe.inner().assume_flushed();
    cancelled
}

/// Returns the default user-scoped Windows named pipe path (`\\.\pipe\apack-terminal-<SID>`).
pub fn default_pipe_name() -> Option<String> {
    let sid = crate::security::current_user_sid().ok()?;
    Some(format!(r"\\.\pipe\apack-terminal-{}", sid.to_string_lossy()))
}

/// Verifies that `pipe` is owned by the current user's SID.
pub fn owned_by_this_user(pipe: &impl AsHandle) -> io::Result<bool> {
    owned_by(pipe, &crate::security::current_user_sid()?)
}

fn owned_by(pipe: &impl AsHandle, sid: &widestring::U16CStr) -> io::Result<bool> {
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    // SAFETY: `handle` is borrowed from a live pipe. `GetSecurityInfo` allocates `descriptor`
    // (freed via `LocalFree` on every path) and `ConvertSidToStringSidW` allocates `text` (freed
    // via `LocalFree` after comparison).
    unsafe {
        let mut owner: PSID = std::ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
        let converted = ConvertSidToStringSidW(owner, &mut text);
        let convert_err = (converted == 0).then(io::Error::last_os_error);
        LocalFree(descriptor as HLOCAL);
        if let Some(err) = convert_err {
            return Err(err);
        }
        let matches = widestring::U16CStr::from_ptr_str(text) == sid;
        LocalFree(text.cast::<core::ffi::c_void>() as HLOCAL);
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, ToFsName};
    use interprocess::os::windows::local_socket::ListenerOptionsExt;

    use super::*;

    #[test]
    fn pipe_owner_check_let_go_and_hang_up() {
        let name = format!(r"\\.\pipe\apack-v2-os-test-{}", std::process::id());
        let listener = ListenerOptions::new()
            .name(name.clone().to_fs_name::<GenericFilePath>().expect("pipe name"))
            .security_descriptor(crate::security::owner_only_security_descriptor().expect("owner descriptor"))
            .create_sync()
            .expect("create pipe");

        let connect_name = name.clone();
        let client_th = std::thread::spawn(move || {
            Stream::connect(connect_name.to_fs_name::<GenericFilePath>().expect("name")).expect("connect")
        });
        let server = Arc::new(listener.accept().expect("accept"));
        let Stream::NamedPipe(client) = client_th.join().expect("join");

        assert!(owned_by_this_user(&client).expect("owner check"));
        let system = widestring::U16CString::from_str("S-1-5-18").expect("system sid");
        assert!(!owned_by(&client, &system).expect("not SYSTEM"));

        (&*server).write_all(b"preserved\n").expect("write");

        let (tx, rx) = channel();
        std::thread::spawn({
            let server = Arc::clone(&server);
            move || {
                let mut buf = [0u8; 16];
                let mut reader: &Stream = &server;
                let _ = tx.send(reader.read(&mut buf).map_err(|e| e.raw_os_error()));
            }
        });

        let mut cancelled = false;
        let start = std::time::Instant::now();
        let outcome = loop {
            assert!(start.elapsed() < Duration::from_secs(10));
            let Stream::NamedPipe(pipe) = &*server;
            cancelled |= let_go(pipe, || {});
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(res) => break res,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("reader exited unexpectedly: {e}"),
            }
        };
        assert!(cancelled);
        assert_eq!(outcome, Err(Some(995)));
        drop(server);

        let mut got = String::new();
        let _ = (&client).read_to_string(&mut got);
        assert_eq!(got, "preserved\n");

        // Second pipe: hang_up disconnects an idle connected pipe and returns false on repeat.
        let idle_name = format!(r"\\.\pipe\apack-v2-os-hangup-{}", std::process::id());
        let idle_listener = ListenerOptions::new()
            .name(idle_name.clone().to_fs_name::<GenericFilePath>().expect("name"))
            .create_sync()
            .expect("idle pipe");
        let idle_th = std::thread::spawn(move || {
            Stream::connect(idle_name.to_fs_name::<GenericFilePath>().expect("name")).expect("connect idle")
        });
        let Stream::NamedPipe(idle_server) = idle_listener.accept().expect("accept idle");
        let _idle_client = idle_th.join().expect("join idle");
        assert!(hang_up(&idle_server));
        assert!(!hang_up(&idle_server));
    }
}
