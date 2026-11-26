//! Top-level [`BrokerServer`] and in-process [`BrokerHandle`] lifecycle wrapper.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use apack_os::OsError;
use apack_os::signal::init_signal_descriptions;
use apack_os::transport::{
    ShutdownSignalGuard, SocketHold, acquire_socket_hold, bind_local_listener, ensure_private_directory,
    install_shutdown_signals,
};
use interprocess::local_socket::Listener;

use crate::config::BrokerConfig;
use crate::transport_loop::{ReactorWake, run_control_reactor};

/// Bound v2 terminal broker ready to run on the current thread or spawn onto a background thread.
pub struct BrokerServer {
    config: BrokerConfig,
    listener: Listener,
    _hold: SocketHold,
}

impl BrokerServer {
    /// Ensures the socket directory is user-private, acquires the exclusive broker hold, and binds
    /// the local Unix socket or Windows protected named pipe.
    pub fn bind(config: BrokerConfig) -> Result<Self, OsError> {
        init_signal_descriptions();
        ensure_private_directory(&config.socket_path)?;
        let hold = acquire_socket_hold(&config.socket_path)?;
        let listener = bind_local_listener(&config.socket_path)?;
        Ok(Self { config, listener, _hold: hold })
    }

    /// Returns the socket/pipe path this broker is listening on.
    pub fn socket_path(&self) -> &str {
        &self.config.socket_path
    }

    /// Runs the broker reactor loop on the calling thread until shutdown or idle exit.
    pub fn run(self) -> Result<(), OsError> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let sig_shutdown = Arc::clone(&shutdown);
        let mut sig_guard: Option<ShutdownSignalGuard> = None;
        let res = run_control_reactor(self.config, self.listener, shutdown, |wake| {
            sig_guard = install_shutdown_signals(sig_shutdown, move || wake.wake()).ok();
        })
        .map_err(|err| OsError::io("running control reactor", err));
        drop(sig_guard);
        #[allow(clippy::drop_non_drop)]
        drop(self._hold);
        res
    }

    /// Spawns the broker reactor loop onto a background thread and returns a [`BrokerHandle`]
    /// supporting clean programmatic shutdown (ideal for integration tests and embedded hosts).
    pub fn spawn(self) -> Result<BrokerHandle, OsError> {
        let socket_path = self.config.socket_path.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = mpsc::sync_channel::<ReactorWake>(1);
        let thread_shutdown = Arc::clone(&shutdown);
        let hold = self._hold;
        let config = self.config;
        let listener = self.listener;

        let thread = thread::Builder::new()
            .name(String::from("apack-reactor"))
            .spawn(move || {
                let res = run_control_reactor(config, listener, thread_shutdown, |wake| {
                    let _ = wake_tx.send(wake);
                });
                #[allow(clippy::drop_non_drop)]
                drop(hold);
                res
            })
            .map_err(|err| OsError::io("spawning reactor thread", err))?;

        let wake = wake_rx
            .recv()
            .map_err(|_| OsError::io("starting reactor", std::io::Error::other("reactor exited before ready")))?;

        Ok(BrokerHandle { socket_path, shutdown, wake, thread: Some(thread) })
    }
}

/// Handle to a background [`BrokerServer`] spawned via [`BrokerServer::spawn`].
pub struct BrokerHandle {
    socket_path: String,
    shutdown: Arc<AtomicBool>,
    wake: ReactorWake,
    thread: Option<JoinHandle<std::io::Result<()>>>,
}

impl BrokerHandle {
    /// Returns the socket/pipe path clients should connect to.
    pub fn socket_path(&self) -> &str {
        &self.socket_path
    }

    #[cfg(test)]
    pub(crate) fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Signals clean shutdown and waits for the reactor and all shards to exit.
    pub fn shutdown(&mut self) -> std::io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        self.wake.wake();
        if let Some(handle) = self.thread.take() {
            return handle.join().unwrap_or(Ok(()));
        }
        Ok(())
    }
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
