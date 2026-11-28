//! `apack-terminal` v2 broker CLI entrypoint (`serve`, `proxy`, `inspect`, `version`).

#![deny(unsafe_code)]

use std::process::ExitCode;

use apack_broker::config::{AUTH_KEY_ENV, BrokerConfig, CliCommand, clamp_shard_count, parse_cli, resolve_socket_path};
use apack_broker::inspect::{collect_report, format_report};
use apack_broker::proxy::run_stdio_proxy;
use apack_broker::runtime::BrokerServer;
use apack_os::OsError;
use apack_os::transport::{HELD_EXIT, START_GRACE_ENV, prepare_detached_daemon, take_secret_env_at_startup};
use apack_proto::PROTOCOL_VERSION;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_cli(&args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    match cmd {
        CliCommand::Help(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        CliCommand::Version => {
            println!("apack-terminal {} (protocol v{PROTOCOL_VERSION})", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        CliCommand::Serve { socket, shards, detached } => {
            let socket_path = match resolve_socket_path(socket) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("apack-terminal: {msg}");
                    return ExitCode::from(2);
                }
            };
            // Read and strip `APACK_TERMINAL_AUTH_KEY` from the broker process `environ` while
            // still strictly single-threaded so spawned child shells cannot inherit it.
            let grace_env = std::env::var(START_GRACE_ENV).ok();
            #[allow(unsafe_code)]
            // SAFETY: invoked strictly at single-threaded `main()` startup before any threads exist.
            let auth_key = unsafe { take_secret_env_at_startup(AUTH_KEY_ENV) };
            let mut cfg =
                BrokerConfig::for_socket(socket_path).apply_env_overrides(grace_env.as_deref(), auth_key.as_deref());
            if let Some(n) = shards {
                cfg.shard_count = clamp_shard_count(n);
            }
            let server = match BrokerServer::bind(cfg) {
                Ok(s) => s,
                Err(OsError::AlreadyHeld) => return ExitCode::from(HELD_EXIT as u8),
                Err(err) => {
                    eprintln!("apack-terminal: {err}");
                    return ExitCode::FAILURE;
                }
            };
            // Still strictly single-threaded: ignore SIGHUP when stdin is not a TTY, and redirect
            // piped startup stderr to /dev/null (NUL on Windows) ONLY when `--detached` was passed
            // by `proxy` autostart so supervised `serve` invocations keep stderr intact.
            prepare_detached_daemon(detached);
            if let Err(err) = server.run() {
                eprintln!("apack-terminal: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        CliCommand::Proxy { socket, autostart } => {
            let socket_path = match resolve_socket_path(socket) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("apack-terminal proxy: {msg}");
                    return ExitCode::from(2);
                }
            };
            if let Err(err) = run_stdio_proxy(&socket_path, autostart) {
                eprintln!("apack-terminal proxy: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        CliCommand::Inspect { socket, json } => {
            let socket_path = match resolve_socket_path(socket) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("apack-terminal inspect: {msg}");
                    return ExitCode::from(2);
                }
            };
            let cfg = BrokerConfig::for_socket(socket_path).with_env_overrides();
            match collect_report(&cfg.socket_path, &cfg.auth) {
                Ok(report) => {
                    print!("{}", format_report(&report, json));
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("apack-terminal inspect: {err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
