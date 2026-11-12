# apack-terminal-view

Draws a terminal from rows something else interpreted.

A package of the [`apack-terminal`](..) repository, beside the broker whose rows it draws.

A view here has no terminal core. It is handed rows of characters with their styles by a `TerminalSource`,
which is whatever owns the terminal — in apack, the broker at this repository's root — and it owns the rest:
painting, scrolling, selection, text input, links and what a screen reader hears. Input goes back to the
source as key and pointer events, not bytes, because what a key sends depends on modes only the terminal's
owner knows.

`src/source.ts` is the contract.

```ts
import { BrokerViews, TerminalView } from 'apack-terminal-view'

// `stream` is a connection to a broker that has already greeted it as a view
const views = new BrokerViews(stream)
const view = new TerminalView(views.open(sessionId), { font, theme })
view.mount(element)
```

From this directory, on Node 24.12 or later:

```sh
npm ci
npm run typecheck
npm test
npm run build        # dist/, which is what a consumer bundles
```

The repository's CI runs those four as its `view` job, and `cargo xtask pre-push` at the root runs them
after the broker's suites.

## Using it

Not published. A consumer depends on this directory (apack: `file:…/vendor/apack-terminal/view`, through
its link to a checkout of this repository) and bundles `dist/`, which is not tracked — so it is built
wherever the checkout is linked or cloned. There is no WebAssembly and nothing to load at runtime: one ES
module and its declarations.

## What is where

| | |
|---|---|
| `src/source.ts` | what a view is drawn from: frames, rows, input, the questions it may ask. Plain objects, so a view is driven by a broker, a recording or a test alike |
| `src/broker/` | a broker as a source: the view protocol's bytes (`wire.ts`) and the sessions on one connection (`source.ts`) |
| `src/row-store.ts`, `src/viewport.ts` | the rows a view holds, by line number, and where it is scrolled to among them |
| `src/draw.ts`, `src/painter.ts`, `src/canvas-painter.ts` | a row as what to paint, and the painter that paints a scene on a 2D canvas. A painter is behind an interface, so another can replace it |
| `src/input.ts`, `src/selection.ts`, `src/links.ts` | the keyboard and the input method; what is selected; URLs and whatever else a host's detectors find |
| `src/view.ts` | the view: it owns the element, asks for history as it is scrolled to, and acknowledges each frame once it is painted |

## What a view does

- **Draws only what it is sent, and says when it has.** A source sends the next frame once the last was
  acknowledged, and a view acknowledges a frame when it has painted it — so a view that is hidden or busy
  is sent nothing, and a program that prints faster than a window paints is drawn at the window's rate.
- **Holds the lines it has been shown.** History is asked for by line number as it is scrolled to, and
  dropped when the screen is reflowed and the numbers no longer mean what they did.
- **Asks the source what only it knows**: the word or line at a cell, the text of a selection, where a
  string appears (`find`, `findNext`), the commands a shell marked (`commands`, `scrollToCommand`,
  `selectOutput`) and where images are.
- **Marks commands** in a gutter where the host gives it one, coloured by how each ended: green for a
  status of zero, red for any other, grey where there was no status or nothing reported the end. The
  colour is the same whether the status was vouched for or only claimed; a command's `outcome` is what
  says which, for whatever reads it.
- **Draws images** a program placed: their pixels are fetched once per generation and drawn under or over
  the text as the program asked, scaled from the cell the terminal measured them in to the cell drawn.
- **Confirms a paste** the source says could run a command, through the host's `confirmPaste`.
- **Says what is on screen as text**, in an element a screen reader and a test read
  (`[data-terminal-text]`), and which frame it last painted (`data-terminal-drawn`).

## What a host says to it

- **`setTheme(theme)` and `setFont(font)`** change what a view is drawn in after it was made. Every row it
  holds is drawn again and none is asked for; a font is measured, and `onFit` says what fits now.
- **`followOnInput`** (on unless turned off): typing, composed text and a paste bring a view that is
  scrolled back down to the screen, where what was typed appears.
- **`onPasteRefused`** hears of a paste too large for the source to carry, of which none was sent. `paste()`
  called by the host rejects with the same `TooLarge` instead.
- **`optionIsAlt`** (off unless turned on): a Mac's option key is alt, as a person who set their terminal
  that way expects, rather than the key that composes characters. A key held with it is then sent as what
  the key types without it. Left off, a character option typed is sent as that character, marked as
  `composed`.
- **`BrokerViews.onClose(listener)`** hears, once, that a connection has stopped — it was closed, its stream
  ended, or what arrived was not the protocol — which is when an owner that keeps one per broker makes
  another.
- **`onEnded(reason)`** hears that no more frames will come: with no reason the session is over, and with
  one it is there and the broker could not show it.

## The protocol's two halves

`src/broker/wire.ts` is the view's half of the broker's view protocol; the other half is in Rust, at this
repository's root (`src/view/wire.rs`). Neither suite can run both, so they are held together by
`node/view-vectors.json`, which the broker's suite writes from its encoder: `tests/wire.spec.ts` reads that
file where it lies, decodes every vector for a view and encodes every one for the broker. When the
protocol changes, the re-recorded file fails this suite until the codec reads it — in the same commit,
since both halves are in it.

## License

MIT ([LICENSE](LICENSE)). The input handling draws on
[coder/ghostty-web](https://github.com/coder/ghostty-web), whose notice that file carries.
