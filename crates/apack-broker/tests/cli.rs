use std::process::{Command, Stdio};
use std::time::Duration;

use apack_broker::client::BrokerClient;
use apack_broker::config::BrokerConfig;
use apack_broker::runtime::BrokerServer;
use apack_core::auth::AuthConfig;
#[cfg(unix)]
use tempfile::tempdir;

#[test]
fn cli_binary_version_invalid_serve_inspect_and_proxy() {
    let bin = env!("CARGO_BIN_EXE_apack-broker");

    let ok = Command::new(bin).arg("--version").output().expect("spawn apack-broker --version");
    assert!(ok.status.success());
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(stdout.contains("apack-broker"), "unexpected stdout: {stdout}");
    assert!(stdout.contains("protocol v2"), "unexpected stdout: {stdout}");

    let bad = Command::new(bin).arg("--not-a-command").output().expect("spawn apack-broker bad");
    assert!(!bad.status.success());
    assert_eq!(bad.status.code(), Some(2));

    // Subprocess `inspect --json` and `proxy --no-autostart` against a live BrokerServer!
    #[cfg(unix)]
    let tmp = tempdir().unwrap();
    #[cfg(unix)]
    let sock = tmp.path().join("cli-e2e.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let sock = format!(r"\\.\pipe\apack-v2-cli-test-{}", std::process::id());

    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();
    // Hold a broker lease so the daemon stays alive across `inspect`, duplicate `serve`, and `proxy`!
    let mut holder = BrokerClient::connect(&sock, &AuthConfig::None, "cli-holder").unwrap();
    let _ = holder.acquire_lease(apack_proto::types::LeaseKind::BrokerImmediate).unwrap();

    // `apack-broker inspect --socket <sock> --json`
    let insp =
        Command::new(bin).args(["inspect", "--socket", &sock, "--json"]).output().expect("spawn apack-broker inspect");
    assert!(insp.status.success(), "inspect stderr: {}", String::from_utf8_lossy(&insp.stderr));
    let insp_out = String::from_utf8_lossy(&insp.stdout);
    assert!(insp_out.contains("\"live_sessions\": 0"), "unexpected inspect output: {insp_out}");

    // Duplicate `serve` on held socket exits with HELD_EXIT (3)!
    let held = Command::new(bin).args(["serve", "--socket", &sock]).output().expect("spawn duplicate serve");
    assert_eq!(held.status.code(), Some(apack_os::transport::HELD_EXIT));

    // `proxy --socket <sock> --no-autostart` over stdin/stdout pipes!
    let mut proxy_child = Command::new(bin)
        .args(["proxy", "--socket", &sock, "--no-autostart"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn apack-broker proxy");

    let proxy_in = proxy_child.stdin.take().unwrap();
    let proxy_out = proxy_child.stdout.take().unwrap();
    struct ChildDuplex(std::process::ChildStdout, std::process::ChildStdin);
    impl std::io::Read for ChildDuplex {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }
    impl std::io::Write for ChildDuplex {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.1.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.1.flush()
        }
    }
    let mut client = BrokerClient::handshake(ChildDuplex(proxy_out, proxy_in), &AuthConfig::None, "cli-proxy").unwrap();
    assert!(client.list_sessions().unwrap().is_empty());
    drop(client);
    let proxy_status = proxy_child.wait().expect("wait proxy");
    assert!(proxy_status.success());

    drop(holder);
    handle.shutdown().unwrap();

    // `proxy --socket <fresh>` WITH autostart spawns a detached broker, serves handshake, and exits cleanly!
    #[cfg(unix)]
    let auto_sock = tmp.path().join("cli-autostart.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let auto_sock = format!(r"\\.\pipe\apack-v2-cli-auto-{}", std::process::id());
    let mut auto_proxy = Command::new(bin)
        .args(["proxy", "--socket", &auto_sock])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn apack-broker autostart proxy");
    let ap_in = auto_proxy.stdin.take().unwrap();
    let ap_out = auto_proxy.stdout.take().unwrap();
    let mut auto_client =
        BrokerClient::handshake(ChildDuplex(ap_out, ap_in), &AuthConfig::None, "cli-autostart").unwrap();
    assert!(auto_client.list_sessions().unwrap().is_empty());
    auto_client.send_fin().unwrap();
    drop(auto_client);
    assert!(auto_proxy.wait().expect("wait autostart proxy").success());

    // `serve --socket <fresh>` with short APACK_TERMINAL_START_GRACE_MS exits cleanly on grace timeout!
    #[cfg(unix)]
    let grace_sock = tmp.path().join("cli-grace.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let grace_sock = format!(r"\\.\pipe\apack-v2-cli-grace-{}", std::process::id());
    let grace = Command::new(bin)
        .args(["serve", "--socket", &grace_sock, "--shards", "1"])
        .env(apack_os::transport::START_GRACE_ENV, "80")
        .output()
        .expect("spawn grace serve");
    assert!(grace.status.success(), "grace serve failed: {}", String::from_utf8_lossy(&grace.stderr));
    let _ = Duration::from_millis(1);
}
