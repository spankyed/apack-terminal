/**
 * What a view is drawn from.
 *
 * **A view interprets nothing.** Something else owns the terminal — reads the program's bytes, keeps the
 * screen and its history, knows which modes are on — and a view is handed the result as rows of characters
 * with their styles. Everything here is that result as plain objects, so a view can be driven by a broker
 * across a socket, by a recording, or by a test that builds rows by hand.
 *
 * **Lines are numbered absolutely.** A line keeps its number for as long as the screen is not reflowed:
 * history is every line below `firstActive`, the screen a program can still write to is `firstActive` up to
 * `firstActive + rows`, and the oldest line still held is `firstActive - scrollback`. So a view can keep
 * rows it has been sent and name the ones it has not, without either side keeping a copy of the other's
 * position.
 */

/**
 * A colour a character asks for: `null` for the terminal's own default, one of the 256 a palette names
 * (`paletteColor`), or exactly `0xRRGGBB`.
 *
 * **A palette colour stays a name.** Which red "red" is belongs to the theme a view is drawn in, so it is
 * looked up when a row is drawn, not when it is sent — and a change of theme repaints every row a view
 * holds without asking anyone for them again.
 */
export type Color = number | null

/** The bit that says a colour is a palette index rather than a colour */
const PALETTE = 0x1000000

/** The colour the palette's entry `index` names, whatever that turns out to be */
export function paletteColor(index: number): number {
  return PALETTE | (index & 0xff)
}

/** The palette entry a colour names, or `null` when it is a colour of its own */
export function paletteIndex(color: number): number | null {
  return color & PALETTE ? color & 0xff : null
}

/**
 * How a run of characters is drawn. `flags` is a set of `StyleFlag` bits with the underline kind above them
 * (`underlineOf`).
 */
export interface Style {
  readonly fg: Color
  readonly bg: Color
  readonly underlineColor: Color
  readonly flags: number
}

export const StyleFlag = {
  bold: 1,
  italic: 2,
  faint: 4,
  blink: 8,
  inverse: 16,
  invisible: 32,
  strikethrough: 64,
  overline: 128,
} as const

export const UNDERLINES = ['none', 'single', 'double', 'curly', 'dotted', 'dashed'] as const
export type Underline = (typeof UNDERLINES)[number]

/** The underline a style asks for, which lives in the three bits above the flags */
export function underlineOf(style: Style): Underline {
  return UNDERLINES[(style.flags >> 8) & 7] ?? 'none'
}

/** The style of a character nothing has styled */
export const PLAIN: Style = { fg: null, bg: null, underlineColor: null, flags: 0 }

/**
 * One line of the screen or its history.
 *
 * **One entry per character drawn, not per cell.** A character is whatever occupies a cell or two as a
 * unit — a letter with its accent, an emoji with its skin tone — so `text[i]` may be several code points,
 * `column[i]` is the cell it starts in and `width[i]` is how many cells it covers. Cells past the last
 * character are blank and carry no style.
 *
 * **A row says everything about itself.** The styles and links its characters name are its own, so a row
 * is drawn from nothing but itself and the theme, means the same in whichever message brought it, and
 * takes its tables with it when it is let go.
 */
export interface Row {
  readonly line: number
  /** The line runs on into the next row: together they are one line a program wrote */
  readonly wrapped: boolean
  /** The shell drew its prompt on this row */
  readonly prompt: boolean
  readonly text: readonly string[]
  readonly column: Uint16Array
  readonly width: Uint8Array
  /** For each character, an index into `styles`; one that names nothing is drawn plain */
  readonly style: Uint16Array
  /** For each character, an index into `links`; `0` is no link */
  readonly link: Uint16Array
  /** The styles on this row. `styles[0]` is always `PLAIN`, so a character's `0` means "unstyled" */
  readonly styles: readonly Style[]
  /** The links on this row. `links[0]` is the empty string, so a character's `0` means none */
  readonly links: readonly string[]
  /**
   * What this row's placeholder cells show, on a row that holds any — which it may while they show
   * nothing, and then `pieces` is empty. Absent on a row with no such cells
   */
  readonly images?: RowImages
}

/**
 * Part of an image shown in a run of placeholder cells on one row.
 *
 * **It is the row's, because the cells are.** A program that wants an image to move with its text prints
 * placeholder characters where the image is to be, and the image is wherever those characters are. So what
 * they show comes with the row that holds them, in the message that brings the row: the picture and the
 * text beside it are drawn from one frame, and a row scrolled back to shows what it showed. The cells
 * themselves arrive blank.
 *
 * `offsetX`, `offsetY`, `width` and `height` are in the pixels of a cell as the terminal was told one is
 * (`RowImages.cellWidth`), which a view scales to the cell it draws. A piece is drawn under the text.
 */
export interface Piece {
  readonly image: number
  /** Moves when the image's pixels do, so the ones a view holds are known stale */
  readonly generation: number
  /** The first cell of the run, and how many cells it is */
  readonly column: number
  readonly columns: number
  readonly offsetX: number
  readonly offsetY: number
  readonly width: number
  readonly height: number
  /** The part of the picture drawn, in its own pixels */
  readonly sourceX: number
  readonly sourceY: number
  readonly sourceWidth: number
  readonly sourceHeight: number
}

/**
 * What a row's placeholder cells show, and one cell in the pixels that is measured in.
 *
 * **Said of every row that holds such cells, whether or not they show anything now.** What they show
 * depends on the image they name as well as on the cells: one not yet all there, or deleted, shows nothing,
 * and shows again when it arrives. A view keeps rows, so this is how it knows which to ask after when the
 * images change.
 */
export interface RowImages {
  readonly cellWidth: number
  readonly cellHeight: number
  readonly pieces: readonly Piece[]
}

export const CURSOR_SHAPES = ['block', 'bar', 'underline', 'hollow'] as const
export type CursorShape = (typeof CURSOR_SHAPES)[number]

/**
 * Where the cursor is, and how it is drawn when it is.
 *
 * **Where it is and whether it is drawn are two facts.** A program that draws a cursor of its own hides
 * the terminal's and still moves it to where typing goes, because that is where an input method composes.
 * So a view puts its input there whether or not it draws anything.
 */
export interface Cursor {
  /** The cell, within the screen: `row` counts from `firstActive` */
  readonly column: number
  readonly row: number
  readonly shape: CursorShape
  readonly blinking: boolean
  /** The program has hidden it: nothing is drawn, and it is still where this says */
  readonly hidden: boolean
}

/** What a view has to know about the program's modes, which is very little: the terminal's owner encodes input */
export interface Modes {
  /** A full-screen program is showing; it has no history to scroll */
  readonly alternateScreen: boolean
  /** The program asked for the mouse, so a click is its business and not a selection */
  readonly mouse: boolean
}

/** Rows, as a message brings them */
export interface Rows {
  readonly rows: readonly Row[]
}

/**
 * The answer to a fetch: the rows the owner holds among the `count` lines from `first`, in the epoch they
 * are numbered in.
 *
 * **It says which lines it answers for**, which may be fewer than were asked about: an answer holds what
 * one message can. A line among them with no row is one the owner does not hold. A line past them was not
 * answered, and is there to be asked for again.
 */
export interface Fetched extends Rows {
  readonly epoch: number
  readonly first: number
  readonly count: number
}

/**
 * The screen as it now stands, as a change from the frame a view last acknowledged.
 *
 * **A frame is not a unit of output.** The owner sends one when the screen has changed and the last has
 * been acknowledged, so a program that writes faster than a view draws produces fewer frames, not a
 * backlog: each carries everything that changed since, however much that was.
 *
 * **Nor is it the whole of a change.** A screen longer than one message can hold comes in as many frames as
 * it takes, the next after the last was acknowledged. A line of the screen no frame has brought yet is
 * blank until one does.
 */
export interface Frame extends Rows {
  readonly sequence: number
  /** Changes when every line number does — a resize that reflowed, a cleared history. A view forgets its rows */
  readonly epoch: number
  readonly firstActive: number
  readonly columns: number
  readonly screenRows: number
  /** How many lines of history are held below `firstActive` */
  readonly scrollback: number
  /** Moves whenever the shell reports a command, which is when `commands()` has something new to say */
  readonly commands: number
  /**
   * Moves whenever an image is sent, placed, replaced or deleted, and whenever the size of a cell the
   * terminal was told changes: the cue to ask where the images are again
   */
  readonly images: number
  /** Absent when it is not on the screen. One the program has hidden is here, and says so */
  readonly cursor: Cursor | null
  readonly modes: Modes
  /**
   * History lines from here up to `firstActive` changed since the last frame and are not among `rows`: a
   * view that kept any of them must let them go. Absent when none did.
   */
  readonly staleFrom?: number
  /** The program has the whole screen in reverse video: the default foreground and background change places */
  readonly reverseVideo: boolean
  /**
   * The colours the program changed, when this frame says what they are: an attachment's first frame
   * does, and so does any frame since whose colours differ from the frame before. Absent, they are what
   * they were.
   */
  readonly colors?: SessionColors
}

/**
 * The colours a program set for itself, over the ones the session was given — which are the theme's.
 *
 * **All of them, every time.** What this names replaces what a view held of the same: a colour that is
 * `null`, and a palette entry that is not listed, is the theme's again.
 */
export interface SessionColors {
  /** `0xRRGGBB`, or `null` where the program left the theme's */
  readonly foreground: number | null
  readonly background: number | null
  readonly cursor: number | null
  /** The numbered colours the program redefined, each by its number */
  readonly palette: ReadonlyArray<{ readonly index: number; readonly rgb: number }>
}

/**
 * How a command ended, or that it has not: one value, which says who said so.
 *
 * **`exited` is the only one that is an exit status.** It is what a mark carrying the session's key
 * reported, and nothing a program printed can be read as one. `claimed` is what a session with no key
 * heard — every marker believed, so a status that anything could have printed — and `unknown` is a
 * command in a session with a key that is over with nothing believable having said how. Code that wants
 * to know a command succeeded asks for `exited` with a code of zero.
 */
export type CommandOutcome =
  /** It has not ended. One still waiting at its prompt has not begun either: its `startedMs` is `null` */
  | { readonly kind: 'running' }
  | { readonly kind: 'exited'; readonly code: number }
  | { readonly kind: 'unknown'; readonly why: 'no_status' | 'superseded' | 'session_ended' }
  | { readonly kind: 'claimed'; readonly code: number | null }

/**
 * A command the shell ran, as far as the shell reported it. What vouches for its fields is the
 * session's: in one with no key, every field but `id` is a claim.
 *
 * `prompt`, `output` and `end` are the lines the command's prompt, its output and its end are on, in the
 * epoch the answer came in. One is `null` where the shell never marked that moment, or where the row has
 * left the history.
 */
export interface Command {
  /** Counts from one in a session, and is never reused */
  readonly id: number
  readonly command: string | null
  readonly cwd: string | null
  /** Milliseconds since the Unix epoch. `finishedMs` is `null` while it runs */
  readonly startedMs: number | null
  readonly finishedMs: number | null
  readonly outcome: CommandOutcome
  readonly prompt: number | null
  readonly output: number | null
  readonly end: number | null
}

/**
 * One image where a program put it: its top-left cell, how much of the picture is drawn, and how large.
 *
 * `offsetX`, `offsetY`, `width` and `height` are in the pixels of a cell as the terminal was told one is
 * (`Placements.cellWidth`), which a view scales to the cell it draws.
 */
export interface Placement {
  readonly image: number
  /** Moves when the image's pixels do, so the ones a view holds are known stale */
  readonly generation: number
  /** Orders images that overlap; below zero is under the text */
  readonly z: number
  readonly line: number
  readonly column: number
  /** How many cells it covers */
  readonly columns: number
  readonly rows: number
  readonly offsetX: number
  readonly offsetY: number
  readonly width: number
  readonly height: number
  /** The part of the picture drawn, in its own pixels */
  readonly sourceX: number
  readonly sourceY: number
  readonly sourceWidth: number
  readonly sourceHeight: number
}

/** Every image in a session, and one cell in pixels as the terminal was told it is: zero when it was not */
export interface Placements {
  readonly cellWidth: number
  readonly cellHeight: number
  readonly placements: readonly Placement[]
}

/** An image's pixels: red, green, blue and alpha for each, rows top to bottom */
export interface Picture {
  readonly generation: number
  readonly width: number
  readonly height: number
  readonly pixels: Uint8ClampedArray
}

/** A cell on an absolute line */
export interface Position {
  readonly line: number
  readonly column: number
}

/** From `start` up to and including `end`, in reading order */
export interface Span {
  readonly start: Position
  readonly end: Position
}

export const KEY_ACTIONS = ['press', 'repeat', 'release'] as const
export type KeyAction = (typeof KEY_ACTIONS)[number]

/** Which modifier keys were down, as bits */
export const Modifier = { shift: 1, control: 2, alt: 4, super: 8, capsLock: 16, numLock: 32 } as const

/**
 * A key, as the keyboard reported it. **Not bytes**: what a key sends depends on modes only the terminal's
 * owner knows, so the owner encodes it.
 */
export interface KeyInput {
  readonly action: KeyAction
  /** `KeyboardEvent.code`: the physical key, whatever the layout */
  readonly code: string
  readonly modifiers: number
  /** What the key types with the modifiers applied, or empty for a key that types nothing */
  readonly text: string
  /** What the key types with no modifier, as a code point, or `0` */
  readonly unshifted: number
  /**
   * Alt went into typing `text` rather than being held with the key: the option key on a Mac, which is how
   * that keyboard reaches its third level. An owner sends such a key as its text, and any other key with
   * alt as alt.
   */
  readonly composed: boolean
}

export const POINTER_ACTIONS = ['press', 'release', 'move', 'wheelUp', 'wheelDown'] as const
export type PointerAction = (typeof POINTER_ACTIONS)[number]

/** The pointer, for a program that asked for it. Positions are pixels from the grid's top-left corner */
export interface PointerInput {
  readonly action: PointerAction
  /** `0` left, `1` middle, `2` right; `-1` where no button is part of it — a move with none down, a wheel */
  readonly button: number
  readonly modifiers: number
  readonly x: number
  readonly y: number
  /** One cell in pixels, which is what turns a position into a cell */
  readonly cellWidth: number
  readonly cellHeight: number
}

export type PasteOutcome =
  /** The text was sent */
  | 'pasted'
  /** The text could run a command — it holds a line ending where the program takes them as typed — and was not sent */
  | 'unsafe'

/** Input too large for the source to carry: a paste past what one message may hold. Nothing was sent */
export class TooLarge extends Error {}

/** What a view hears from its source */
export interface SourceListener {
  frame(frame: Frame): void
  /**
   * No more frames will come: the session is over, or — with `reason`, which is the owner's own words for
   * it — the session is there and could not be shown.
   */
  ended(reason?: string): void
}

/**
 * Where a view's rows come from and where its input goes.
 *
 * Every read is by absolute line, and answers for the epoch it was asked in: an answer from before a
 * reflow names lines that no longer exist, so it carries the epoch it describes.
 */
export interface TerminalSource {
  /**
   * Starts the frames. The returned function stops them.
   *
   * **An attachment's first frames say what of an attachment before can be trusted.** They describe the
   * whole screen, and name as stale (`staleFrom`) the history that may have changed while nobody was
   * listening, so a view lets go of nothing by hand. The epoch is the same where the lines still have the
   * numbers they had — the owner kept them, for another view or for this one — and another where they do
   * not, and then a view forgets its rows as it does for any epoch that moves.
   */
  attach(listener: SourceListener): () => void
  /** The frame with this sequence has been drawn, so the next may be sent */
  acknowledge(sequence: number): void
  /** Lines the view does not hold. No answer, as a rejection, when the lines have been numbered again since */
  fetch(first: number, count: number): Promise<Fetched>

  key(input: KeyInput): void
  /** Text that did not come from a key: what an input method composed */
  text(text: string): void
  pointer(input: PointerInput): void
  /** Rejects with `TooLarge` for text the source cannot carry, having sent none of it */
  paste(text: string, options?: { readonly allowUnsafe?: boolean }): Promise<PasteOutcome>
  focus(gained: boolean): void

  /** The word at a cell, or `null` where there is none */
  wordAt(position: Position): Promise<Span | null>
  /** The whole line a cell is on, across the rows it wrapped onto */
  lineAt(position: Position): Promise<Span | null>
  /**
   * The text of a span as a program wrote it: wrapped rows joined, trailing blanks dropped. No answer, as a
   * rejection, for a span numbered in an epoch that is over, and for text longer than an owner will carry
   */
  textOf(span: Span): Promise<string>
  /**
   * Every place `needle` appears, newest first — as many of the newest as the owner looks for. Letters match in
   * either case unless `caseSensitive` says the needle's own case only
   */
  search(needle: string, caseSensitive: boolean): Promise<readonly Span[]>
  /** The commands the shell has reported, oldest first. Empty for a shell that marks none */
  commands(): Promise<readonly Command[]>
  /** Where every image is. Their lines are in the epoch of the frame that prompted the question */
  images(): Promise<Placements>
  /** An image's pixels, or `null` for one that is gone */
  image(id: number): Promise<Picture | null>
}
