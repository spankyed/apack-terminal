# The licenses of code no crate's metadata names

The license files here are the notices of code compiled into the release binaries whose crates don't carry them,
copied verbatim; `cargo xtask notices` appends them to `THIRD-PARTY-NOTICES` after the crates' (`xtask/src/notices.rs`,
`BUNDLED`), with ConPTY's (`windows/conpty/LICENSE` and `NOTICE`). Read on 2026-11-17.

## libghostty-vt

`ghostty-vt-rs-sys` links libghostty-vt, a static library compiled with Zig from Ghostty's code and the libraries
Ghostty builds it with, which the linker draws on for every release binary; a platform's linker may leave out a part
of it, such as `compiler_rt` on macOS, and the notices list it whole, crediting more than is shipped, never less.
Ghostty's own license is shipped by `ghostty-vt-rs-sys` and listed with the crates.

**Which libraries.** Those Ghostty's `deps/libghostty-vt-third-party.json` names as `linked`, at Ghostty
`0fa3d577a5614150ff8e2c152de5fb9d5c6d2dec`, the commit `ghostty-vt-rs-sys` 0.3.0 builds and links (`GHOSTTY_COMMIT` in its
`build.rs`, which `cargo xtask notices` reads). The macOS release links the archive its upstream builds with Zig 0.17.0
(`ghostty-vt-rs`'s `.github/workflows/prebuilt.yml`); the Linux and Windows releases build it from that commit's sources
with Zig 0.17.0 (`binaries.yml`). So Zig's standard library and `compiler_rt` are compiled in too, whose math is in part
ported from musl's and Go's, and Ghostty's own code holds two tables taken from elsewhere: X11's `rgb.txt`
(`src/terminal/x11_color.zig`) and Foot's Kitty keymap (`src/input/kitty.zig`). `cargo xtask notices` fails when the
crate links another commit, until these are read again for it.

| File | What | From | SHA-256 |
|---|---|---|---|
| `zig-LICENSE` | Zig 0.17.0's standard library and `compiler_rt` | `codeberg.org/ziglang/zig`, tag `0.17.0`, `LICENSE` | `5c537d6853e005298a285d508cff9ac7192cea23576c840d485b2b586a7ff177` |
| `translate-c-LICENSE` | translate-c 2.0.0 | `codeberg.org/ziglang/translate-c` at `02ff0c523fb92a38939b04d0145c4a9602a74d56`, `LICENSE` | `5c537d6853e005298a285d508cff9ac7192cea23576c840d485b2b586a7ff177` |
| `highway-LICENSE-BSD3` | Highway 1.2.0, `Apache-2.0 OR BSD-3-Clause`, taken under BSD-3-Clause | `github.com/google/highway` at `66486a10623fa0d72fe91260f96c892e41aceb06`, `LICENSE-BSD3` | `d25e82e26acd42ca3ccc9993622631163425b869b9e16284226d534cff6470f2` |
| `simdutf-LICENSE-MIT` | simdutf 9.0.0, `Apache-2.0 OR MIT`, taken under MIT; vendored in Ghostty | Ghostty at the commit above, `pkg/simdutf/vendor/LICENSE-MIT` | `fc8dbc04e03ad4efc08a647ffe7f995b811a95bc04c0e85a56d5277c6593fa5f` |
| `wuffs-LICENSE-MIT` | Wuffs 0.4.0-alpha.10, `Apache-2.0 OR MIT`, taken under MIT | `github.com/google/wuffs-mirror-release-c` at `7411f488fe2e2c205c3d3b3d28638b7356522930`, `LICENSE-MIT` | `bb811a4b485eac3caeb8bdc58bbafb355fbcb6116010a7ea9179e5c70b0bc878` |
| `uucode-LICENSE` | uucode 0.2.0, `MIT AND Unicode-3.0` | `github.com/jacobsandlund/uucode` at `1fb73433bba5d93366c57f23ff2e9d7939746500`, `LICENSE.md` | `312e901e142be2477b4ca859e9311f9e3f80d33372991759b7921c1893605f33` |
| `uucode-LICENSE_Bjoern_Hoehrmann` | uucode's UTF-8 decoder | the same, `licenses/LICENSE_Bjoern_Hoehrmann` | `de219cece932aad5a817bf763393d8d149d378a15d2ad5320e3331eac07626dd` |
| `uucode-LICENSE_unicode` | uucode's Unicode data | the same, `licenses/LICENSE_unicode` | `1eda5a3b026870c737b22e8bcd4954338612c790db688242e003f41a4fa95175` |
| `musl-COPYRIGHT` | musl's notices, for the math Zig's `compiler_rt` and standard library port from it | `codeberg.org/ziglang/zig`, tag `0.17.0`, `lib/libc/musl/COPYRIGHT` | `f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af` |
| `go-LICENSE` | Go's license, for `std/math/pow.zig`, ported from Go | `github.com/golang/go`, `master`, `LICENSE` | `911f8f5782931320f5b8d1160a76365b83aea6447ee6c04fa6d5591467db9dad` |
| `x11-rgb-COPYING` | X11's `rgb.txt`, which Ghostty embeds | `gitlab.freedesktop.org/xorg/app/rgb`, `master`, `COPYING` | `eb3b0a2c9810a0a364cd813377713a223651c17ae8be97c62c2e5d680f71f843` |
| `foot-LICENSE` | Foot's, whose Kitty keymap Ghostty ports | `codeberg.org/dnkl/foot`, `master`, `LICENSE` | `d534a23a31500a0ac958d9634b84f532bd73ff1aca1bb8f7debbcbebc16ff39a` |

The copies are named for what they are, and uucode's `LICENSE.md` is kept without its extension: CI reads a change
to `.md` files alone as documentation and runs no check, which an edit here would need.

Zig's `compiler_rt` also holds code ported from LLVM's, under `Apache-2.0 WITH LLVM-exception`, whose exception
lifts the notice requirement for code compiled into a binary.

## Crates ported from other code

| File | What | From | SHA-256 |
|---|---|---|---|
| `zmij-LICENSE` | Victor Zverovich's zmij, of which the `zmij` crate, serde_json's float formatting, is a line-by-line port | `github.com/vitaut/zmij`, `main`, `LICENSE` | `a69b36adb8d116376bcd580772fa45f08fffecc17833cb9822532f4e4f5ec372` |

## ConPTY

`windows/conpty/LICENSE` is Microsoft's license for the package; `windows/conpty/NOTICE` is `NOTICE.md` of
`microsoft/terminal` at `v1.25.3131.0`, the tag the package is attached to, which names the third-party code built
into conhost and OpenConsole. It is kept without its extension for the reason uucode's license is. SHA-256:
`e7fbaadee6ab20c28b87730a510ee5f5815d8fb4bd88d1d54d282dc2a74c0726`.
