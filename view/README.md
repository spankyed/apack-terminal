# @apackdev/terminal-view

Thin TypeScript terminal renderer for `apack-terminal` v2: paints style-run rows, Kitty/Sixel images,
decorations, selection, links, search highlights, and command gutters without running a terminal emulator.

A package of the [`apack-terminal`](..) repository alongside the Rust broker. The broker's `libghostty-vt`
core is the sole VT authority; `@apackdev/terminal-view` interprets zero VT bytes and communicates over the
v2 multiplexed binary protocol (`PROTOCOL_VERSION = 2`).

```ts
import { TerminalClient, TerminalView } from '@apackdev/terminal-view'

const client = await TerminalClient.connect(stream, { authToken, clientName: 'app' })
const source = await client.openView(sessionId)
const view = new TerminalView(source, { font, theme }) // auto-selects WebGL2 with Canvas 2D fallback
view.mount(element)
```

From this directory, on Node 24.12 or later:

```sh
npm ci
npm run typecheck
npm test
npm run build        # dist/, which is what a consumer bundles
```

## Architecture & Modules

| Path | What it is |
|---|---|
| `src/wire/` | canonical LEB128 + binary v2 codec (`codec.ts`, `frame.ts`, `auth.ts`, `messages.ts`, `types.ts`) matching `crates/apack-proto` and verified against `node/wire-vectors-v2.json` |
| `src/broker/client.ts` | multiplexed `TerminalClient` and `BrokerSessionSource` (mutual HMAC-SHA256 handshake, `spawn`, `attach`, credit/BDP `FrameAck`s, `SearchWatch`, `getImageBlob`, leases, signals) |
| `src/source.ts` | pure `TerminalSource` contract so views can be driven by a live broker, WAL playback, or unit tests |
| `src/row-store.ts`, `src/viewport.ts`, `src/local-query.ts` | bounded line-indexed row cache, scrollback paging, local word/line queries, and epoch invalidation |
| `src/draw.ts`, `src/painter.ts` | row-to-scene preparation and the pluggable `Painter` interface |
| `src/canvas-painter.ts` | `CanvasPainter`: 2D canvas renderer with row-shift scroll blitting and image compositing |
| `src/webgl-painter.ts` | `WebGL2AtlasPainter`, `ShelfPacker`, and `createPainter` (`'auto'` prefers WebGL2 with seamless fallback to Canvas 2D) |
| `src/input.ts`, `src/selection.ts`, `src/links.ts`, `src/images.ts` | keyboard/IME/mouse encoding, selection, OSC 8 + URL link detection, and decoded image cache |
| `src/view.ts` | `TerminalView`: DOM lifecycle, resize fitting, history paging, `FrameAck` pacing, accessibility mirror (`[data-terminal-text]`) |

## Key Properties

- **Dual painter pipeline (`WebGL2AtlasPainter` + `CanvasPainter`).** `createPainter(font, options)` prefers
  `WebGL2AtlasPainter` (`preferWebGL !== false`), rendering runs through a premultiplied RGBA8 shelf-packed
  glyph atlas, single-pass shader-driven dotted/dashed/curly underlines, instanced quads, and a bounded 64 MiB
  GPU texture LRU for Kitty/Sixel images — falling back cleanly to `CanvasPainter` when WebGL2 is unavailable
  or disabled (`preferWebGL: false`).
- **Adaptive backpressure.** A view acknowledges each rendered frame (`FrameAck` with `paint_us` and
  `frame_seq`), feeding the broker's adaptive BDP window so hidden or slow tabs never accumulate stale frames.
- **Authoritative broker queries.** Selection across scrollback, word/line boundaries, regex/literal search
  (`SearchWatch`), command navigation (`commands`, `scrollToCommand`, `selectOutput`), and content-addressed
  image blobs (`getImageBlob` with SHA-256 verification) all resolve against the broker's VT state.
- **Vouched vs claimed command gutter.** Commands render in the gutter by trust outcome: blue (running),
  green (vouched exit 0), red (vouched non-zero exit), yellow (claimed exit regardless of code), grey
  (unknown/not started).
- **Zero runtime dependencies & optional `clock`.** Ships one ES module + `.d.ts` with no WebAssembly or Node
  globals (strict DOM-only types). Pass an optional structural `{ after(ms, run): () => void }` `clock` to
  share timers or drive blink/debounce deterministically in tests.

## Golden Wire Vector Contract

`src/wire/` (`codec.ts`, `frame.ts`, `messages.ts`, `types.ts`) is the TypeScript half of `crates/apack-proto`. `node/wire-vectors-v2.json` is generated
by the Rust test suite (`cargo xtask generate`) and verified by `tests/wire-vectors.spec.ts` on every test run so
Rust and TypeScript codecs can never silently drift.

## License

MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)). Input handling draws on
[coder/ghostty-web](https://github.com/coder/ghostty-web) under MIT ([NOTICE](NOTICE)).
