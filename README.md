# apack-terminal-view

Draws a terminal from rows something else interpreted.

A view here has no terminal core. It is handed rows of characters with their styles by a `TerminalSource`,
which is whatever owns the terminal — in apack, the `apack-terminal` broker — and it owns the rest: painting,
scrolling, selection, text input, links and what a screen reader hears. Input goes back to the source as key
and pointer events, not bytes, because what a key sends depends on modes only the terminal's owner knows.

`src/source.ts` is the contract.

```sh
npm install
npm test
npm run build
```

Began as a clone of [coder/ghostty-web](https://github.com/coder/ghostty-web) (MIT), whose input handling it
draws on.
