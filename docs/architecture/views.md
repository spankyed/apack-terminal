# Views & Rendering — apack-terminal v2

`@apackdev/terminal-view` (`view/`) is a zero-VT TypeScript terminal renderer driven exclusively by
coalesced differential state frames from the broker's `libghostty-vt` core.

## Single Authority & Unidirectional Data Flow

```text
PTY bytes ─▶ GhosttyEngine (broker shard) ─▶ ViewProjection (dirty diff)
                                                    │
                                      ViewFrame (PackedRow runs,
                                       scroll_hint, cursor, modes,
                                       images, palette, epoch)
                                                    ▼
                    TerminalView ◀── BrokerSessionSource ◀── TerminalClient (v2 wire)
                         │
            ┌────────────┴────────────┐
            ▼                         ▼
    WebGL2AtlasPainter          CanvasPainter
  (ShelfPacker RGBA8 atlas,   (2D canvas + row-shift
   instanced quads, 64 MiB     scroll blitting +
   GPU image texture LRU)      image compositing)
```

Because the broker is the sole VT authority:
- Screen text, scrollback search, word/line selection, shell markers, and rendered pixels can **never diverge**.
- Structured `KeyInput`, `MouseInput`, and `Paste` events flow back to the broker, which encodes escape
  sequences using the live terminal modes (Kitty keyboard flags, DECCKM, SGR/UTF-8 mouse modes, bracketed
  paste) directly inside `apack-vt-ghostty/src/input.rs`.

## Differential View Replication & `PackedRow`

Each `ViewFrame` carries:
- `frame_seq`: monotonic per-stream sequence number acknowledged by `FrameAck`.
- `epoch`: increments on screen reflow or primary/alternate screen transitions, telling `RowStore` to drop
  cached absolute line numbers cleanly.
- `size`, `first_active_line`, `total_lines`: exact viewport geometry and scrollback extent.
- `scroll_hint`: optional `{ top, bottom, delta }` vertical scroll region hint enabling GPU/Canvas blit
  shortcuts without repainting untouched rows.
- `dirty_rows`: only the rows modified since the view's last emitted frame (or all viewport rows on initial
  attach / epoch change), encoded as compact **style-run `PackedRow`s**:
  - `line`: absolute line index
  - `flags`: `WRAPPED`, `HAS_GRAPHEMES`, `HAS_WIDE`
  - `text`: UTF-8 text of the row
  - `runs`: array of `StyleRun { byte_len, cols, fg, bg, underline_color, attrs, link_idx }`
  - `links`: deduplicated per-row OSC 8 hyperlink URI table (`link_idx` indexes directly into `links[link_idx]`)
  - `cell_byte_offsets`: optional per-cell byte-offset map present only when a row contains multi-codepoint
    grapheme clusters or wide/surrogate cells (`PER_CELL`)
- `cursor`, `modes`, `palette`, `images`: authoritative cursor shape/position/visibility, active DEC/input
  modes, palette overrides, and visible Kitty/Sixel `ImagePlacement` descriptors (referencing blobs by SHA-256
  `BlobHash`).
- **DEC 2026 Synchronized Output:** While mode 2026 is active, the shard withholds intermediate `ViewFrame`
  flushes until the program ends the sync block or `SYNC_HOLD_MAX_MS` (`1_000 ms`) expires via the shard's
  `DeadlineWheel`.

## Painters (`createPainter`)

`createPainter(font, options)` prefers `WebGL2AtlasPainter` when available (`preferWebGL: true` by default) and falls back to `CanvasPainter` (or forces Canvas 2D with `preferWebGL: false`):

### `WebGL2AtlasPainter` (`view/src/webgl-painter.ts`)
- **Dynamic Glyph Atlas (`ShelfPacker`):** Packs rasterized glyphs into a premultiplied-alpha RGBA8 WebGL2
  texture (`UNPACK_PREMULTIPLY_ALPHA_WEBGL = 1`) with bearing/overhang padding for italic and bold glyphs.
  ASCII fast paths upload directly via WebGL2 9-arg `texSubImage2D` from the scratch canvas with zero
  `getImageData` readbacks; non-ASCII color emoji are detected (including grayscale color glyphs) and
  composited with full RGBA channels.
- **Shader-Driven Decorations & Instanced Rendering:** Backgrounds, glyphs, selection highlights, cursors,
  and single-pass shader-driven solid/double/dotted/dashed/curly underlines (`DrawMode`) render in minimal
  instanced draw calls at exact 1:1 device-pixel coordinates (`uViewportPx`).
- **Bounded GPU Image Texture LRU (`64 MiB`):** Kitty/Sixel `ImageBitmap`s upload into GPU textures managed
  by a strict byte-budgeted LRU that calls `gl.deleteTexture` on eviction, close, or `dispose()`, batching
  same-texture slices and rendering the cursor strictly above `above: true` image overlays.
- **Context-Loss Resilience:** Handles `webglcontextlost` / `webglcontextrestored` cleanly and calls
  `WEBGL_lose_context` on `dispose()`; in `'auto'` mode, permanently lost or unavailable WebGL2 contexts
  fall back automatically to `CanvasPainter`.

### `CanvasPainter` (`view/src/canvas-painter.ts`)
- Zero-WebGL 2D canvas renderer supporting full DPR scaling, style runs, underline variants, image
  placements, and integer-row `tryScrollBlit` fast scrolling.

## Client, Scrollback, Search & Accessibility

- **`TerminalClient` (`view/src/broker/client.ts`):** Multiplexes control RPCs, views, `SearchWatch` streams,
  and `getImageBlob` (with SHA-256 digest verification and strict size bounds) over one `ByteStream`.
- **`RowStore` & `Viewport` (`view/src/row-store.ts`, `view/src/viewport.ts`):** Bounded line cache with
  demand-paged scrollback (`readHistory`) and atomic epoch reset.
- **Incremental `SearchWatch` (`view/src/broker/client.ts`):** Subscribes to broker-side incremental search updates (debounced
  at `150 ms` on the shard) while preserving the active needle across screen epoch changes.
- **`InputHandler` (`view/src/input.ts`):** Encodes keyboard, IME composition, Option-as-Alt, bracketed paste
  confirmation, and SGR mouse events (ignoring unsupported extra buttons safely).
- **Accessibility:** Mirrors active viewport text in `[data-terminal-text]` and exposes rendered sequence
  state via `data-terminal-drawn`.
- **Optional `clock`:** Structural `{ after(ms, run): () => void }` injection lets hosts share one timer
  queue across many open views or run deterministic tests with zero runtime dependencies.
