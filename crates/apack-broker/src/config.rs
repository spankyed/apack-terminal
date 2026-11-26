//! Broker configuration and CLI argument parsing.

use std::time::Duration;

use apack_core::MAX_CONTROL_QUEUE_BYTES;
use apack_core::auth::{AuthConfig, AuthSecret};
use apack_os::transport::{START_GRACE, START_GRACE_ENV, default_socket_path, start_grace};
use apack_wal::WalConfig;
use sha2::{Digest, Sha256};

/// Derives a 32-byte [`AuthSecret`] from an arbitrary passphrase/key slice via SHA-256.
pub fn derive_auth_secret(secret: &[u8]) -> AuthSecret {
    let digest = Sha256::digest(secret);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    AuthSecret::new(out)
}

/// Maximum number of session shards spawned by default (`min(cores, 8)`).
pub const MAX_DEFAULT_SHARDS: usize = 8;

/// Default idle duration before a shard cooperatively compresses a session's scrollback (`2 s`).
pub const DEFAULT_IDLE_COMPRESS: Duration = Duration::from_secs(2);

/// Default escalation delay after `SIGHUP` before `SIGKILL` of surviving descendants (`2 s`).
pub const DEFAULT_KILL_ESCALATE: Duration = Duration::from_secs(2);

/// Environment variable supplying the optional mutual-HMAC authentication secret.
pub const AUTH_KEY_ENV: &str = "APACK_TERMINAL_KEY";

/// Runtime configuration for [`crate::BrokerServer`].
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Local Unix socket path or Windows named pipe (`\\.\pipe\...`).
    pub socket_path: String,
    /// Optional mutual HMAC-SHA256 authentication configuration.
    pub auth: AuthConfig,
    /// How long a freshly started broker waits for its first connection before exiting.
    pub start_grace: Duration,
    /// Number of session shard threads (`>= 1`).
    pub shard_count: usize,
    /// Per-connection outbound queue hard byte cap.
    pub max_queue_bytes: usize,
    /// Idle duration before cooperative scrollback compression starts.
    pub idle_compress_after: Duration,
    /// Escalation delay after graceful `SIGHUP` before `SIGKILL` of surviving descendants.
    pub kill_escalate_after: Duration,
    /// Segmented WAL defaults for recorded sessions.
    pub wal_config: WalConfig,
}

impl BrokerConfig {
    /// Builds a standard configuration bound to `socket_path` with `min(cores, 8)` shards.
    pub fn for_socket(socket_path: impl Into<String>) -> Self {
        let cores = std::thread::available_parallelism().map_or(2, std::num::NonZeroUsize::get);
        Self {
            socket_path: socket_path.into(),
            auth: AuthConfig::None,
            start_grace: START_GRACE,
            shard_count: clamp_shard_count(cores),
            max_queue_bytes: MAX_CONTROL_QUEUE_BYTES,
            idle_compress_after: DEFAULT_IDLE_COMPRESS,
            kill_escalate_after: DEFAULT_KILL_ESCALATE,
            wal_config: WalConfig::default(),
        }
    }

    /// Applies optional `start_grace` and `auth_key` string overrides.
    pub fn apply_env_overrides(mut self, grace: Option<&str>, auth_key: Option<&str>) -> Self {
        self.start_grace = start_grace(grace);
        if let Some(secret) = auth_key
            && !secret.is_empty()
        {
            self.auth = AuthConfig::Required(derive_auth_secret(secret.as_bytes()));
        }
        self
    }

    /// Loads `start_grace` and optional `APACK_TERMINAL_KEY` from process environment variables.
    pub fn with_env_overrides(self) -> Self {
        let grace_env = std::env::var(START_GRACE_ENV).ok();
        let key_env = std::env::var(AUTH_KEY_ENV).ok();
        self.apply_env_overrides(grace_env.as_deref(), key_env.as_deref())
    }
}

/// Clamps requested shard count to `1..=MAX_DEFAULT_SHARDS`.
pub fn clamp_shard_count(requested: usize) -> usize {
    requested.clamp(1, MAX_DEFAULT_SHARDS)
}

/// Parsed top-level CLI subcommand for `apack-broker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    /// Run the broker daemon listening on a local socket/pipe.
    Serve {
        /// Optional explicit socket/pipe path (defaults to per-user path).
        socket: Option<String>,
        /// Optional explicit shard count override (`1..=8`).
        shards: Option<usize>,
    },
    /// Run the dumb single-channel stdio-to-broker proxy (for SSH remote connections).
    Proxy {
        /// Optional explicit socket/pipe path.
        socket: Option<String>,
        /// Automatically spawn a detached broker when no broker is listening.
        autostart: bool,
    },
    /// Inspect live broker sessions and telemetry metrics.
    Inspect {
        /// Optional explicit socket/pipe path.
        socket: Option<String>,
        /// Emit machine-readable JSON instead of human-readable text.
        json: bool,
    },
    /// Print version and protocol version.
    Version,
}

/// Usage banner returned when `apack-terminal` is invoked with no subcommand.
pub const CLI_USAGE: &str = "usage: apack-terminal <serve | proxy | inspect | version> [options]";

/// Parses CLI arguments (excluding `argv[0]`) into a [`CliCommand`].
pub fn parse_cli(args: &[String]) -> Result<CliCommand, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Err(String::from(CLI_USAGE));
    };
    match cmd.as_str() {
        "serve" => {
            let mut socket = None;
            let mut shards = None;
            let mut iter = rest.iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--socket" => {
                        let val = iter.next().ok_or_else(|| String::from("--socket requires a value"))?;
                        socket = Some(val.clone());
                    }
                    "--shards" => {
                        let val = iter.next().ok_or_else(|| String::from("--shards requires a number"))?;
                        let parsed = val.parse::<usize>().map_err(|_| format!("invalid --shards value: {val}"))?;
                        if parsed == 0 || parsed > MAX_DEFAULT_SHARDS {
                            return Err(format!("--shards must be in 1..={MAX_DEFAULT_SHARDS}"));
                        }
                        shards = Some(parsed);
                    }
                    other => return Err(format!("unknown serve flag: {other}")),
                }
            }
            Ok(CliCommand::Serve { socket, shards })
        }
        "proxy" => {
            let mut socket = None;
            let mut autostart = true;
            let mut iter = rest.iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--socket" => {
                        let val = iter.next().ok_or_else(|| String::from("--socket requires a value"))?;
                        socket = Some(val.clone());
                    }
                    "--no-autostart" => {
                        autostart = false;
                    }
                    other => return Err(format!("unknown proxy flag: {other}")),
                }
            }
            Ok(CliCommand::Proxy { socket, autostart })
        }
        "inspect" => {
            let mut socket = None;
            let mut json = false;
            let mut iter = rest.iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--socket" => {
                        let val = iter.next().ok_or_else(|| String::from("--socket requires a value"))?;
                        socket = Some(val.clone());
                    }
                    "--json" => {
                        json = true;
                    }
                    other => return Err(format!("unknown inspect flag: {other}")),
                }
            }
            Ok(CliCommand::Inspect { socket, json })
        }
        "--version" | "-V" | "version" => {
            if !rest.is_empty() {
                return Err(format!("unexpected arguments to version: {}", rest.join(" ")));
            }
            Ok(CliCommand::Version)
        }
        other => Err(format!("unknown subcommand: {other} (expected serve, proxy, inspect, or version)")),
    }
}

/// Resolves an explicit `--socket` flag or falls back to [`default_socket_path`].
pub fn resolve_socket_path(explicit: Option<String>) -> Result<String, String> {
    match explicit {
        Some(path) if !path.is_empty() => Ok(path),
        Some(_) => Err(String::from("--socket path must not be empty")),
        None => default_socket_path().ok_or_else(|| String::from("could not determine default broker socket path")),
    }
}
