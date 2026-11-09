# Who may open the broker's pipe on Windows

On a Unix system the broker's socket is a file in the app's own data directory and the filesystem has
already answered this. A Windows named pipe has not: it carries a security descriptor, and **the default one
a pipe gets grants read access to Everyone**, the anonymous logon included. What crosses this pipe is every
byte the user's shells print and every key they type into them, so the default is wrong here in a way that
fails nothing and shows nothing.

So the listener is created with a descriptor of its own (`src/security.rs`), and a descriptor that cannot be
built refuses the listen rather than falling back — falling back would mean serving terminal output to every
local account because something about the token could not be read.

## What the code asks for

`access_for(sid)` is the whole of the decision, as SDDL:

```
O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)
```

The owner is the account that started the broker; the DACL is protected, so nothing is inherited into it;
that account and `SYSTEM` may do everything. **Everyone else is denied by absence**, which is what makes the
`P` load-bearing, and `tests/security.rs` reads the string back on every platform — the broadest ACE in
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

## What a second account would add

Nothing that this does not already settle. The DACL is protected and names two accounts, so any third is
denied by Windows' own semantics rather than by anything in this repository; an account created to be
refused would be testing `AccessCheck`. What was worth measuring was whether our descriptor is the one on
the object, and that is the reading above.
