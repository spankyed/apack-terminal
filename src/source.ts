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

/** A colour as `0xRRGGBB`, or `null` where the terminal's own default applies */
export type Color = number | null

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
  /** For each character, an index into the styles its message came with; `0` is always `PLAIN` */
  readonly style: Uint16Array
  /** For each character, an index into the links its message came with; `0` is no link */
  readonly link: Uint16Array
}

export const CURSOR_SHAPES = ['block', 'bar', 'underline', 'hollow'] as const
export type CursorShape = (typeof CURSOR_SHAPES)[number]

export interface Cursor {
  /** The cell, within the screen: `row` counts from `firstActive` */
  readonly column: number
  readonly row: number
  readonly shape: CursorShape
  readonly blinking: boolean
}

/** What a view has to know about the program's modes, which is very little: the terminal's owner encodes input */
export interface Modes {
  /** A full-screen program is showing; it has no history to scroll */
  readonly alternateScreen: boolean
  /** The program asked for the mouse, so a click is its business and not a selection */
  readonly mouse: boolean
}

/** Rows and the tables their `style` and `link` indices point into */
export interface Rows {
  readonly styles: readonly Style[]
  /** `links[0]` is unused, so that a `link` of `0` means none */
  readonly links: readonly string[]
  readonly rows: readonly Row[]
}

/**
 * The screen as it now stands, as a change from the frame a view last acknowledged.
 *
 * **A frame is not a unit of output.** The owner sends one when the screen has changed and the last has
 * been acknowledged, so a program that writes faster than a view draws produces fewer frames, not a
 * backlog: each carries everything that changed since, however much that was.
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
  /** Absent while the program has hidden it */
  readonly cursor: Cursor | null
  readonly modes: Modes
  /**
   * History lines from here up to `firstActive` changed since the last frame and are not among `rows`: a
   * view that kept any of them must let them go. Absent when none did.
   */
  readonly staleFrom?: number
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
}

export const POINTER_ACTIONS = ['press', 'release', 'move'] as const
export type PointerAction = (typeof POINTER_ACTIONS)[number]

/** The pointer, for a program that asked for it. Positions are pixels from the grid's top-left corner */
export interface PointerInput {
  readonly action: PointerAction
  /** `0` left, `1` middle, `2` right, `3` and up as the browser numbers them; `-1` for a move with none down */
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

/** What a view hears from its source */
export interface SourceListener {
  frame(frame: Frame): void
  /** The session is over; no more frames will come */
  ended(): void
}

/**
 * Where a view's rows come from and where its input goes.
 *
 * Every read is by absolute line, and answers for the epoch it was asked in: an answer from before a
 * reflow names lines that no longer exist, so it carries the epoch it describes.
 */
export interface TerminalSource {
  /** Starts the frames. The returned function stops them */
  attach(listener: SourceListener): () => void
  /** The frame with this sequence has been drawn, so the next may be sent */
  acknowledge(sequence: number): void
  /** Lines the view does not hold. Lines no longer held by the owner are simply absent from the answer */
  fetch(first: number, count: number): Promise<Rows & { readonly epoch: number }>

  key(input: KeyInput): void
  /** Text that did not come from a key: what an input method composed */
  text(text: string): void
  pointer(input: PointerInput): void
  paste(text: string, options?: { readonly allowUnsafe?: boolean }): Promise<PasteOutcome>
  focus(gained: boolean): void

  /** The word at a cell, or `null` where there is none */
  wordAt(position: Position): Promise<Span | null>
  /** The whole line a cell is on, across the rows it wrapped onto */
  lineAt(position: Position): Promise<Span | null>
  /** The text of a span as a program wrote it: wrapped rows joined, trailing blanks dropped */
  textOf(span: Span): Promise<string>
  /** Every place `needle` appears, newest first */
  search(needle: string): Promise<readonly Span[]>
}
