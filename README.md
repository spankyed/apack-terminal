# apack-terminal-view

Draws a terminal from rows something else interpreted.

A view here has no terminal core. It is handed rows of characters with their styles by a `TerminalSource`,
which is whatever owns the terminal — in apack, the `apack-terminal` broker — and it owns the rest: painting,
scrolling, selection, text input, links and what a screen reader hears. Input goes back to the source as key
and pointer events, not bytes, because what a key sends depends on modes only the terminal's owner knows.

`src/source.ts` is the contract.

```ts
import { BrokerViews, TerminalView } from 'apack-terminal-view'

// `stream` is a connection to a broker that has already greeted it as a view
const views = new BrokerViews(stream)
const view = new TerminalView(views.open(sessionId), { font, theme })
view.mount(element)
```

```sh
npm install
npm test
npm run build
```

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
- **Marks commands** in a gutter where the host gives it one, coloured by how each ended.
- **Draws images** a program placed: their pixels are fetched once per generation and drawn under or over
  the text as the program asked, scaled from the cell the terminal measured them in to the cell drawn.
- **Confirms a paste** the source says could run a command, through the host's `confirmPaste`.
- **Says what is on screen as text**, in an element a screen reader and a test read
  (`[data-terminal-text]`), and which frame it last painted (`data-terminal-drawn`).

## The protocol's two halves

`src/broker/wire.ts` is the view's half of `apack-terminal`'s view protocol; the other half is in Rust, in
that repository. Neither suite can see both, so they are held together by
`tests/vectors/view-vectors.json`, which the broker writes from its encoder: `tests/wire.spec.ts` decodes
every vector for a view and encodes every one for the broker. When the broker's protocol changes, its
re-recorded file is copied here, and this suite fails until the codec reads it.

Began as a clone of [coder/ghostty-web](https://github.com/coder/ghostty-web) (MIT), whose input handling it
draws on.
