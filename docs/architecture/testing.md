# Testing

```sh
cargo xtask pre-push                           # before a push: every check CI runs, and the change's mutants
cargo nextest run --workspace                  # everything, each test in its own process
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo xtask generate                           # regenerates node/protocol.ts; commit it if it moved
cargo xtask check-generated                    # fails when it has fallen behind
cargo xtask check-exclusions                   # fails when a mutants exclusion excludes nothing
```

## What each suite is for

| Suite | Subject |
|---|---|
| `tests/broker.rs` | the broker driven as a consumer drives it: a real socket, the real binary |
| `tests/parser.rs` | the VT core as a parser — grid snapshots, and the byte-boundary property |
| `tests/markers.rs` | the OSC scanner, including every chunk boundary |
| `tests/frames.rs` | the frame decoder as a decoder — arbitrary bytes, mutated frames, the round trip |
| `tests/screen.rs` | what a snapshot answers: the viewport, the history asked for, the alternate screen |
| `tests/channel.rs` | the bounded output channel and what an overload reports |
| `tests/coalescing.rs` | output gathered into one event rather than one per read |
| `tests/security.rs` | the pipe's descriptor as a string, on every platform |
| `tests/candidates.rs` | the crates that were tried, kept as tests so the answers stay checked |

`src/lib.rs` exists so the parser can be tested as a parser and the decoder as a decoder. Neither the grid,
the scanner nor the bytes a stranger sends is a thing you write through a pty and a socket.

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

## The bytes a stranger sends

The frame decoder (`src/frames.rs`) is the only part of the protocol an unknown sender reaches: everything
else in the broker runs on a `Request` that already parsed. A panic there ends every terminal the app has
open, so the claim is not that a bad frame is reported well — it is that **no byte string gets anything
other than one of five answers**, which is a property rather than a list of the malformed frames whoever
wrote the test thought of.

Three strategies, in increasing order of what they find:

- **Arbitrary bytes**, which cover the reader: the cap, a newline that never comes, UTF-8 that is not.
- **A valid frame with one byte changed**, which is the half that reaches the deserializer in a state it has
  to decide about — `"kind":"spaw"`, a number where a string belongs, a string cut off mid-escape. Random
  bytes are never nearly-valid JSON, so without this the deserializer is barely exercised.
- **Every request serialized and read back**, which is the app's half of the seam: a `Serialize` and a
  `Deserialize` that disagree is a frame the app can send and the broker cannot read, and no amount of
  fuzzing the broker's side would show it. `node/protocol.ts` is generated from the same derives.

Beside them the cap's boundary is asserted by example, because it is one byte and not a distribution: a
frame whose line is exactly `MAX_FRAME_BYTES` is read, and one byte longer is refused. That is what the
reader's `take(cap + 1)` is for, and nothing had asserted it.

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

## A test runs in its own process, and has a deadline

`cargo nextest run` runs each test in a process of its own, so a test that has stopped ending is killed at
its deadline and the rest run on. That matters more here than in most suites: nearly every case waits for a
frame the broker sends *when something happened*, so the shape of a lost signal is a `read` that never
returns rather than a wrong answer — and under `cargo test` one such case holds the whole run.

The deadlines are in [`.config/nextest.toml`](../../.config/nextest.toml). Each is several times the slowest
reading rather than near it, because what it is for is a test that has stopped ending at all: the whole
suite is 8.5 s and its slowest case 7.2 s on an Apple-silicon laptop (55 tests, 2026-11-08), against a
default deadline of 20 s. **A reading, never a bound** — a slower machine reports, it does not fail.

## Mutation testing

**`src/tree.rs` is answered on Windows, and only there.** It is `#[cfg(windows)]` in its entirety, so a
Mac or a Linux run compiles none of it and cannot kill any of its mutants — which is why it is excluded
from no run and why `mutants.yml` takes a dispatch: `gh workflow run mutants.yml -f files=src/tree.rs`.
The first such run, 2026-11-09, tested 7 and reported 5 caught, 1 unviable and 1 missed. The missed one is
the POSIX arm of `Tree::owning`, which Windows does not compile and which does not build off Windows
either, and it is excluded by name with that reading.

**Every exclusion excludes something**, which `cargo xtask check-exclusions` asks cargo-mutants rather than
deciding for itself (`--re` filters by the same matching `--exclude-re` uses). A pattern that matches no
mutant is worse than no pattern: the gate goes on reporting the survivor it was written for while the comment
above it reads as a settled question. It has happened twice — `security.rs`'s patterns were written as
`in <fn>`, which cargo-mutants prints only for a mutant inside a function, and `tree.rs`'s was written with
one backslash, which is not a TOML escape and stopped the file parsing at all. Both were found by CI rather
than by the author. `xtask/tests/exclusions.rs` drives the check's decision with counts of its own, so it can
be watched failing without a dead pattern committed to the file it reads.

`cargo xtask pre-push` ends with the change's mutants: cargo-mutants edits each line the change touched into
something that still compiles, and a mutant the suite still passes is a line no case holds. The run is
scoped to the change (`--in-diff`) rather than the repository, because a survivor nobody edited is not this
push's to answer for, and a whole-repository run is minutes of them.

A survivor is answered one of two ways, and never left:

- **killed** — the case that should have caught it is written, which is the normal answer.
- **excluded** — in [`.cargo/mutants.toml`](../../.cargo/mutants.toml) with the reason, for a mutant that
  answers exactly what the code answers. There is one: `sendable`'s ASCII arm returns 1 and so does the `_`
  fallback, so deleting the arm changes no answer. It is kept because it says ASCII is one byte, where `_`
  says *not a lead byte at all* — removing it to kill the mutant would make that comment wrong.

`.github/workflows/mutants.yml` runs the same thing on a pull request and fails on a survivor. It runs twice:
Windows-only code compiles on no other platform, so a Linux run reports every mutant of `src/tree.rs` and of
every `cfg(windows)` arm as unviable, and the Windows job is where those are actually tested.

## The suite runs on all three platforms

[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) runs the whole suite on Linux, macOS and
Windows, and the cheap checks — format, clippy, the generated protocol, the doc tests — on Linux alone.
Three platforms is the point rather than thoroughness: a pty, a process tree and a shell's own reports are
each the platform's answer, and the rule above holds for every one of them.
