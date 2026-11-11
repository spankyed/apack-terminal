# Docs

What each directory is for, so you know how far to trust what you're reading.

| Directory | Holds | Read it as |
|---|---|---|
| [`architecture/`](architecture/design.md) | how the broker works today | current: a change to the code updates it |
| [`archive/`](archive/) | questions that are settled, and why | history: the code and `architecture/` win over it |

## Architecture

- [design.md](architecture/design.md): the two layers, who owns a session, and what the broker will not do.
  Start here.
- [protocol.md](architecture/protocol.md): every message, the handshake, the roles, and the closed error set.
- [views.md](architecture/views.md): how a window draws a session from rows it is sent, and the binary
  protocol a view speaks.
- [windows-pipe-access.md](architecture/windows-pipe-access.md): who may open the pipe there, and the
  descriptor read back off a live one.
- [testing.md](architecture/testing.md): what proves it correct, and the command for each check.

[background.md](background.md) is why the broker exists, and
[archive/candidates.md](archive/candidates.md) is which crates were tried and what trying them settled.
