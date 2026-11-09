//! What the candidate crates actually do, tried rather than argued.
//!
//! Phase S of the apack monorepo's terminal-ownership goal says to add the whole candidate set at once and
//! settle the library questions by trying them. These are those attempts, kept as tests so the answers stay
//! checked rather than becoming a note that rots: each one fails if the crate stops doing what it was picked
//! for.
//!
//! **POSIX only.** These drive `/bin/sh` and a default-program pty to establish what each crate does; on
//! Windows the same crates are exercised for real by `tests/broker.rs`, which passes there in full, so a
//! second set of probes would add nothing and the shell differences would be the whole of what they
//! measured. The pty probe in particular sends `\n`, which a Windows console echoes without running
//! (X6), and then waits for an EOF a ConPTY never gives.
#![cfg(unix)]

use std::io::{Read, Write};

/// `libghostty-vt` is the VT core: bytes in, a screen that can be read back.
///
/// **It replaced `alacritty_terminal`, and the question that settled it was not taste.** D11 chose a Rust
/// VT crate because one engine across the broker and the window was unbuyable; `libghostty-vt` 1.0's
/// stable C ABI made it buyable, and Phase 0a measured the two as producing identical grids over every
/// stream in `tests/parser.rs`. The snapshots there were recorded from the old parser and still pass
/// unchanged, which is the evidence rather than this case.
///
/// What this case is for is the same thing every case in this file is for: it fails if the crate stops
/// doing the one thing it was picked for.
#[test]
fn the_vt_core_parses_bytes_into_a_readable_screen() {
    use apack_terminal::vt::Terminal;

    let mut terminal = Terminal::new(20, 3).expect("a terminal");
    // One byte at a time, because a parser that only works on whole writes is the failure this would miss
    for byte in b"hello\x1b[1;1H\x1b[31mred" {
        terminal.feed(&[*byte]);
    }

    let row: String = (0..5)
        .map(|column| {
            let cell = terminal.cell(column, 0).expect("a cell read").expect("a cell");
            char::from_u32(terminal.cell_codepoint(cell).expect("a codepoint")).unwrap_or(' ')
        })
        .collect();
    // "hello", then the cursor home and "red": three cells overwritten, the rest left alone
    assert_eq!(row, "redlo", "the screen is readable and a cursor move overwrote the start of the line");
}

/// `portable-pty` is the process layer: a pty whose child's output comes back.
#[test]
fn a_pty_runs_a_command_and_its_output_comes_back() {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty =
        native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).expect("a pty");
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
