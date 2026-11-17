# What two agents sent over OSC 7501

Each file is the body of every OSC 7501 sequence one agent wrote during a recorded session, in order, one
per line: what follows `ESC ] 7501 ;` and precedes the terminator. A line that is `?` is the agent asking
whether the terminal supports the protocol. Nothing else of the session is here, and nothing else may be
added: a screen's bytes hold paths and account details.

| File | Session |
|---|---|
| `codex-0.167.0.txt` | Codex started twice. First: a prompt it answered with no tool, then one whose command its sandbox refused. Second, with a read-only sandbox: a prompt whose command needed approval, which was denied. Each start ends in `state=clear` at exit. Codex asked no support query. |
| `claude-code-2.1.326.txt` | Claude Code, signed out. It asked the support query and was answered, reported `idle`, took a prompt it could not send and reported `blocked` with `kind=auth` and a message, then exited. |

`tests/status.rs` feeds each through a screen and holds the records it ends with.
