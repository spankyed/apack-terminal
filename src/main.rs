//! `apack-terminal` v2 broker CLI entrypoint (`serve`, `proxy`, `inspect`, `version`).

#![forbid(unsafe_code)]

use std::process::ExitCode;

use apack_broker::config::{BrokerConfig, CliCommand, clamp_shard_count, parse_cli, resolve_socket_path};
use apack_broker::inspect::{collect_report, format_report};
use apack_broker::proxy::run_stdio_proxy;
use apack_broker::runtime::BrokerServer;
use apack_os::OsError;
use apack_os::transport::HELD_EXIT;
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
        CliCommand::Version => {
            println!("apack-terminal {} (protocol v{PROTOCOL_VERSION})", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        CliCommand::Serve { socket, shards } => {
            let socket_path = match resolve_socket_path(socket) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("apack-terminal: {msg}");
                    return ExitCode::from(2);
                }
            };
            let mut cfg = BrokerConfig::for_socket(socket_path).with_env_overrides();
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
