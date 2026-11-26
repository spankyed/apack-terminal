# Docs

What each directory is for, so you know how far to trust what you're reading.

| Directory | Holds | Read it as |
|---|---|---|
| [`architecture/`](architecture/design.md) | how `apack-terminal` v2 works today | current: a change to the code updates it |
| [`archive/`](archive/) | historical v1 investigations and crate evaluations | history: the code and `architecture/` win over it |

## Architecture

- [design.md](architecture/design.md): workspace layering, sharded I/O reactor, sans-IO core, process-tree ownership, `libghostty-vt`, and segmented WAL. Start here.
- [protocol.md](architecture/protocol.md): unified `PROTOCOL_VERSION = 2` binary framing, mutual HMAC-SHA256 auth, multiplexed streams, message catalog, and flow control.
- [views.md](architecture/views.md): differential view replication, `PackedRow` encoding, Canvas 2D & WebGL2 atlas painters, and `@apackdev/terminal-view`.
- [windows-pipe-access.md](architecture/windows-pipe-access.md): Windows named-pipe SDDL security descriptor and owner SID verification.
- [testing.md](architecture/testing.md): sans-IO deterministic simulation (`apack-sim`), mutation testing, ASAN, fuzzing, and cross-platform CI.

[background.md](background.md) explains why the broker exists.
