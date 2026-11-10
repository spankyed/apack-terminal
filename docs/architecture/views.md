# Views

A window draws a session from **rows the broker sends it**. It interprets nothing: the broker is the only
thing that reads a program's bytes, and a view is told the result — the characters on each line that
changed, with their styles — and paints it. So there is one copy of a screen, the cursor a program is told
about is the cursor that is drawn, and a question a program asks its terminal is answered once however
many windows are showing it.

The view's half is [`apack-terminal-view`](https://github.com/spankyed/apack-terminal-view), in
TypeScript. This is the broker's half: `src/view/wire.rs` is the bytes, `src/view/publish.rs` is what is
sent and when.

## A connection of its own

A view connects to the same socket as everything else and greets with `role: "view"`. The broker answers
`hello` as one JSON line, and from there the connection is binary in both directions. One connection
carries every session a window draws: the view gives each a **channel** number when it attaches, and every
message after names the channel.

Each view connection has its own writer and its own queue. A view that has stopped reading holds up
nobody else's screen and no client's answer.

## A message

`u32` little-endian length of what follows, one byte saying which message, then that message's fields in
order. Integers are LEB128; a string is its length and its UTF-8. A message is at most 8 MiB.

| to the broker | | to the view | |
|---|---|---|---|
| 1 `attach` | start sending this session's frames on a channel | 65 `frame` | what changed since the last frame this view drew |
| 2 `detach` | stop | 66 `rows` | lines asked for by number |
| 3 `acknowledge` | this frame was drawn | 67 `ended` | the session is over |
| 4 `fetch` | lines by number | 68 `pasted` | whether a paste went, or was held as unsafe |
| 5 `key` | a key as the keyboard reported it | 69 `span` | a word or a line, as the cells it covers |
| 6 `text` | text that did not come from a key | 70 `text` | the text of a range |
| 7 `pointer` | a pointer event, in cells | 71 `spans` | every match of a search |
| 8 `paste` | pasted text | 72 `failed` | a request could not be answered |
| 9 `focus` | the view gained or lost focus | 73 `commands` | the commands the shell marked, with their lines |
| 10 `wordAt`, 11 `lineAt` | the word or line at a cell | 74 `images` | where every image is |
| 12 `textOf` | the text between two cells | 75 `image` | part of an image's pixels |
| 13 `search` | where a string appears | | |
| 14 `commands` | the commands the shell marked | | |
| 15 `images`, 16 `image` | where the images are; one's pixels | | |

## Lines are numbered

A line keeps its number for as long as the screen is not reflowed. `first_active` is the number of the
screen's top row; history is every line below it, back to `first_active - scrollback`. So a view keeps the
rows it has been sent and names the ones it has not, and neither side keeps a copy of the other's position.

A resize, a switch to or from the alternate screen, or an erased history changes what the numbers mean.
The **epoch** in every frame says which numbering it is in, and a view drops what it holds when it moves.
An answer from before the epoch moved names lines that are no longer there, and is not used.

The count is exact because it is read from the core: a tracked reference pinned at the screen's top row
before a write says how far the screen moved during it.

## A frame, and when one is sent

A frame carries the grid, the cursor, the modes a view needs (alternate screen, mouse reporting), counters
that move when the commands or the images changed, and the rows that differ from what **that view** last
acknowledged.

**A view is sent a frame when the screen has changed and it has acknowledged the last one.** One frame is
in flight per view, so a view that stops drawing — hidden, busy, gone — costs nothing, and a program that
prints faster than a window paints is drawn at the window's rate with nothing queued between them. Lines
that scrolled off while a view was not looking are resent when they are few and marked stale when they are
many, for the view to fetch if it scrolls there.

A program that asks for its output to be held until a frame is whole (mode 2026) is published once, at the
end, and held for a second at most.

## Input is encoded here

A view sends a key, a pointer event or a paste as it happened. The broker's core turns it into bytes by
the modes the program actually set — which keyboard protocol, which mouse reporting, whether pastes are
bracketed — so a view carries no table of escape sequences and cannot disagree with the terminal about
what mode it is in.

A paste that could run a command — one holding the marker that ends a bracketed paste, or a newline where
pastes are not bracketed — is answered `unsafe` and not sent. The view asks the person, and sends it again
allowed if they say so.

## Reads are the core's

What counts as a word, where a wrapped line begins and ends, what the text of a selection is, and where a
string appears are asked of the core rather than worked out from rows. They are the rules every terminal
built on it has, and a search covers history a view has never been sent.

## Commands and images

A shell that marks its commands (OSC 133, OSC 633) has each kept with the line its prompt, its output and
its end are on; a frame's `commands` counter moves when one is added, and a view asks for the list. The
last 256 are kept.

A session asked to take images (`Spawn.images`) accepts the Kitty graphics protocol, as bytes in the
stream only. A frame's `images` counter moves when one is sent, placed, replaced or deleted. A view asks
where they are and is told each by the line it is on, which stays true as the screen scrolls; it asks for
an image's pixels once per generation, in parts of 4 MiB. A placement shown through placeholder cells has
no place of its own and is not told. Animations are not advanced.

## The two halves meet in a file

`node/view-vectors.json` holds every kind of message as the bytes this encoder writes and the value they
mean. `tests/view_vectors.rs` fails when the file is not what the encoder writes now; the view's suite
decodes every vector for a view and encodes every one for the broker. Re-record with
`APACK_UPDATE_VECTORS=1 cargo nextest run --test view_vectors`, read the diff, and take the file to the
view: every line of it is a change a view has to be taught.
