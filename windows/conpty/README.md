# ConPTY, as the broker ships it on Windows

`conpty.dll` and `OpenConsole.exe` from Microsoft's `Microsoft.Windows.Console.ConPTY` package, version
1.25.261109001, attached to the Windows Terminal release
[v1.25.3131.0](https://github.com/microsoft/terminal/releases/tag/v1.25.3131.0). MIT licensed, copyright
Microsoft Corporation (`LICENSE`, the license of `microsoft/terminal` at that tag).

**Why the broker carries its own.** The ConPTY built into Windows drops what it does not draw itself before a
terminal sees it: the graphics APC (`ESC _`) a program places an image with, the mouse-mode request (`DECSET
1002`, `1006`) and a program's question about the clipboard (`OSC 52 ;?`). The broker is then never told an image
was placed or that a program wants the mouse. This package's ConPTY passes all three through. Read off a
session's raw output on Windows Server 2025 (build 26100), 2026-11-15: the system's carried none of them and
this one carried each.

**How it is found.** `portable-pty` loads a `conpty.dll` that sits beside the executable in preference to the
system's, and that `conpty.dll` starts the `OpenConsole.exe` beside it. So the two files go wherever the broker
binary goes: `build.rs` puts them beside the one `cargo` builds, and `binaries.yml` publishes them beside the
Windows binary it releases. Each directory here is one architecture's; the build takes the target's.

| File | SHA-256 |
|---|---|
| package `Microsoft.Windows.Console.ConPTY.1.25.261109001.nupkg` | `2d4f846e9b59ab85977d4d15022c696b36e5400f6320608be1c75d3d0195fee7` |
| `x64/conpty.dll` | `e971856f89748dc3b458125e9afdd79b5e9574a7c937676116d3cbbd3b71f492` |
| `x64/OpenConsole.exe` | `b17e48b07ef153b420f0a6e178bacd31fb4c3390e21ae746586213fc0719a775` |
| `arm64/conpty.dll` | `3608cef830a1c4fbbf3981594ad3f64ebe2df6db98fe43a48a4ca7d2875d8959` |
| `arm64/OpenConsole.exe` | `c04334f450b5731930b63a28ac4da8a058367242dbcf2fb9b323ea25e5a23252` |

The package's hash is the one GitHub publishes for the release asset. The files are the package's
`runtimes/win-<arch>/native/conpty.dll` and `build/native/runtimes/<arch>/OpenConsole.exe`, unchanged.
