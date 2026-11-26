//! Windows named-pipe transport helpers: per-SID default name, `GetSecurityInfo` owner check,
//! `CancelIoEx` + `DisconnectNamedPipe` hang-up, and `assume_flushed` non-discarding let-go.
//!
//! Listed in `.cargo/windows-only` so Linux mutation shards skip it and Windows CI mutates it.

#![cfg(windows)]

use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Pipes::{DisconnectNamedPipe, GetNamedPipeClientProcessId};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

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

/// Verifies (client-side) that `pipe` is owned by the current user's SID.
pub fn owned_by_this_user(pipe: &impl AsHandle) -> io::Result<bool> {
    owned_by(pipe, &crate::security::current_user_sid()?)
}

/// Verifies (server-side) that the connected named-pipe client process runs as the current user's SID.
///
/// Unlike [`owned_by_this_user`] (which reads the pipe object's own owner SID), this inspects the
/// connecting client's PID via `GetNamedPipeClientProcessId` and compares its process token user SID
/// against the current user's SID.
pub fn client_is_this_user(pipe: &impl AsHandle) -> io::Result<bool> {
    let expected = crate::security::current_user_sid()?;
    let handle = pipe.as_handle().as_raw_handle() as HANDLE;
    let mut client_pid: u32 = 0;
    // SAFETY: `handle` is borrowed from a live server-side named pipe; `client_pid` is a valid out-pointer.
    if unsafe { GetNamedPipeClientProcessId(handle, &mut client_pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if client_pid == 0 {
        return Ok(false);
    }
    // SAFETY: opens the client process for query-only token inspection and closes the handle before returning.
    let proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, client_pid) };
    if proc.is_null() {
        return Ok(false);
    }
    let client_sid = crate::security_windows::user_sid_for_process(proc);
    // SAFETY: `proc` is a valid non-null handle opened by `OpenProcess` above.
    unsafe { CloseHandle(proc) };
    Ok(client_sid.is_ok_and(|sid| sid == expected))
}

struct CtrlHook {
    flag: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

static CTRL_HOOK: Mutex<Option<CtrlHook>> = Mutex::new(None);

// SAFETY: Windows calls this handler on a dedicated OS control thread (not an async POSIX signal
// context); locking a short non-reentrant mutex to set an atomic flag and wake the reactor is safe.
unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> i32 {
    if let Ok(guard) = CTRL_HOOK.lock()
        && let Some(hook) = guard.as_ref()
    {
        hook.flag.store(true, Ordering::Release);
        (hook.wake)();
        return 1;
    }
    0
}

/// Guard keeping a Windows `SetConsoleCtrlHandler` registration active until dropped.
pub struct ShutdownSignalGuard(());

impl Drop for ShutdownSignalGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = CTRL_HOOK.lock() {
            *guard = None;
        }
        // SAFETY: removes the exact handler registered in `install_shutdown_signals`.
        unsafe {
            SetConsoleCtrlHandler(Some(console_ctrl_handler), 0);
        }
    }
}

/// Installs a Windows console control handler (`Ctrl+C`, `Ctrl+Break`, close, shutdown) that sets
/// `flag` and calls `wake` so the broker shuts down cleanly.
pub fn install_shutdown_signals(
    flag: Arc<AtomicBool>,
    wake: impl Fn() + Send + Sync + 'static,
) -> io::Result<ShutdownSignalGuard> {
    if let Ok(mut guard) = CTRL_HOOK.lock() {
        *guard = Some(CtrlHook { flag, wake: Arc::new(wake) });
    }
    // SAFETY: registers a valid `extern "system"` console control callback.
    let ok = unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) } != 0;
    if !ok {
        if let Ok(mut guard) = CTRL_HOOK.lock() {
            *guard = None;
        }
        return Err(io::Error::last_os_error());
    }
    Ok(ShutdownSignalGuard(()))
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
    fn map_bind_error_classifies_held_vs_other() {
        assert!(matches!(
            map_bind_error(io::Error::new(io::ErrorKind::AddrInUse, "in use")),
            crate::OsError::AlreadyHeld
        ));
        assert!(matches!(
            map_bind_error(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            crate::OsError::AlreadyHeld
        ));
        assert!(matches!(
            map_bind_error(io::Error::new(io::ErrorKind::NotFound, "missing")),
            crate::OsError::Io { .. }
        ));
    }

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
        let Stream::NamedPipe(server_pipe) = &*server;
        assert!(client_is_this_user(server_pipe).expect("client user check"));
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

        // Second pipe: let_go on an idle pipe with no pending I/O cancels nothing (returns false),
        // and hang_up disconnects a connected idle pipe (returns true) then false on repeat.
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
        assert!(!let_go(&idle_server, || {}));
        assert!(hang_up(&idle_server));
        assert!(!hang_up(&idle_server));
    }

    #[test]
    fn shutdown_signal_guard_registers_invokes_and_unregisters() {
        let flag = Arc::new(AtomicBool::new(false));
        let woke = Arc::new(AtomicBool::new(false));
        let woke_cb = Arc::clone(&woke);
        let guard = install_shutdown_signals(Arc::clone(&flag), move || {
            woke_cb.store(true, Ordering::Release);
        })
        .expect("install console ctrl handler");
        // SAFETY: invoking the registered callback directly simulates an OS console control event.
        assert_eq!(unsafe { console_ctrl_handler(0) }, 1);
        assert!(flag.load(Ordering::Acquire));
        assert!(woke.load(Ordering::Acquire));
        drop(guard);
        // SAFETY: after dropping the guard, CTRL_HOOK is cleared and the callback returns 0.
        assert_eq!(unsafe { console_ctrl_handler(0) }, 0);
    }
}
