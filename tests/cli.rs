use std::process::{Child, Command, ExitStatus, Stdio};

use apack_broker::client::BrokerClient;
use apack_broker::config::BrokerConfig;
use apack_broker::runtime::BrokerServer;
use apack_core::auth::AuthConfig;
#[cfg(unix)]
use tempfile::tempdir;

/// RAII subprocess guard so an early test assertion panic never leaks broker/proxy child processes.
struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn inner_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child still armed")
    }

    fn wait_and_disarm(mut self) -> std::io::Result<ExitStatus> {
        self.0.take().expect("child still armed").wait()
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn cli_binary_usage_version_serve_inspect_and_proxy() {
    let bin = env!("CARGO_BIN_EXE_apack-terminal");

    // No arguments prints usage matching `node/release/smoke.mjs` and exits with status 2.
    let no_args = Command::new(bin).output().expect("spawn apack-terminal with no args");
    assert_eq!(no_args.status.code(), Some(2));
    let no_args_err = String::from_utf8_lossy(&no_args.stderr);
    assert!(no_args_err.contains("usage: apack-terminal"), "unexpected stderr: {no_args_err}");

    // Top-level & per-subcommand `-h` / `--help` print usage to stdout and exit 0 (`M16`)!
    for help_args in [
        &["-h"][..],
        &["--help"][..],
        &["serve", "--help"][..],
        &["proxy", "-h"][..],
        &["inspect", "--help"][..],
        &["version", "--help"][..],
    ] {
        let h = Command::new(bin).args(help_args).output().expect("spawn help");
        assert!(h.status.success(), "{help_args:?} must exit 0");
        let h_out = String::from_utf8_lossy(&h.stdout);
        assert!(h_out.contains("usage: apack-terminal"), "unexpected help stdout for {help_args:?}: {h_out}");
    }

    let ok = Command::new(bin).arg("--version").output().expect("spawn apack-terminal --version");
    assert!(ok.status.success());
    let stdout = String::from_utf8_lossy(&ok.stdout);
    assert!(stdout.contains("apack-terminal"), "unexpected stdout: {stdout}");
    assert!(stdout.contains("protocol v2"), "unexpected stdout: {stdout}");

    let bad = Command::new(bin).arg("--not-a-command").output().expect("spawn apack-terminal bad");
    assert!(!bad.status.success());
    assert_eq!(bad.status.code(), Some(2));

    // Subprocess `inspect --json` and `proxy --no-autostart` against a live BrokerServer!
    #[cfg(unix)]
    let tmp = tempdir().unwrap();
    #[cfg(unix)]
    let sock = tmp.path().join("p/cli-e2e.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let sock = format!(r"\\.\pipe\apack-v2-cli-test-{}", std::process::id());

    let mut cfg = BrokerConfig::for_socket(&sock);
    cfg.shard_count = 1;
    let mut handle = BrokerServer::bind(cfg).unwrap().spawn().unwrap();
    // Hold a broker lease so the daemon stays alive across `inspect`, duplicate `serve`, and `proxy`!
    let mut holder = BrokerClient::connect(&sock, &AuthConfig::None, "cli-holder").unwrap();
    let _ = holder.acquire_lease(apack_proto::types::LeaseKind::BrokerImmediate).unwrap();

    // `apack-terminal inspect --socket <sock> --json`
    let insp = Command::new(bin)
        .args(["inspect", "--socket", &sock, "--json"])
        .output()
        .expect("spawn apack-terminal inspect");
    assert!(insp.status.success(), "inspect stderr: {}", String::from_utf8_lossy(&insp.stderr));
    let insp_out = String::from_utf8_lossy(&insp.stdout);
    assert!(insp_out.contains("\"live_sessions\": 0"), "unexpected inspect output: {insp_out}");
    assert!(insp_out.ends_with('\n'), "inspect --json must terminate with a trailing newline");

    // Duplicate `serve` on held socket exits with HELD_EXIT (3)!
    let held = Command::new(bin).args(["serve", "--socket", &sock]).output().expect("spawn duplicate serve");
    assert_eq!(held.status.code(), Some(apack_os::transport::HELD_EXIT));

    // `proxy --socket <sock> --no-autostart` over stdin/stdout pipes!
    let mut proxy_child = KillOnDrop::new(
        Command::new(bin)
            .args(["proxy", "--socket", &sock, "--no-autostart"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn apack-terminal proxy"),
    );

    let proxy_in = proxy_child.inner_mut().stdin.take().unwrap();
    let proxy_out = proxy_child.inner_mut().stdout.take().unwrap();
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
    let proxy_status = proxy_child.wait_and_disarm().expect("wait proxy");
    assert!(proxy_status.success());

    drop(holder);
    handle.shutdown().unwrap();

    // `proxy --socket <fresh>` WITH autostart spawns a detached broker, serves handshake, and exits cleanly!
    #[cfg(unix)]
    let auto_sock = tmp.path().join("p/cli-autostart.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let auto_sock = format!(r"\\.\pipe\apack-v2-cli-auto-{}", std::process::id());
    let mut auto_proxy = KillOnDrop::new(
        Command::new(bin)
            .args(["proxy", "--socket", &auto_sock])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn apack-terminal autostart proxy"),
    );
    let ap_in = auto_proxy.inner_mut().stdin.take().unwrap();
    let ap_out = auto_proxy.inner_mut().stdout.take().unwrap();
    let mut auto_client =
        BrokerClient::handshake(ChildDuplex(ap_out, ap_in), &AuthConfig::None, "cli-autostart").unwrap();
    assert!(auto_client.list_sessions().unwrap().is_empty());
    auto_client.send_fin().unwrap();
    drop(auto_client);
    assert!(auto_proxy.wait_and_disarm().expect("wait autostart proxy").success());
    // Wait for the detached autostarted broker to finish shutting down and unlink its socket
    // before `tmp` drops so no daemon lingers after the test exits.
    #[cfg(unix)]
    {
        let unlink_dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::path::Path::new(&auto_sock).exists() && std::time::Instant::now() < unlink_dl {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!std::path::Path::new(&auto_sock).exists(), "autostarted broker must unlink {auto_sock} on exit");
    }

    // `serve --socket <fresh>` with short APACK_TERMINAL_START_GRACE_MS exits cleanly on grace timeout!
    #[cfg(unix)]
    let grace_sock = tmp.path().join("p/cli-grace.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let grace_sock = format!(r"\\.\pipe\apack-v2-cli-grace-{}", std::process::id());
    let grace = Command::new(bin)
        .args(["serve", "--socket", &grace_sock, "--shards", "1"])
        .env(apack_os::transport::START_GRACE_ENV, "80")
        .output()
        .expect("spawn grace serve");
    assert!(grace.status.success(), "grace serve failed: {}", String::from_utf8_lossy(&grace.stderr));

    // `serve` with `APACK_TERMINAL_AUTH_KEY` enforces auth AND strips the secret from broker env
    // before spawning threads/shells (`L10`)!
    #[cfg(unix)]
    let auth_sock = tmp.path().join("p/cli-auth.sock").to_string_lossy().into_owned();
    #[cfg(windows)]
    let auth_sock = format!(r"\\.\pipe\apack-v2-cli-auth-{}", std::process::id());
    let auth_secret = "pr6-super-secret-broker-key";
    let mut auth_srv = KillOnDrop::new(
        Command::new(bin)
            .args(["serve", "--socket", &auth_sock, "--shards", "1"])
            .env(apack_broker::config::AUTH_KEY_ENV, auth_secret)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn auth serve"),
    );
    let auth_cfg = AuthConfig::Required(apack_broker::config::derive_auth_secret(auth_secret.as_bytes()));
    let mut auth_client = None;
    let auth_dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < auth_dl {
        if let Ok(c) = BrokerClient::connect(&auth_sock, &auth_cfg, "cli-auth-client") {
            auth_client = Some(c);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut auth_client = auth_client.expect("connect to authenticated serve subprocess");
    // Detached (`Stdio::null()` stdin) `serve` must ignore SIGHUP (`H11`) and keep serving clients.
    #[cfg(unix)]
    {
        let hup = Command::new("kill")
            .args(["-HUP", &auth_srv.inner_mut().id().to_string()])
            .status()
            .expect("send SIGHUP to detached serve");
        assert!(hup.success(), "kill -HUP failed");
        // Verify broker stays responsive after SIGHUP via an RPC roundtrip rather than a blind sleep.
        let _ = auth_client.list_sessions().expect("detached serve must keep serving after SIGHUP");
        assert!(
            auth_srv.inner_mut().try_wait().expect("try_wait after SIGHUP").is_none(),
            "detached serve died on SIGHUP!"
        );
    }
    let shell_spec = apack_proto::messages::SpawnSpec {
        argv: if cfg!(windows) {
            vec![String::from("cmd.exe"), String::from("/C"), String::from("echo KEY=[%APACK_TERMINAL_AUTH_KEY%]")]
        } else {
            vec![
                String::from("/bin/sh"),
                String::from("-c"),
                String::from("printf 'KEY=[%s]\\r\\n' \"$APACK_TERMINAL_AUTH_KEY\""),
            ]
        },
        cwd: None,
        env: Vec::new(),
        size: apack_proto::types::GridSize::new(80, 24).unwrap(),
        cell_px: apack_proto::types::CellPx { width: 8, height: 16 },
        scrollback_limit: 100,
        marks_key: None,
        resize_policy: apack_proto::types::ResizePolicy::LatestActive,
        record_dir: None,
        restore_from: None,
        image_files: false,
        kept: false,
    };
    let (auth_sid, _) = auth_client.spawn(shell_spec).unwrap();
    let expected_key = if cfg!(windows) { "KEY=[%APACK_TERMINAL_AUTH_KEY%]" } else { "KEY=[]" };
    assert_eq!(
        auth_client.wait(auth_sid, apack_proto::types::WaitCondition::Text(String::from(expected_key)), 5_000).unwrap(),
        apack_proto::types::WaitOutcome::Met
    );
    let _ = auth_client.send_fin();
    drop(auth_client);
    // `auth_srv` MUST auto-exit cleanly with status 0 once its sole client disconnects (tearing down
    // the session & unlinking its socket); `KillOnDrop` still guarantees cleanup on assertion panic.
    let auth_exit_dl = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut auth_status = None;
    while std::time::Instant::now() < auth_exit_dl {
        if let Some(st) = auth_srv.inner_mut().try_wait().expect("try_wait auth_srv") {
            auth_status = Some(st);
            auth_srv.disarm();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = auth_status.expect("auth serve must auto-exit cleanly after sole client disconnects");
    assert!(status.success(), "auth serve exited with non-zero status: {status}");
    #[cfg(unix)]
    {
        let unlink_dl = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::path::Path::new(&auth_sock).exists() && std::time::Instant::now() < unlink_dl {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!std::path::Path::new(&auth_sock).exists(), "auth serve must unlink {auth_sock} on exit");
    }
}
