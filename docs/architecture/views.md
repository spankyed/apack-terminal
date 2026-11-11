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
nobody else's screen and no client's answer — and is not waited for for ever: once 64 MiB is waiting for
it to read, what was waiting is dropped, nothing more is queued for it, and it is hung up on. So is a
view that closed its side and then took nothing it was still owed for three seconds: finishing speaking
is what a view that is listening does, and listening is taking messages.

**On Windows the queue ends and the connection does not.** A named pipe cannot be closed from under a
thread that is writing to it, so there a view past the limit is sent nothing more and holds none of the
broker's memory, and its connection — with the one thread parked writing to it — lasts until the view
reads or closes its end. A view that finds itself sent nothing should reconnect; one that has stopped
reading altogether costs a thread until its process goes.

A channel's session is looked up when the view attaches, and what the view asks from then on is that
session's work alone. An `attach` to a session that is not there is answered `ended`; one to a session
that is there and could not be shown is answered `failed`, as request `0`.

## A message

`u32` little-endian length of what follows, one byte saying which message, then that message's fields in
order. Integers are LEB128; a string is its length and its UTF-8. A message is at most 8 MiB, and the
broker builds none longer: a frame or a fetch carries the rows that fit and leaves the rest for the next,
and a text too long to carry is refused.

| to the broker | | to the view | |
|---|---|---|---|
| 1 `attach` | start sending this session's frames on a channel | 65 `frame` | what changed since the last frame this view drew |
| 2 `detach` | stop | 66 `rows` | lines asked for by number |
| 3 `acknowledge` | this frame was drawn | 67 `ended` | the session is over |
| 4 `fetch` | lines by number | 68 `pasted` | whether a paste went, or was held as unsafe |
| 5 `key` | a key as the keyboard reported it | 69 `span` | a word or a line, as the cells it covers |
| 6 `text` | text that did not come from a key | 70 `text` | the text of a range |
| 7 `pointer` | a pointer event, in pixels from the grid's corner, with the size of a cell | 71 `spans` | every match of a search |
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

A resize, a switch to or from the alternate screen, or more scrolling between two looks than the history
holds changes what the numbers mean. The **epoch** in every frame says which numbering it is in, and a
view drops what it holds when it moves.

**Attaching does not move it while another view has stayed**, since that view's rows are still good. What
a view that attaches is told instead is that nothing it may have kept of the history is: its first frame
carries the whole screen and names every line of history as stale (`stale_from`, the oldest line held),
so a view that was attached before drops what it held and fetches it again. Once the last view has left,
the numbering is let go of, and the next view to attach is in a new epoch.

**An erased history does not.** The lines that are left keep their numbers; what a frame says is how many
lines of history there now are (`scrollback`), and a view drops every line it holds below
`first_active - scrollback`. A line number at or above that with no row behind it is a line the session
does not hold: one that is gone, or one not written yet.

**A question names its epoch, and is refused when the epoch has moved.** `fetch`, `wordAt`, `lineAt` and
`textOf` carry the epoch the view was in when it asked; if the lines have been numbered again since, the
same numbers are other lines, and the answer is `failed` rather than text from somewhere else. Every
answer that names lines carries the epoch they are in.

**The numbering is the views'.** A session keeps one only while something views it, so a line number
means nothing to a `client`, and is never made up for one.

The count is exact because it is read from the core: a tracked reference pinned at the screen's top row
before a write says how far the screen moved during it.

## A frame, and when one is sent

A frame carries the grid, the cursor, the modes a view needs (alternate screen, mouse reporting, reverse
video), counters that move when the commands or the images changed, and the rows that differ from what
**that view** last acknowledged.

**And the colours a program changed.** A view draws in a theme of its own, which is where the colours a
session is given come from; a program that sets its foreground, background or cursor colour (OSC 10, 11,
12) or redefines palette entries (OSC 4) expects to be drawn in them. A frame carries what differs from
the given colours — all of it, whenever any of it changed, and in a view's first frame — and what it
carries replaces what the view held: a colour absent from it is the theme's again. A change of colour is
a frame like any other change, with no row in it.

**A row says everything about itself**: its styles and its links are its own, and its glyphs index them
from one, `0` being the plain style and no link. So a row is written out once when it changes, the same
bytes go to every view owed it, and how long a message will be is known before it is built.

**A view is sent a frame when the screen has changed and it has acknowledged the last one.** One frame is
in flight per view, so a view that stops drawing — hidden, busy, gone — costs nothing, and a program that
prints faster than a window paints is drawn at the window's rate with nothing queued between them. Lines
that scrolled off while a view was not looking are resent when they are few and marked stale when they are
many, for the view to fetch if it scrolls there.

A resize is itself a change a view is sent, whether or not the program prints anything in answer.

**A screen longer than one message is sent in as many frames as it takes.** Rows go in line order; a frame
that is full stops there, and the view is owed the rest in a frame of its own, with a sequence of its own,
as soon as it acknowledges — nothing more needs to be written.

**A program that asks for its output to be held until a frame is whole (mode 2026) is drawn when it lets
go**: by the write that let go, with the screen as it stood then, rather than by whatever is written
next. Each hold has a second from when it began, however soon after the last; one that is never let go is
drawn when that second is up — or a moment after, when the session's screen is busy just then — which is
the one thing here a clock decides, since no byte arrives to say
so. A view that attaches during a hold is showing nothing at all, and is sent the screen as it stands.

## Input is encoded here

A view sends a key, a pointer event or a paste as it happened. The broker's core turns it into bytes by
the modes the program actually set — which keyboard protocol, which mouse reporting, whether pastes are
bracketed — so a view carries no table of escape sequences and cannot disagree with the terminal about
what mode it is in.

A paste that could run a command — one holding the marker that ends a bracketed paste, or a newline where
pastes are not bracketed — is answered `unsafe` and not sent. The view asks the person, and sends it again
allowed if they say so. A paste is answered when it has been queued for the program, not when the program
has read it.

**What a view says about a key that the terminal cannot know** is which keyboard it came from. `composed`
says alt went into producing the key's text rather than modifying the key — the option key on a Mac that
is typing characters. Without it alt is alt everywhere: `Alt+Shift+A` is sent as `ESC A`. A character
typed through AltGr, which a browser reports as control and alt held, is sent as its text with those two
modifiers left out. And where the option key is set to be alt, the text sent is what the key types
without it, since the system has already composed the character.

Which buttons are down is each view's own. A view that loses focus, detaches or goes is holding none,
whatever it last said; a button other than left, middle or right is never held.

## Reads are the core's

What counts as a word, where a wrapped line begins and ends, what the text of a selection is, and where a
string appears are asked of the core rather than worked out from rows. They are the rules every terminal
built on it has, and a search covers history a view has never been sent. A search answers the newest two
thousand matches, and stops looking once it has them.

`rows` says which lines it answers for — `first`, and how many from there. A line among them with no row
is one the session does not hold: one that is gone, or one not written yet. A line past them was not
answered, because the answer was full or the fetch was for more than two thousand lines, and is asked for
again.

## Commands and images

A shell that marks its commands (OSC 133, OSC 633) has each kept with the line its prompt, its output and
its end are on; a frame's `commands` counter moves when one is added, and a view asks for the list. The
last 256 are kept. A command's lines are told while the screen it ran on is the one showing: the shell's
commands have none while an alternate-screen application is up.

A session asked to take images (`Spawn.images`) accepts the Kitty graphics protocol, as bytes in the
stream only: raw pixels, or a PNG, which the broker decodes for the core (`rgba_of_png`) since the core
leaves that to whoever embeds it. A frame's `images` counter moves when one is sent, placed, replaced or
deleted. A view asks
where they are and is told each by the line it is on, which stays true as the screen scrolls; it asks for
an image's pixels once per generation, in parts of 4 MiB. A placement shown through placeholder cells has
no place of its own and is not told. Animations are not advanced.

## The two halves meet in a file

`node/view-vectors.json` holds every kind of message as the bytes this encoder writes and the value they
mean. `tests/view_vectors.rs` fails when the file is not what the encoder writes now; the view's suite
decodes every vector for a view and encodes every one for the broker. Re-record with
`APACK_UPDATE_VECTORS=1 cargo nextest run --test view_vectors`, read the diff, and take the file to the
view: every line of it is a change a view has to be taught.
