//! What the candidate crates actually do, tried rather than argued.
//!
//! Phase S of the apack monorepo's terminal-ownership goal says to add the whole candidate set at once and
//! settle the library questions by trying them. These are those attempts, kept as tests so the answers stay
//! checked rather than becoming a note that rots: each one fails if the crate stops doing what it was picked
//! for.

use std::io::{Read, Write};

/// `alacritty_terminal` is the VT core: bytes in, a screen that can be read back.
///
/// The question D11 settled on lineage and Phase S re-asked on pairing — `termwiz` is from the same project
/// as `portable-pty` — is answered by the registry rather than by taste: **`wezterm-term`, wezterm's actual
/// terminal model, is not published to crates.io**, so the matched pair covers the parser and not the
/// emulation. `termwiz` would mean vendoring or a git dependency for the part that matters.
#[test]
fn the_vt_core_parses_bytes_into_a_readable_screen() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    #[derive(Clone, Copy)]
    struct Size {
        columns: usize,
        screen_lines: usize,
    }
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.screen_lines
        }
        fn screen_lines(&self) -> usize {
            self.screen_lines
        }
        fn columns(&self) -> usize {
            self.columns
        }
    }

    let mut term = Term::new(
        Config::default(),
        &Size { columns: 20, screen_lines: 3 },
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    for byte in b"hello\x1b[1;1H\x1b[31mred" {
        parser.advance(&mut term, &[*byte]);
    }

    let row = &term.grid()[alacritty_terminal::index::Line(0)];
    let line: String = (0..5).map(|column| row[alacritty_terminal::index::Column(column)].c).collect();
    // "hello", then the cursor home and "red": three cells overwritten, the rest left alone
    assert_eq!(line, "redlo", "the screen is readable and a cursor move overwrote the start of the line");
}

/// `command-group` owns a process tree: a group on POSIX, a Job Object on Windows under one API.
///
/// **A dev-dependency, not a dependency, and the reason is a measurement.** The broker's spawn path does not
/// use it: `tests/broker.rs` shows that killing a session reaps its grandchildren on POSIX without it,
/// because the kernel sends SIGHUP to a pty's foreground group when its session leader dies — demonstrated,
/// including with the master kept open, so it is the leader's death that carries it and not the pty's close.
///
/// Windows has no equivalent, which is what X1 and X2 are about, and
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is the answer there — the OS killing the tree when the handle
/// closes. This test keeps the API retired so that change is a wiring job rather than a question, and it
/// moves into `[dependencies]` in the change that can be run on Windows.
#[test]
fn a_child_is_spawned_into_a_group_this_process_owns() {
    use command_group::CommandGroup;

    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30 & echo started; wait"])
        .stdout(std::process::Stdio::piped())
        .group_spawn()
        .expect("a group spawn");

    let mut started = [0u8; 7];
    child.inner().stdout.as_mut().expect("piped stdout").read_exact(&mut started).expect("the child's first line");
    assert_eq!(&started, b"started");

    // Killing the group, not the shell: the `sleep` is the grandchild X1 is about
    child.kill().expect("the group is killable");
    let status = child.wait().expect("the group's exit is reaped");
    assert!(!status.success(), "a killed group does not exit successfully");
}

/// `portable-pty` is the process layer: a pty whose child's output comes back.
#[test]
fn a_pty_runs_a_command_and_its_output_comes_back() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("a pty");
    let mut child = pty.slave.spawn_command(CommandBuilder::new_default_prog()).expect("a shell");
    // **Drop the slave, or the master never sees EOF.** Holding it keeps the slave fd open, so the read loop
    // below blocks for ever after the shell exits instead of ending — which is what it did first, and is the
    // lifetime the broker's session has to get right too.
    drop(pty.slave);
    let mut reader = pty.master.try_clone_reader().expect("a reader");
    let mut writer = pty.master.take_writer().expect("a writer");

    writer.write_all(b"printf apack-terminal-probe\nexit\n").expect("the write reaches the pty");
    drop(writer);

    // Read to EOF, which the shell's exit produces now that nothing else holds the slave
    let mut output = String::new();
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => output.push_str(&String::from_utf8_lossy(&buffer[..read])),
        }
    }
    child.wait().expect("the shell exits");

    assert!(output.contains("apack-terminal-probe"), "the child's output came back: {output:?}");
}

/// `ts-rs` is the protocol's generator, arranged as elmdb's `crates/elmdb-node/protocol/` is.
#[test]
fn a_protocol_type_generates_typescript() {
    use ts_rs::TS;

    #[derive(TS)]
    #[allow(dead_code)]
    struct Spawned {
        session: String,
        pid: u32,
    }

    let declaration = Spawned::decl(&ts_rs::Config::default());
    assert!(declaration.contains("session: string"), "{declaration}");
    assert!(declaration.contains("pid: number"), "{declaration}");
}
