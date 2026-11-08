# Testing

```sh
cargo test                                     # everything
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
TS_RS_EXPORT_DIR=protocol cargo test           # regenerates protocol/protocol.ts; commit it if it moved
```

## What each suite is for

| Suite | Subject |
|---|---|
| `tests/broker.rs` | the broker driven as a consumer drives it: a real socket, the real binary |
| `tests/parser.rs` | the VT core as a parser — grid snapshots, and the byte-boundary property |
| `tests/markers.rs` | the OSC scanner, including every chunk boundary |
| `tests/channel.rs` | the bounded output channel and what an overload reports |
| `tests/candidates.rs` | the crates that were tried, kept as tests so the answers stay checked |

`src/lib.rs` exists so the parser can be tested as a parser. Neither the grid nor the scanner is a thing you
write through a pty and a socket.

## The property that catches what examples do not

Output arrives in whatever sizes a `read` returned, so an escape sequence, a UTF-8 code point or an OSC
string is routinely delivered in pieces — and that is where VT parsers break.

So both parsing suites **feed the same stream at every boundary and demand the same answer**: exhaustively
one byte at a time, at every single cut, and at arbitrary cuts with `proptest`. A wrapper that buffered a
whole chunk, or reset anything per call, passes every test that feeds a stream whole and fails these.

The streams carry UTF-8, CSI, OSC, DCS, APC, an alternate-screen round trip and a scroll region, because
each is parsed by a different part of the state machine.

The first snapshot of the UTF-8 stream is what found the one defect in the grid read: a wide character
occupies two cells and the second holds a space, so the row read as text said `日 本 語`. Right to draw,
wrong to match against — and matching is what a bounded read is for.

## Snapshots

`insta` holds the grid a stream produces, so a change in what the parser does to a stream is a reviewable
diff rather than an assertion someone has to word. `cargo insta review` accepts or rejects one.

## Killing a session's tree

`killing_a_session_takes_the_whole_tree_with_it` spawns a grandchild that outlives its shell and asks the OS
whether it is gone — with a pid captured at spawn, never by matching a process by name.

On Windows that is the Job Object doing it (`tree.rs`); on POSIX it is the kernel's SIGHUP to the pty's
foreground group. **The Windows half is claimed on a Windows run or not at all**, which is why the suite
runs there rather than being reasoned about from a Mac.

## Two cases are POSIX-only, and say why

`cmd.exe` has no `printf` and no way to write an escape byte that is not also echoed, so the Windows form of
a marker case would assert the echo rather than the shell's report. PowerShell can, and arranging markers
there is work that belongs to a Windows session.

## A disconnect has to be made to happen

Dropping the test's handles does not reliably close a connection: the events reader holds a clone of the
socket and is blocked in `read`, so it cannot notice its channel is gone until something wakes it. A case
that drained the output first and then dropped would wait for ever for a broker that was behaving
correctly.

`disconnect` wakes it on purpose — the receiver goes, then one malformed frame the broker is certain to
answer. This is worth knowing before writing a case about a client going away.
