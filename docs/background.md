# Why the broker exists

apack is an Electron app whose backend is a separate Node process — the API. Terminals used to be ptys
owned by that process, through `node-pty`, and everything wrong with that followed from where the pty
lived rather than from the binding's quality.

**A session's lifetime was the API's.** Restarting the API took the user's shells with it, including the
shells running the build they had restarted the API to fix. Nothing could outlive a reload.

**An addon in the API process needs an ABI rebuild.** `node-pty` was the one module in the tree tied to
Electron's Node version, so an Electron upgrade was a native rebuild, and a packaged app carried the
platform's debug symbols whether or not anyone wanted them.

**Windows was where it showed.** A process tree with no owner is not reliably reaped; ConPTY gives no EOF
on the master, so a shell that exited was never reported and the terminal stayed on screen with nothing
behind it; and a console that opens with a cursor-position request waits for a reply before it initialises,
which nothing was answering.

**Two copies of the screen disagreed.** The renderer parsed with xterm.js and the app read that copy as a
fact, so what a feature knew about a terminal depended on a window being open, and a session nobody was
drawing knew nothing about itself at all.

So the pty moved out, into one process that owns it:

- a session outlives whatever asked for it, and ends when the thing that *claimed* it goes
  ([design.md](architecture/design.md));
- a standalone binary has no ABI to match;
- the process tree is owned by the kernel's own mechanism on each platform;
- and the screen has one authority, which answers whether or not a window exists.

The work is tracked in the apack monorepo as `docs/goals/goal-terminal-ownership.md`, against a review of
the terminal integration that this broker's phases close.
