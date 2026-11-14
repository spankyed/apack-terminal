# apack-terminal

A terminal broker: one process that owns ptys and their process trees, parses VT, and answers a protocol —
so that no terminal session's lifetime is tied to the process that asked for it.

Written for [apack](https://github.com/spankyed/apack), whose backend is a separate Node process. A session
survives that process restarting, and ends when the app that claimed it goes.

The repository also holds what draws a session in a window: [`view/`](view) is the `apack-terminal-view`
package, a view over the rows the broker sends, with no terminal core of its own. The broker interprets
and the view paints, so they are the two halves of one protocol and change in one commit.

| | |
|---|---|
| `src/`, `tests/`, `xtask/` | the broker, in Rust |
| [`node/`](node) | the `apack-terminal` npm package: where the broker's binary is, and the protocol's generated types |
| [`view/`](view) | the `apack-terminal-view` npm package, in TypeScript ([its README](view/README.md)) |

- **Sessions outlive their client.** The connection that opened a session is not what ends it; whoever
  claimed the broker's lifetime is, and a descriptor closes on process death however the process died.
- **Owned process trees.** A Job Object on Windows, the kernel's own signal on POSIX — one mechanism per
  platform rather than cleanup code that has to run.
- **One authority on the screen.** The broker parses the same bytes it forwards, so a session nobody is
  drawing still answers a bounded read, a search and a wait.
- **Shell facts, not guesses.** OSC 633 markers give the working directory, command boundaries and exit
  status; what a shell did not say is reported as unknown rather than inferred.
- **Marks that can be vouched for.** A session spawned with a key takes its commands only from marks that
  carry it, so nothing printed into the terminal — a file, a log, a web response — can report a command
  as ended or as having succeeded. A status a keyed mark did not give is told as a claim, never as an
  exit.
- **Bounded output with real backpressure.** Reads are coalesced, the channel is credit-bounded, and an
  overload is reported with the microseconds it waited rather than being silent.

## The protocol

Two, on one socket. A client that opens and ends sessions speaks newline-delimited JSON: Rust is the source
of truth and `node/protocol.ts` is generated from it and shipped by the npm package, so a consumer imports
the types without building Rust. A view that draws a session speaks length-prefixed binary after the same
greeting.

```jsonc
→ {"kind":"hello","protocol":1,"role":"client"}
← {"kind":"hello","protocol":1,"history":1000}
→ {"kind":"spawn","cols":80,"rows":24,"shell":null,"args":[],"env":[],"unset":[],"cwd":null}
← {"kind":"spawned","session":"s-1","pid":4242,"recorded":false,"restored":false}
→ {"kind":"write","session":"s-1","text":"echo hi\n"}
← {"kind":"output","session":"s-1","text":"echo hi\r\nhi\r\n"}
→ {"kind":"facts","session":"s-1"}
← {"kind":"facts","session":"s-1","cwd":"/home/me","command":"echo hi","running":false,"outcome":{"kind":"claimed","code":0},"finished":1,"vouched":false,"refused":0}
```

[protocol.md](docs/architecture/protocol.md) has every message, the roles, and the closed error set.

A window draws a session over a connection of its own, greeting as a `view`: it is sent the rows that
changed and paints them, interpreting nothing ([views.md](docs/architecture/views.md)). What paints them
is the `apack-terminal-view` package in [`view/`](view), whose codec (`view/src/broker/wire.ts`) is the
other half of `src/view/wire.rs`. The two meet in `node/view-vectors.json`: the broker's suite writes every
kind of message there as bytes and as the value they mean, and the view's suite reads that file where it
lies, so a change to the wire that taught only one half fails.

`apack-terminal proxy [<socket>]` is a process's stdin and stdout as a connection to the broker on that
socket, started first if there is none — so `ssh host apack-terminal proxy` is a connection to a broker
on another machine. With no socket named it is the user's own: `$XDG_RUNTIME_DIR/apack-terminal/broker.sock`
where the system gives a runtime directory, and `~/.apack-terminal/broker.sock` otherwise. A socket goes
in a directory that is its user's alone: one other users can write to, `/tmp` among them, is refused.

## Status

Built for apack and not published, so build it from source. Nothing here is a stable API yet: the protocol
version is 1 and is edited in place until a release has users.

```sh
git clone https://github.com/spankyed/apack-terminal.git && cd apack-terminal
cargo build
cargo test
npm --prefix view ci && npm --prefix view test    # the view
```

| | |
|---|---|
| Rust | pinned by `rust-toolchain.toml` |
| Node | 24.12 or later, for the view |
| Platforms | macOS and Linux: the whole suite. Windows: most of it, with the POSIX-only cases gated and named in `tests/` (see [Known limits](docs/architecture/design.md#known-limits)) |
| Transport | a Unix socket, or a named pipe on Windows |
| Crash | a session ends when the broker stops; there is no handoff of a session to another broker |
| Not in scope | agent status rules, command semantics beyond what a shell reports, storage, a recording or a path link on a remote target, `signal` on Windows, accessibility |

Consumed by pinning a commit: apack keeps a gitignored `vendor/apack-terminal` symlink beside a committed
`vendor/apack-terminal.rev`, and derives one Rust toolchain across every checkout linked that way. One pin
holds the broker and the view, which is why they are one repository.

**The binary reaches a consumer as an npm package.** `node/` is that package, and its one job is answering
where the binary is — the name differs by platform, and a consumer that derived the path itself was wrong
about `apack-terminal.exe` and started no terminal on Windows. `cargo xtask package` stages a release build
into it, beside the generated protocol types; apack installs it as `file:…/vendor/apack-terminal/node`, so
its packaged app collects the binary with every other dependency rather than needing a rule of its own, and
imports the protocol's types from the package rather than keeping a copy of them.

```js
const { brokerBinary } = require('apack-terminal');
brokerBinary(); // the path, or undefined when this checkout has not staged one
```

**The view reaches a consumer the same way**, as `file:…/vendor/apack-terminal/view`. What it ships is its
compiled `dist/`, which is not tracked, so whatever links or clones the checkout builds it
(`npm run build` in `view/`): apack's linker does when `dist/` is missing or older than its sources, and
its CI does after the clone.

**v1 scope.** What v1 is: a broker that owns processes and screens, serves views over a protocol, records
sessions and restores them from their recordings, keeps sessions the app asked to keep, and reports what a shell
said of its commands, where the shell vouched for it. What v1 is not is said in the table above. Its decisions
(scope, platforms, the crash behaviour, the echo flush) are recorded where they were taken, and each limit a
platform has is stated beside the code it limits, in [Known limits](docs/architecture/design.md#known-limits).

## Docs

See the [docs map](docs/README.md). The ones most people want:

- [design.md](docs/architecture/design.md): the two layers, who owns a session, and what the broker won't do
- [protocol.md](docs/architecture/protocol.md): every message, the handshake, the roles, the errors
- [views.md](docs/architecture/views.md): how a window draws a session, and the protocol a view speaks
- [testing.md](docs/architecture/testing.md): what proves it correct, and the command for each check
- [background.md](docs/background.md): why the broker exists

## Develop

Needs Rust, which `rust-toolchain.toml` pins, and Node for the view. `CLAUDE.md` has the repository's rules.

```sh
cargo xtask pre-push                         # before a push: every check CI runs, and the change's mutants
cargo nextest run --workspace                # every suite, each test in its own process
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo xtask generate                         # regenerate the protocol's TypeScript; commit it if it moved
cargo xtask package                          # release build, staged into the npm package in node/
cargo run -- ~/.apack-terminal/broker.sock   # run one, and talk to it
npm --prefix view test                       # the view's suite (also: run typecheck, run build)
```

`cargo xtask pre-push` is the one to run before a push: it is every check
[CI](.github/workflows/ci.yml) fails a change on, cheapest first, and it refuses a dirty tree because what
is checked has to be what is pushed. A mutant of the change that survives is killed, or excluded in
`.cargo/mutants.toml` with the reason. The view's types, suite and build are among its steps, after the
broker's suites, since the view's suite reads the vectors those hold to the encoder.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). The view is MIT
([`view/LICENSE`](view/LICENSE)), which carries the notice of
[coder/ghostty-web](https://github.com/coder/ghostty-web), whose input handling it draws on.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work,
as defined in the Apache-2.0 license, is dual-licensed as above, without any additional terms or conditions.
