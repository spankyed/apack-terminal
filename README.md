# apack-terminal

A terminal broker: one process that owns ptys and their process trees, parses VT, and answers a protocol —
so that no terminal session's lifetime is tied to the process that asked for it.

Written for [apack](https://github.com/spankyed/apack), whose backend is a separate Node process. A session
survives that process restarting, and ends when the app that claimed it goes.

- **Sessions outlive their client.** The connection that opened a session is not what ends it; whoever
  claimed the broker's lifetime is, and a descriptor closes on process death however the process died.
- **Owned process trees.** A Job Object on Windows, the kernel's own signal on POSIX — one mechanism per
  platform rather than cleanup code that has to run.
- **One authority on the screen.** The broker parses the same bytes it forwards, so a session nobody is
  drawing still answers a bounded read, a search and a wait.
- **Shell facts, not guesses.** OSC 633 markers give the working directory, command boundaries and exit
  status; what a shell did not say is reported as unknown rather than inferred.
- **Bounded output with real backpressure.** Reads are coalesced, the channel is credit-bounded, and an
  overload is reported with the microseconds it waited rather than being silent.

## The protocol

Newline-delimited JSON. Rust is the source of truth and `node/protocol.ts` is generated from it and shipped
by the npm package, so a consumer imports the types without building Rust.

```jsonc
→ {"kind":"hello","protocol":1,"role":"client"}
← {"kind":"hello","protocol":1,"history":1000}
→ {"kind":"spawn","cols":80,"rows":24,"shell":null,"args":[],"env":[],"unset":[],"cwd":null}
← {"kind":"spawned","session":"s-1","pid":4242}
→ {"kind":"write","session":"s-1","text":"echo hi\n"}
← {"kind":"output","session":"s-1","text":"echo hi\r\nhi\r\n"}
→ {"kind":"facts","session":"s-1"}
← {"kind":"facts","session":"s-1","cwd":"/home/me","command":"echo hi","running":false,"exit_status":0,"finished":1}
```

[protocol.md](docs/architecture/protocol.md) has every message, the roles, and the closed error set.

## Status

Built for apack and not published, so build it from source. Nothing here is a stable API yet: the protocol
version is 1 and is edited in place until a release has users.

```sh
git clone https://github.com/spankyed/apack-terminal.git && cd apack-terminal
cargo build
cargo test
```

| | |
|---|---|
| Rust | pinned by `rust-toolchain.toml` |
| Platforms | macOS, Linux and Windows |
| Transport | a Unix socket, or a named pipe on Windows |
| Not in scope | agent status rules, command semantics beyond what a shell reports, storage |

Consumed by pinning a commit: apack keeps a gitignored `vendor/apack-terminal` symlink beside a committed
`vendor/apack-terminal.rev`, and derives one Rust toolchain across every checkout linked that way.

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

## Docs

See the [docs map](docs/README.md). The ones most people want:

- [design.md](docs/architecture/design.md): the two layers, who owns a session, and what the broker won't do
- [protocol.md](docs/architecture/protocol.md): every message, the handshake, the roles, the errors
- [testing.md](docs/architecture/testing.md): what proves it correct, and the command for each check
- [background.md](docs/background.md): why the broker exists

## Develop

Needs Rust, which `rust-toolchain.toml` pins. `CLAUDE.md` has the repository's rules.

```sh
cargo xtask pre-push                         # before a push: every check CI runs, and the change's mutants
cargo nextest run --workspace                # every suite, each test in its own process
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo xtask generate                         # regenerate the protocol's TypeScript; commit it if it moved
cargo xtask package                          # release build, staged into the npm package in node/
cargo run -- /tmp/apack-terminal.sock        # run one, and talk to it
```

`cargo xtask pre-push` is the one to run before a push: it is every check
[CI](.github/workflows/ci.yml) fails a change on, cheapest first, and it refuses a dirty tree because what
is checked has to be what is pushed. A mutant of the change that survives is killed, or excluded in
`.cargo/mutants.toml` with the reason.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work,
as defined in the Apache-2.0 license, is dual-licensed as above, without any additional terms or conditions.
