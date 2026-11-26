# Who may open the broker's pipe on Windows

On a Unix system the broker's socket is a file in the app's own data directory and the filesystem has
already answered this. A Windows named pipe has not: it carries a security descriptor, and **the default one
a pipe gets grants read access to Everyone**, the anonymous logon included. What crosses this pipe is every
byte the user's shells print and every key they type into them, so the default is wrong here in a way that
fails nothing and shows nothing.

So the listener is created with a descriptor of its own (`crates/apack-os/src/security_windows.rs`), and a descriptor that cannot be
built refuses the listen rather than falling back — falling back would mean serving terminal output to every
local account because something about the token could not be read.

## What the code asks for

`sddl_for_user_sid(sid)` is the whole of the decision, as SDDL:

```
O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)
```

The owner is the account that started the broker; the DACL is protected, so nothing is inherited into it;
that account and `SYSTEM` may do everything. **Everyone else is denied by absence**, which is what makes the
`P` load-bearing, and `crates/apack-os` unit tests verify the SDDL builder on every platform — the broadest ACE in
Windows should not be one edit away on the one platform most developers here cannot run.

## What the object actually holds

Read off a live pipe on the Windows development VM, 2026-11-09, with
`GetNamedSecurityInfoW(SE_FILE_OBJECT, OWNER | GROUP | DACL)` and
`ConvertSecurityDescriptorToStringSecurityDescriptorW`, against a broker started from
`target\debug\apack-terminal.exe \\.\pipe\apack-acl-check`:

```
O:LAG:S-1-5-21-…-513D:P(A;;FA;;;LA)(A;;FA;;;SY)
```

The starting account is `S-1-5-21-…-500`. So, term by term:

| what | asked | held | |
|---|---|---|---|
| owner | `S-1-5-21-…-500` | `LA` | the same account; `LA` is SDDL's abbreviation for it |
| DACL | protected, two allow ACEs | protected, two allow ACEs | |
| the grants | `GA` | `FA` | the same access: `GA` is a *generic* right, which the object resolves against its own mapping, and a file object's name for all of it is `FA` |
| group | the user's SID | the token's primary group | **the descriptor's group does not reach the object** |

**This is the measurement that says the descriptor reaches the kernel at all**, which no string test can
show: `interprocess`'s `security_descriptor` is taken on trust until something reads the object back. It
does, and nothing but the starting account and `SYSTEM` is granted anything.

**The group was dropped because of this reading, and the same reading taken again proves the drop changed
nothing.** With `G:` gone from `access_for`, a rebuilt broker's pipe reports the same owner, the same two
ACEs and the same `…-513` group — so the group on the object was never ours to set. Nothing referenced it
either: no ACE names it, and the pipe is inherited into by nothing. A descriptor should not say things the
object will not hold.

## A second account

**Refused in every direction.** On the development VM, 2026-11-15, a throwaway local account (not an
administrator) tried the pipe of a broker the first account had started, through .NET's
`NamedPipeClientStream`: for reading and writing, for reading alone and for writing alone, each was
`Access to the path is denied`. That is the DACL above doing what Windows' semantics say it does, now seen.

**What a second account does add is the name.** A pipe's name is one namespace for the whole machine, and any
account may create a pipe under a name nobody holds yet — with a descriptor that lets this user in. A client
connecting by name then reaches the other account's process, and the DACL on *our* pipe has nothing to say about
it. So a client or proxy asks whose the pipe it reached is before it carries a byte (`crates/apack-os/src/transport_windows.rs`,
`verify_pipe_owner`), as it asks the kernel for the peer's uid on POSIX (`crates/apack-os/src/transport_posix.rs`).
An owner is the creator or a SID the creator holds, so an ordinary account cannot make a pipe that answers with
ours. In the same run the second account created such a pipe, open to Everyone, and the first account's proxy was
pointed at it: the proxy refused and the other account's pipe received 0 bytes. Reading that owner is verified
in `crates/apack-os` on Windows against a pipe created with the broker's own security descriptor.
