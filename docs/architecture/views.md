# Views & Rendering — apack-terminal v2

`@apackdev/terminal-view` (`view/`) is a zero-VT TypeScript terminal renderer driven exclusively by
coalesced differential `FrameUpdate` messages from the broker's `libghostty-vt` core.

## Single Authority & Unidirectional Data Flow

```text
PTY bytes ─▶ GhosttyEngine (broker shard) ─▶ SessionMachine (dirty diff)
                                                    │
                                     BrokerMsg::Frame(FrameUpdate)
                                    (PackedRow runs, scroll_delta,
                                     cursor, modes, palette, images_gen,
                                     epoch, full_sync)
                                                    ▼
                    TerminalView ◀── BrokerSessionSource ◀── TerminalClient (v2 wire)
                         │
            ┌────────────┴────────────┐
            ▼                         ▼
    WebGL2AtlasPainter          CanvasPainter (default)
  (ShelfPacker RGBA8 atlas,   (2D canvas + row-shift
   instanced quads, 64 MiB     scroll blitting +
   GPU image texture LRU)      image compositing)
```

Because the broker is the sole VT authority:
- Screen text, scrollback search, word/line selection, shell markers, and rendered pixels can **never diverge**.
- Structured `Key`, `Text`, `Pointer`, `Focus`, and `Paste` messages flow back to the broker, which encodes
  escape sequences using the live terminal modes (Kitty keyboard flags, DECCKM, SGR/UTF-8 mouse modes,
  bracketed paste) directly inside `crates/apack-vt-ghostty/src/input.rs`.

## Differential View Replication & `PackedRow`

Each `BrokerMsg::Frame(FrameUpdate)` carries (`crates/apack-proto/src/messages.rs`):
- `seq`: monotonic per-stream sequence number acknowledged by `FrameAck { seq, credit }`.
- `epoch`: increments on screen reflow or primary/alternate screen transitions, telling `RowStore` to drop
  cached absolute line numbers cleanly.
- `size`, `first_active_line`, `scrollback_lines`: exact viewport geometry and scrollback extent.
- `scroll_delta: i32`: signed vertical scroll shift enabling Canvas/GPU blit shortcuts without repainting
  untouched rows.
- `full_sync: bool`: `true` when `rows` replaces the full active viewport (initial attach, epoch change, or
  large scroll).
- `rows: Vec<PackedRow>`: only the dirty rows since the view's last emitted frame (or full viewport on
  `full_sync`), encoded compactly (`crates/apack-proto/src/types.rs`):
  - `line`: absolute epoch line index
  - `flags`: `FLAG_WRAPPED`, `FLAG_PROMPT`, `FLAG_HAS_BLINK`, `FLAG_HAS_PLACEHOLDER`
  - `styles: Vec<PackedStyle>`: deduplicated style palette for the row (fg, bg, underline color/style, attrs, `link_idx`)
  - `links: Vec<String>`: deduplicated per-row OSC 8 hyperlink URIs (`link_idx` indexes directly into `links[link_idx]`)
  - `runs: Vec<PackedRun>`: contiguous styled text runs (`col_start`, `col_len`, `style_idx`, UTF-8 text or `PER_CELL` grapheme/wide breakdown)
  - `pieces`: optional Kitty Unicode placeholder runs on the row
- `cursor`, `modes`, `palette`, `images_gen`: authoritative cursor shape/position/visibility, active DEC/input
  modes, optional palette overrides, and monotonic `images_gen` counter (incremented whenever Kitty image
  placements change, prompting the view to refresh placements via `Images` and fetch missing SHA-256 blobs via
  `GetImageBlob`).
- **DEC 2026 Synchronized Output:** While mode 2026 is active, the shard withholds intermediate `Frame`
  flushes until the program ends the sync block or `SYNC_HOLD_MAX_MS` (`1_000 ms`) expires via the shard's
  `TimerWheel`.

## Painters (`createPainter`)

`TerminalView` defaults to `CanvasPainter`; callers opt into automatic WebGL2 with Canvas 2D fallback by
passing `painter: (font) => createPainter(font)` (`preferWebGL: true` by default; pass `preferWebGL: false`
to force Canvas 2D).

### `WebGL2AtlasPainter` (`view/src/webgl-painter.ts`)
- **Dynamic Glyph Atlas (`ShelfPacker`):** Packs rasterized glyphs into a premultiplied-alpha RGBA8 WebGL2
  texture (`UNPACK_PREMULTIPLY_ALPHA_WEBGL = 1`) with bearing/overhang padding for italic and bold glyphs.
  ASCII fast paths upload directly via WebGL2 9-arg `texSubImage2D` from the scratch canvas with zero
  `getImageData` readbacks; non-ASCII color emoji are detected (including grayscale color glyphs) and
  composited with full RGBA channels.
- **Shader-Driven Decorations & Instanced Rendering:** Backgrounds, glyphs, selection highlights, cursors,
  and single-pass shader-driven solid/double/dotted/dashed/curly underlines (`DrawMode`) render in minimal
  instanced draw calls at exact 1:1 device-pixel coordinates (`uViewportPx`).
- **Bounded GPU Image Texture LRU (`64 MiB`):** Kitty `ImageBitmap`s upload into GPU textures managed by a
  strict byte-budgeted LRU that calls `gl.deleteTexture` on eviction, close, or `dispose()`, batching
  same-texture slices and rendering the cursor strictly above `above: true` image overlays.
- **Context-Loss Resilience:** Handles `webglcontextlost` / `webglcontextrestored` cleanly and calls
  `WEBGL_lose_context` on `dispose()`.

### `CanvasPainter` (`view/src/canvas-painter.ts`)
- Zero-WebGL 2D canvas renderer supporting full DPR scaling, style runs, underline variants, Kitty image
  placements, and integer-row `tryScrollBlit` fast scrolling.

## Client, Scrollback, Search & Accessibility

- **`TerminalClient` & `BrokerSessionSource` (`view/src/broker/client.ts`):** Multiplexes control RPCs, views,
  incremental `setSearch` subscriptions, and pipelined `64 KiB` `getImageBlob` downloads (with full SHA-256
  verification of the reassembled bytes via `sha256` + `ctEq32`) over one `ByteStream`.
- **`RowStore` & `Viewport` (`view/src/row-store.ts`, `view/src/viewport.ts`):** Bounded line cache with
  demand-paged scrollback (`fetchRows`) and atomic epoch reset.
- **Incremental Search (`view/src/broker/client.ts`):** Subscribes to broker-side `SearchMatches` /
  `SearchDelta` updates (coalesced at `150 ms` on the shard) while preserving the active needle across screen
  epoch changes.
- **`Input` (`view/src/input.ts`):** Encodes keyboard, IME composition, Option-as-Alt, bracketed paste
  confirmation, and SGR mouse events (ignoring unsupported extra buttons safely).
- **Accessibility:** Mirrors active viewport text in `[data-terminal-text]`.
- **Optional `clock`:** Structural `{ after(ms, run): () => void }` injection lets hosts share one timer
  queue across many open views or run deterministic tests with zero runtime dependencies.
