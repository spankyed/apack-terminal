import { CanvasPainter } from './canvas-painter.ts'
import { blinks, drawRow, sessionTheme, type RowDrawing, type SelectedCells, type Theme } from './draw.ts'
import { Input, modifiersOf } from './input.ts'
import { linkAt, linkRuns, urls, type Detector, type Link, type LinkRun } from './links.ts'
import { fontOf, type Font, type Painter, type Scene } from './painter.ts'
import { RowStore } from './row-store.ts'
import { Selection, type Granularity } from './selection.ts'
import { Modifier, PLAIN, TooLarge, type Command, type Frame, type Picture, type Placements, type PointerInput, type CursorShape, type Position, type Row, type SessionColors, type Span, type TerminalSource } from './source.ts'
import { Viewport } from './viewport.ts'

/**
 * A run of lines a host folds away. The first line stays on screen as a header, which stands for the lines after it up
 * to `last`: those are neither drawn nor pointed at, and what is copied from the fold is still all of its text.
 */
export interface Fold {
  readonly first: number
  readonly last: number
  /** What the header says the fold holds, after the count of lines. Left out, the header says only the count */
  readonly label?: string
}

/** How long the cursor is shown, and then hidden, while it blinks */
export const BLINK_MS = 530

/**
 * Whether a row has blinking text, asked once per row it is. A row is replaced when it changes, so a row
 * held is answered from memory and nothing rescans every cell of a screen on every frame.
 */
const blinkingRows = new WeakMap<Row, boolean>()
function blinksIn(row: Row): boolean {
  let answer = blinkingRows.get(row)
  if (answer === undefined) {
    answer = blinks(row)
    blinkingRows.set(row, answer)
  }
  return answer
}

/**
 * How many lines of history are asked for at once, in pages that start on a multiple of it. A view scrolling
 * a line at a time crosses into a new page once in this many lines, and asks once.
 */
export const PAGE_LINES = 256
/** Matches a search highlights and steps through: the rest are counted as more, as "1000+" */
export const MATCH_HIGHLIGHTS = 1000
/** At most one refresh of the matches in this long: what changed while it waits is the next one's to find */
const MATCH_REFRESH_MS = 200

/** How fast a selection dragged past an edge scrolls: lines a second, for each row the pointer is past it */
const DRAG_LINES_PER_SECOND = 15

/** How soon after a press, in milliseconds, and how near it, in pixels, another is the same click again */
const CLICK_AGAIN_MS = 500
const CLICK_AGAIN_PIXELS = 4

/** The shortest the scrollbar's thumb is drawn, in pixels, however much history it stands for */
const THUMB_MIN = 20

/**
 * Whether the cursor blinks while the view has the keyboard. `on` and `off` are the app's setting, and
 * override what the program asks for: in the core, DECSCUSR's blink and DEC mode 12 are one setting, so
 * `program` is whichever of those the program last set.
 */
export type CursorBlink = 'on' | 'off' | 'program'

/**
 * What hovering a link shows of where it opens, as the pointer's title: `osc8` for a link a program declared
 * with a hyperlink sequence, `all` for any link (text that looks like a URL, or what a detector found), and
 * `off` for none. The pointer is a pointer over every link either way.
 */
export type LinkPreview = 'off' | 'osc8' | 'all'

export interface ViewOptions {
  /** The typeface a view starts in; `setFont` changes it */
  readonly font: Font
  /** The colours a view starts in; `setTheme` changes them */
  readonly theme: Theme
  /**
   * Turns an image's pixels into something the painter draws. The default makes a bitmap; a painter that
   * draws another way gives its own, and so does a page with no canvas to make one on.
   */
  readonly picture?: (read: Picture) => Promise<CanvasImageSource | null>
  /**
   * Makes the painter. Left out, a view draws on a 2D canvas; a host that has a GPU painter passes it, and
   * a case passes one that records.
   */
  readonly painter?: (font: Font) => Painter
  /**
   * The view measured how many cells fit where it is mounted. The grid a session runs at is the owner's to
   * decide — several views may be showing it — so this is a report, and the view goes on drawing the grid
   * its frames describe.
   */
  readonly onFit?: (fit: { columns: number; rows: number; cellWidth: number; cellHeight: number }) => void
  /** Whether the view is following the output changed, which a host draws a "jump to bottom" control from */
  readonly onFollowingChange?: (following: boolean) => void
  /**
   * No more frames will come: the session ended, or — with `reason`, in the source's words — it is there
   * and could not be shown.
   */
  readonly onEnded?: (reason?: string) => void
  /**
   * A key the host wants for itself. Answering `true` keeps it from the program. Asked before anything
   * else, for every key.
   */
  readonly onKey?: (event: KeyboardEvent) => boolean
  /**
   * A paste holds what could run a command. Answer whether the person, shown the text, said to paste it
   * anyway. Left out, such a paste is dropped.
   */
  readonly confirmPaste?: (text: string) => Promise<boolean>
  /** A paste the person made was too large for the source to carry, and none of it was sent */
  readonly onPasteRefused?: (error: TooLarge) => void
  /**
   * Whether typing, composed text and a paste bring a view that is scrolled back down to the screen, where
   * what was typed appears. Left out, they do.
   */
  readonly followOnInput?: boolean
  /**
   * The shape the cursor is drawn in while the view does not have the keyboard. Left out, a hollow block.
   */
  readonly cursorInactive?: CursorShape
  /**
   * Whether the cursor blinks while the view has the keyboard (`CursorBlink`). Left out, `program`: the
   * blink the broker reports, as the program left it. A host that has an app setting passes it, with
   * "unset" resolved to the value that setting means.
   */
  readonly cursorBlink?: CursorBlink
  /**
   * Whether a Mac's option key is alt rather than the key that composes characters: a key held with it is
   * then sent as what the key types without it. Left out, option composes, as it does everywhere else on
   * a Mac.
   */
  readonly optionIsAlt?: boolean
  /** The person opened a link: clicked it with the platform's modifier held */
  readonly onLink?: (link: Link, event: MouseEvent) => void
  /** Which links show where they open when the pointer is over them (`LinkPreview`). Left out, `osc8` */
  readonly linkPreview?: LinkPreview
  /** What else on a row can be opened, beyond URLs and what a program declared */
  readonly detectors?: readonly Detector[]
  /** Whether anything is selected changed, which a host draws its copy control from */
  readonly onSelectionChange?: (selected: boolean) => void
  /** Text was copied. Left out, it is written to the clipboard the page has */
  readonly onCopy?: (text: string) => void
  /** The commands the shell has reported, each time the view hears them: a host draws its command list from this */
  readonly onCommands?: (commands: readonly Command[]) => void
  /**
   * Copies a selection as the pointer is let go, as iTerm2 and Ghostty can, so the text is on the clipboard without a
   * key. Off unless set, as macOS Terminal has it: a selection overwriting what was copied is a surprise to most people.
   * `setCopyOnSelect` changes it while the view is up.
   */
  readonly copyOnSelect?: boolean
  /** Whether this is a Mac, where the command key is the app's. Left out, asked of the browser */
  readonly mac?: boolean
  /**
   * A strip this many pixels wide left of the first column, where each command the shell ran is marked
   * beside its prompt, coloured by how it ended. Left out, there is none and nothing is marked.
   */
  readonly gutter?: number
}

interface Kept {
  readonly row: Row
  readonly selected: string
  readonly drawing: RowDrawing
}

/** A button that is down: what it is doing, and where it went down */
interface Pressed {
  readonly button: number
  readonly reporting: boolean
  readonly at: Position
  moved: boolean
  /** Whether Alt (Option on a Mac) was held when the press began: a drag over cells then selects a rectangle */
  readonly block: boolean
}

/** A point on the page, as a pointer event gives one */
interface Point {
  readonly clientX: number
  readonly clientY: number
}

/**
 * A picture a view holds for an image, or is asking for. `generation` is the one `picture` is of, and
 * `refused` the placements that were current when an answer was not the pixels they name.
 */
interface HeldPicture {
  generation: number
  picture: CanvasImageSource | null
  asking: boolean
  refused: Placements | null
}

/** A command, by the line it is on */
interface CommandLine {
  readonly line: number
  readonly command: Command
}

/**
 * A terminal, drawn.
 *
 * **It draws when something changed and at no other time.** A frame arrives, the user scrolls, a
 * selection moves, the cursor blinks: each asks for one paint on the next animation frame, and with
 * nothing asking there is no loop running. A frame is acknowledged once it has been painted, which is what
 * lets the source send the next — so a view that is slow to draw is sent fewer frames rather than more
 * than it can use, and a view that is not mounted is sent none and draws nothing.
 */
export class TerminalView {
  readonly element: HTMLDivElement
  readonly store = new RowStore()
  readonly viewport = new Viewport(this.store)

  private readonly painter: Painter
  private font: Font
  /**
   * The colours the view is drawn in: the host's theme, under the colours the session's program set for
   * itself and the reverse video it may have the screen in (`sessionTheme`). Made again when any of the
   * three changes, and the same object until then, which is how a painter knows the colours are the same.
   */
  private theme: Theme
  private hostTheme: Theme
  /** What the source last said the program's own colours are, and whether it has the screen reversed */
  private colors: SessionColors | null = null
  private reverseVideo = false
  /**
   * The drawings of the rows in view and a screenful either side, each kept while its row looks the same.
   * A painter knows a row is unchanged by its drawing being the same object as last time.
   */
  private readonly drawings = new Map<number, Kept>()
  /** The pages of history asked for and not yet answered. Of this epoch only */
  private readonly asking = new Set<number>()
  private frame: Frame | null = null
  private columns = 0
  private rows = 0
  /** The frame to acknowledge once it is on the screen */
  private unacknowledged: number | null = null
  private detach: (() => void) | null = null
  /**
   * Whether the view is taking frames, from before it asks for them: a source may hand over the first
   * inside the asking. Counts the attachments, so an answer can tell it was asked for in one that is over.
   */
  private listening = false
  private attachment = 0
  /** The session is over: nothing more is asked of its source */
  private ended = false
  private scheduled: number | null = null
  private observer: ResizeObserver | null = null
  /** Stops listening for the display and the page's fonts changing, while mounted */
  private unwatch: (() => void) | null = null
  /** The pixel ratio the painter was last sized for, or `null` before it has been */
  private pixelRatio: number | null = null
  /** How tall the view is on the page, as last measured */
  private height = 0
  private blink: ReturnType<typeof setInterval> | null = null
  private cursorShown = true
  /** The clock blinking text goes by, while any is in view, and which half of a blink it is in */
  private textBlink: ReturnType<typeof setInterval> | null = null
  private textShown = true
  private focused = false
  private wasFollowing = true
  private disposed = false
  private wasSelected = false
  /** The frame the page was last told is drawn */
  private drawn = -1
  /**
   * What is being searched for, where it was found, newest first, and which of those the person is on.
   * `finding` counts the searches a person asked for, so an answer to one that was overtaken says nothing.
   */
  private needle = ''
  private matches: readonly Span[] = []
  /** Whether the search found more than `MATCH_HIGHLIGHTS` matches, of which the first are held */
  private moreMatches = false
  private matchCase = false
  private refreshTimer: ReturnType<typeof setTimeout> | undefined
  private currentMatch = -1
  private finding = 0
  /** A search made again because rows changed is on its way, and whether rows changed again since */
  private refreshing = false
  private refreshAgain = false
  private readonly gutter: number
  /** The commands the shell has reported, and the frame's count that said to ask for them */
  private commandList: readonly Command[] = []
  private folds: readonly Fold[] = []
  private foldRows = new Map<string, Row>()
  private commandsSeen = -1
  /** Those of them that begin on a line, and those with a prompt on one: each in the order of its lines */
  private commandStarts: readonly CommandLine[] = []
  private commandPrompts: readonly CommandLine[] = []
  /** Where the session's images are, as last told, and the generation of the frame that prompted asking */
  private placed: Placements = { cellWidth: 0, cellHeight: 0, placements: [] }
  private imagesSeen = -1
  private readonly pictures = new Map<number, HeldPicture>()
  /** The generation of each image last asked for, which is the one an answer is held to */
  private readonly wanted = new Map<number, number>()
  private readonly mac: boolean
  private readonly detectors: readonly Detector[]
  private readonly input: Input
  /** The rows on screen as text, for whatever reads a page rather than looks at it */
  private readonly mirror: HTMLDivElement
  private readonly mirrored: Array<Row | null | undefined> = []
  private readonly thumb: HTMLDivElement
  /** The thumb as last drawn, so the page is told only what changed: its height, its top, whether it shows */
  private thumbDrawn = { height: 0, top: 0, shown: false }
  /** Ends a drag of the thumb that is under way */
  private thumbRelease: (() => void) | null = null
  /** The link the pointer is over, the line it is on, and the cells it covers on each line it runs across */
  private hovered: { line: number; link: Link; runs: LinkRun[] } | null = null
  /** The cell the pointer was over when that was last worked out, and the row that was on its line then */
  private hoverAt: { line: number; column: number; row: Row | undefined } | null = null
  private pressed: Pressed | null = null
  /**
   * The last press: when, where on the page, with which button, and how many presses it was the last of.
   * A pointer event does not say how many times it was clicked — only the mouse events a view prevents
   * do — so a view counts.
   */
  private lastPress: { time: number; clientX: number; clientY: number; button: number; count: number } | null = null
  /** The cell a program was last told the pointer is over, and with which button: it is told of each once */
  private reported: { column: number; row: number; button: number } | null = null
  /** A selection is being dragged past the top or bottom edge: where the pointer is, and the frame that scrolls */
  private dragScroll: { point: Point; frame: number; at: number | null } | null = null
  /** Fractions of a wheel notch not yet sent to a program that takes whole ones */
  private wheelOwed = 0
  /** Whether a selection is copied as the pointer is let go: the option at construction, then `setCopyOnSelect` */
  private copyOnSelect: boolean
  /** The cursor's shape without the keyboard, its blink and the link previews: the options at construction, then their setters */
  private cursorInactive: CursorShape
  private cursorBlink: CursorBlink
  private linkPreview: LinkPreview
  /** What is selected. It asks the source what a word or a line is and draws nothing itself */
  readonly selection: Selection

  constructor(private readonly source: TerminalSource, private readonly options: ViewOptions) {
    this.mac = options.mac ?? /Mac|iPhone|iPad/.test(navigator.platform)
    this.gutter = options.gutter ?? 0
    this.copyOnSelect = options.copyOnSelect ?? false
    this.cursorInactive = options.cursorInactive ?? 'hollow'
    this.cursorBlink = options.cursorBlink ?? 'program'
    this.linkPreview = options.linkPreview ?? 'osc8'
    this.font = options.font
    this.theme = options.theme
    this.hostTheme = options.theme
    this.detectors = [urls, ...(options.detectors ?? [])]
    this.element = document.createElement('div')
    this.element.style.cssText = 'position:relative;width:100%;height:100%;overflow:hidden;cursor:text'
    this.painter = (options.painter ?? ((font) => new CanvasPainter(font)))(options.font)
    this.painter.canvas.style.display = 'block'
    this.painter.whenCleared?.(() => this.schedule())
    this.element.appendChild(this.painter.canvas)

    this.selection = new Selection(source, () => this.columns, () => {
      const selected = this.selection.span !== null
      if (selected !== this.wasSelected) {
        this.wasSelected = selected
        this.options.onSelectionChange?.(selected)
      }
      this.schedule()
    }, () => ({ oldest: this.store.oldest, end: this.store.end }))
    this.input = new Input(source, {
      mac: this.mac,
      optionIsAlt: options.optionIsAlt,
      intercept: (event) => this.shortcut(event) || (options.onKey?.(event) ?? false),
      paste: (text) => {
        void this.paste(text).catch((error: unknown) => {
          if (error instanceof TooLarge) this.options.onPasteRefused?.(error)
        })
      },
      copy: () => { void this.copy() },
      focused: (focused) => this.setFocused(focused),
      typed: () => this.typed(),
      compositionStarted: () => this.selection.clear(),
    })
    this.element.appendChild(this.input.field)

    this.mirror = document.createElement('div')
    this.mirror.setAttribute('role', 'document')
    this.mirror.setAttribute('aria-label', 'Terminal')
    this.mirror.dataset.terminalText = ''
    // Read, not seen: the canvas is what is looked at
    this.mirror.style.cssText = 'position:absolute;left:0;top:0;width:1px;height:1px;overflow:hidden;'
      + 'clip-path:inset(50%);white-space:pre;pointer-events:none;user-select:none'
    this.element.appendChild(this.mirror)

    this.thumb = document.createElement('div')
    // Not there to be pressed while it is not there to be seen
    this.thumb.style.cssText = 'position:absolute;right:2px;width:6px;border-radius:3px;top:0;height:0;'
      + 'background:currentColor;opacity:0;pointer-events:none;transition:opacity 120ms;cursor:default;z-index:2'
    this.element.appendChild(this.thumb)
    this.themed()

    this.element.addEventListener('wheel', this.wheel, { passive: false })
    this.element.addEventListener('pointerdown', this.pointerDown)
    this.element.addEventListener('pointermove', this.pointerMove)
    this.element.addEventListener('pointerup', this.pointerUp)
    this.element.addEventListener('pointercancel', this.pointerLost)
    this.element.addEventListener('lostpointercapture', this.pointerLost)
    this.element.addEventListener('pointerleave', this.pointerLeave)
    this.element.addEventListener('contextmenu', this.contextMenu)
    this.thumb.addEventListener('pointerdown', this.thumbDown)
  }

  /**
   * Draws in other colours from here on: every row a view holds is drawn again, and none is asked for.
   * A colour the session's program set for itself stays the program's, over the new theme as over the old.
   *
   * The theme is taken as it is when given: a host that changes one it gave gives it again.
   */
  /** Changes whether a selection is copied as the pointer is let go (`copyOnSelect`) */
  setCopyOnSelect(on: boolean): void {
    this.copyOnSelect = on
  }

  /**
   * Changes the shape the cursor is drawn in while the view does not have the keyboard (`cursorInactive`). The
   * view is not remade: its scroll position and selection stay, and the next paint draws the new shape.
   */
  setCursorInactive(style: CursorShape): void {
    this.cursorInactive = style
    this.schedule()
  }

  /**
   * Changes whether the cursor blinks (`cursorBlink`). The blink starts over in the new mode, as it does after
   * anything is typed, and the view keeps its scroll position and selection.
   */
  setCursorBlink(mode: CursorBlink): void {
    this.cursorBlink = mode
    this.blinkAgain()
    this.schedule()
  }

  /** Changes which links show where they open (`linkPreview`), and the link under the pointer shows it at once */
  setLinkPreview(mode: LinkPreview): void {
    this.linkPreview = mode
    if (this.hovered) this.element.title = this.previews(this.hovered.link) ? this.hovered.link.target : ''
  }

  setTheme(theme: Theme): void {
    // A copy, so that the same object given again with other colours in it is other colours
    this.hostTheme = { ...theme, palette: [...theme.palette] }
    this.recolour()
  }

  /** The theme, the program's colours or the reverse video changed: everything is drawn again in what they make */
  private recolour(): void {
    const theme = sessionTheme(this.hostTheme, this.colors, this.reverseVideo)
    if (theme === this.theme) return
    this.theme = theme
    this.themed()
    this.drawings.clear()
    this.schedule()
  }

  /** Draws in another typeface from here on, and reports what fits now that a cell is another size */
  setFont(font: Font): void {
    const before = this.painter.metrics
    this.font = font
    this.painter.setFont(font)
    this.themed()
    const { cellWidth, cellHeight } = this.painter.metrics
    if (cellWidth !== before.cellWidth || cellHeight !== before.cellHeight) this.fit()
    this.schedule()
  }

  /** What of the page around the canvas is in the theme's colours and the view's font */
  private themed(): void {
    const { background, foreground } = this.theme
    this.element.style.background = hex(background)
    this.thumb.style.color = hex(foreground)
    this.input.appearance({ font: fontOf(this.font, false, false), color: hex(foreground), background: hex(background) })
  }

  /**
   * Selects everything the terminal holds, history included. Copying it asks the source for all of that as
   * one text, and a source carries only so much in one answer — a broker about eight megabytes — past
   * which `copy` answers `false` and nothing is copied.
   */
  selectAll(): void {
    if (!this.frame) return
    this.selection.select({
      start: { line: this.store.oldest, column: 0 },
      end: { line: this.store.firstActive + this.rows - 1, column: Math.max(0, this.columns - 1) },
    })
  }

  /**
   * Finds every place `needle` appears, in the screen and all of its history, and goes to the newest.
   * Answers how many there are — of as many as the source looks for, which for a broker is the newest two
   * thousand. An empty needle clears the search.
   *
   * What is found is kept true to the screen: when rows change, the search is made again — one at a time,
   * and once more after it if rows changed while it was on its way — and the person stays on the match
   * they were on.
   */
  async find(needle: string, options: { matchCase?: boolean } = {}): Promise<number> {
    const asked = (this.finding += 1)
    this.needle = needle
    this.matchCase = options.matchCase ?? false
    const found = (await this.search(needle)) ?? []
    // A search that was overtaken by a later one has nothing to say
    if (asked !== this.finding) return this.matches.length
    this.setMatches(found)
    this.currentMatch = this.matches.length > 0 ? 0 : -1
    this.drawings.clear()
    this.showMatch()
    return this.matches.length
  }

  /** Holds what a search found, up to `MATCH_HIGHLIGHTS` of it */
  private setMatches(found: readonly Span[]): void {
    this.matches = found.slice(0, MATCH_HIGHLIGHTS)
    this.moreMatches = found.length > MATCH_HIGHLIGHTS
  }

  /** Where the source finds `needle`, or `null` when it could not say */
  private async search(needle: string): Promise<readonly Span[] | null> {
    if (needle === '') return []
    try {
      return await this.source.search(needle, this.matchCase)
    } catch {
      return null
    }
  }

  /**
   * Rows changed under a search: what it found may no longer be where it was, or there at all. At most one
   * refresh per `MATCH_REFRESH_MS`, so a stream of output costs one search a window, and the last change is
   * found once it is quiet.
   */
  private refreshMatches(): void {
    if (this.needle === '' || this.refreshTimer !== undefined) return
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = undefined
      this.runRefresh()
    }, MATCH_REFRESH_MS)
  }

  private runRefresh(): void {
    if (this.needle === '') return
    if (this.refreshing) {
      this.refreshAgain = true
      return
    }
    this.refreshing = true
    this.refreshAgain = false
    const asked = this.finding
    void this.search(this.needle).then((found) => {
      this.refreshing = false
      if (this.disposed) return
      // Nothing to say when the person has searched again since, or the source could not answer
      if (found && asked === this.finding) {
        const on = this.matches[this.currentMatch]
        const still = on ? found.findIndex((match) => match.start.line === on.start.line && match.start.column === on.start.column) : -1
        // On the match they were on; failing that the nearest there is, which for a search that had found
        // nothing is the newest
        const current = found.length === 0 ? -1 : still !== -1 ? still : Math.max(0, Math.min(this.currentMatch, found.length - 1))
        const held = found.slice(0, MATCH_HIGHLIGHTS)
        const same = current === this.currentMatch && held.length === this.matches.length && held.every((match, index) => sameSpan(match, this.matches[index]!))
        this.setMatches(found)
        this.currentMatch = current
        // A row's drawing is kept by what matches are on it, so only the rows whose matches moved are
        // drawn again — and with none moved, nothing is
        if (!same) this.schedule()
      }
      if (this.refreshAgain) this.refreshMatches()
    })
  }

  /** Which match the person is on, counting from one at the newest, and how many there are */
  get found(): { current: number; total: number; more: boolean } {
    return { current: this.currentMatch + 1, total: this.matches.length, more: this.moreMatches }
  }

  /** Goes to the next match: an older one for `1`, a newer one for `-1`, round to the other end past the last */
  findNext(direction: -1 | 1 = 1): void {
    if (this.matches.length === 0) return
    this.currentMatch = (this.currentMatch + direction + this.matches.length) % this.matches.length
    this.drawings.clear()
    this.showMatch()
  }

  /** Brings the match the person is on into view, in the middle where there is room */
  private showMatch(): void {
    const match = this.matches[this.currentMatch]
    if (match) {
      const [first, count] = this.viewport.visible
      if (match.start.line < first || match.start.line >= first + count) {
        this.scrollTo(match.start.line - Math.floor(this.rows / 2))
      }
    }
    this.schedule()
  }

  /**
   * The cells of `line` that matches cover, each with whether it is the match the person is on. Matches
   * are newest first and do not overlap, so the ones on a line are side by side among them and are found
   * by halving.
   */
  private matchesOn(line: number): Array<{ from: number; to: number; current: boolean }> {
    const matches = this.matches
    // The first that starts on this line or above it: every one before it starts below
    let low = 0
    let high = matches.length
    while (low < high) {
      const middle = (low + high) >> 1
      if (matches[middle]!.start.line > line) low = middle + 1
      else high = middle
    }
    const on: Array<{ from: number; to: number; current: boolean }> = []
    for (let index = low; index < matches.length; index += 1) {
      const match = matches[index]!
      if (match.end.line < line) break
      on.push({
        from: line === match.start.line ? match.start.column : 0,
        to: line === match.end.line ? match.end.column + 1 : this.columns,
        current: index === this.currentMatch,
      })
    }
    return on
  }

  /** The commands the shell has reported, as the view last heard them */
  get commands(): readonly Command[] {
    return this.commandList
  }

  /**
   * Folds the lines a host names. Each fold is one header row in the lines it starts at, and its other lines are not drawn:
   * a host draws its command blocks from this. Folds may not overlap.
   */
  setFolds(folds: readonly Fold[]): void {
    const sorted = folds.filter((fold) => fold.last > fold.first).sort((one, other) => one.first - other.first)
    for (let index = 1; index < sorted.length; index += 1) {
      if (sorted[index]!.first <= sorted[index - 1]!.last) throw new RangeError('folds overlap')
    }
    this.folds = sorted
    this.foldRows.clear()
    this.drawings.clear()
    this.schedule()
  }

  /** The fold a line is hidden in: a header is not, and neither is a line outside every fold */
  private hiddenIn(line: number): Fold | undefined {
    return this.folds.find((fold) => line > fold.first && line <= fold.last)
  }

  /**
   * The rows the view draws, top down: the absolute line each is, a fold's header standing for the line it starts at. The
   * window is as many rows as the screen, which is more lines than that where folds hide some.
   */
  private layout(): { first: number; lines: number[] } {
    const [top, count] = this.viewport.visible
    const hidden = this.hiddenIn(top)
    const first = hidden ? hidden.first : top
    const lines: number[] = []
    let line = first
    while (lines.length < count) {
      const fold = this.hiddenIn(line)
      if (fold) line = fold.last + 1
      else {
        lines.push(line)
        line += 1
      }
    }
    return { first, lines }
  }

  /** The number of rows from line `from` up to line `to`, folded lines taking none: negative when `to` is above `from` */
  private rowsBetween(from: number, to: number): number {
    if (to < from) return -this.rowsBetween(to, from)
    let rows = to - from
    for (const fold of this.folds) {
      const hidden = Math.min(fold.last + 1, to) - Math.max(fold.first + 1, from)
      if (hidden > 0) rows -= hidden
    }
    return rows
  }

  /** The header row of a fold, drawn as its label in the place of the line it starts at */
  private foldRow(fold: Fold): Row {
    const text = `▸ ${fold.label ?? `${fold.last - fold.first} lines folded`}`
    const key = `${fold.first}:${fold.last}:${text}`
    const known = this.foldRows.get(key)
    if (known) return known
    const characters = [...text]
    const row: Row = {
      line: fold.first,
      wrapped: false,
      prompt: false,
      text: characters,
      column: Uint16Array.from(characters, (_, index) => index),
      width: Uint8Array.from(characters, () => 1),
      style: new Uint16Array(characters.length),
      link: new Uint16Array(characters.length),
      styles: [PLAIN],
      links: [''],
    }
    this.foldRows.set(key, row)
    return row
  }

  private setCommands(commands: readonly Command[]): void {
    this.commandList = commands
    this.options.onCommands?.(commands)
    const starts: CommandLine[] = []
    const prompts: CommandLine[] = []
    for (const command of commands) {
      const begins = command.prompt ?? command.output
      if (begins !== null) starts.push({ line: begins, command })
      if (command.prompt !== null) prompts.push({ line: command.prompt, command })
    }
    this.commandStarts = starts
    this.commandPrompts = prompts
  }

  /**
   * The command whose prompt, line or output is on `line`: the last one that began at or before it.
   * `null` above the first command, and for a shell that marks none.
   */
  commandAt(line: number): Command | null {
    return this.commandStarts[firstAfter(this.commandStarts, line) - 1]?.command ?? null
  }

  /** The cells a command's output covers, or `null` where it printed nothing or its rows are gone */
  outputOf(command: Command): Span | null {
    if (command.output === null || !this.frame) return null
    // To where it ended; while it runs, and where its end was never marked, to the bottom of the screen
    const last = command.end !== null ? command.end - 1 : this.store.firstActive + this.rows - 1
    if (last < command.output) return null
    return { start: { line: command.output, column: 0 }, end: { line: last, column: Math.max(0, this.columns - 1) } }
  }

  /** Selects what a command printed. Answers whether there was anything to select */
  selectOutput(command: Command): boolean {
    const span = this.outputOf(command)
    if (span) this.selection.select(span)
    return span !== null
  }

  /**
   * Scrolls to the prompt of the command before or after what is at the top of the view. From the
   * oldest, back stays there; from the newest, forward returns to the screen and follows it.
   */
  scrollToCommand(direction: -1 | 1): void {
    const top = Math.floor(this.viewport.top)
    const prompts = this.commandPrompts
    const target = direction === -1 ? prompts[firstAfter(prompts, top - 1) - 1] : prompts[firstAfter(prompts, top)]
    if (target !== undefined) this.scrollTo(target.line)
    else if (direction === 1) this.follow()
  }

  /** The keys that are the view's own: moving between commands */
  private shortcut(event: KeyboardEvent): boolean {
    const held = this.mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && event.shiftKey
    if (!held || event.altKey || (event.code !== 'ArrowUp' && event.code !== 'ArrowDown')) return false
    this.scrollToCommand(event.code === 'ArrowUp' ? -1 : 1)
    return true
  }

  /**
   * Lets go of the pictures of images nothing shows any more: none places it, and no row held shows part
   * of it. Known once placements are told, which is whenever the images change.
   */
  private dropPictures(): void {
    const shown = this.store.pictured()
    for (const placement of this.placed.placements) shown.add(placement.image)
    for (const [image, held] of this.pictures) {
      if (shown.has(image)) continue
      release(held.picture)
      this.pictures.delete(image)
      this.wanted.delete(image)
    }
  }

  /**
   * Asks for the pixels of the images in view, or within a screenful of it, that this view does not hold
   * at the generation shown: the ones placed there, and the ones the rows there show part of. One far up
   * the history costs nothing until it is scrolled to.
   */
  private wantPictures(first: number, count: number): void {
    if (this.ended || !this.listening) return
    for (const placement of this.placed.placements) {
      if (placement.line + placement.rows <= first - count || placement.line >= first + count * 2) continue
      this.wantPicture(placement.image, placement.generation)
    }
    for (let line = first - count; line < first + count * 2; line += 1) {
      for (const piece of this.store.row(line)?.images?.pieces ?? []) this.wantPicture(piece.image, piece.generation)
    }
  }

  /**
   * Asks for one image's pixels, once.
   *
   * **An answer that is not what was asked for is not asked for again until the placements are told.**
   * The image is gone, or could not be made into a picture, or was replaced since — and asking again gets
   * the same answer, on every paint, until something has changed. What changes is the images, and the
   * placements are told again whenever they do, which is when it is asked for once more.
   */
  private wantPicture(image: number, generation: number): void {
    this.wanted.set(image, generation)
    let held = this.pictures.get(image)
    if (!held) {
      held = { generation: -1, picture: null, asking: false, refused: null }
      this.pictures.set(image, held)
    }
    if (held.generation === generation || held.asking || held.refused === this.placed) return
    const entry = held
    entry.asking = true
    const attachment = this.attachment
    const made = this.source.image(image).then(async (read) => ({
      picture: read ? await (this.options.picture ?? pictureOf)(read) : null,
      generation: read?.generation ?? -1,
    }))
    void made.then(({ picture, generation: got }) => {
      entry.asking = false
      // Let go of while its pixels were on their way: a picture nothing holds is closed, not leaked
      if (this.disposed || this.pictures.get(image) !== entry) return release(picture)
      if (picture) {
        release(entry.picture)
        entry.picture = picture
        entry.generation = got
      }
      // No answer for want of an attachment says nothing about the image: the next attachment asks
      if (!picture && (attachment !== this.attachment || !this.listening)) return
      if (!picture || got !== this.wanted.get(image)) entry.refused = this.placed
      this.schedule()
    }, () => {
      entry.asking = false
      if (attachment === this.attachment && this.listening) entry.refused = this.placed
    })
  }

  /**
   * The images with any part in view, lowest first, in the pixels this view draws a cell in: the ones
   * placed there, and the parts of images the rows there show through placeholder cells.
   */
  private imagesIn(first: number, span: number, lines: number[]): NonNullable<Scene['images']> {
    const { cellWidth, cellHeight } = this.painter.metrics
    // What the terminal measured an image in, against what this view draws: the same, where it was not told
    const scaleX = this.placed.cellWidth > 0 ? cellWidth / this.placed.cellWidth : 1
    const scaleY = this.placed.cellHeight > 0 ? cellHeight / this.placed.cellHeight : 1
    // A piece is under the text and over whatever else is: the core draws one at a z of minus one
    const images: Array<NonNullable<Scene['images']>[number] & { z: number }> =
      this.piecesIn(first, lines).map((piece) => ({ ...piece, z: -1 }))
    for (const placement of this.placed.placements) {
      if (placement.line + placement.rows <= first || placement.line >= first + span) continue
      if (this.hiddenIn(placement.line)) continue
      const picture = this.pictures.get(placement.image)?.picture
      if (!picture) continue
      images.push({
        picture,
        x: placement.column * cellWidth + placement.offsetX * scaleX,
        y: this.rowsBetween(first, placement.line) * cellHeight + placement.offsetY * scaleY,
        width: placement.width * scaleX,
        height: placement.height * scaleY,
        source: { x: placement.sourceX, y: placement.sourceY, width: placement.sourceWidth, height: placement.sourceHeight },
        above: placement.z >= 0,
        z: placement.z,
      })
    }
    return images.sort((one, other) => one.z - other.z)
  }

  /** The parts of images the rows in view show, each where its cells are, under the text */
  private piecesIn(first: number, lines: number[]): NonNullable<Scene['images']> {
    const { cellWidth, cellHeight } = this.painter.metrics
    const shown: Array<NonNullable<Scene['images']>[number]> = []
    for (const line of lines) {
      const images = this.store.row(line)?.images
      // A fold's line is drawn as the fold, not as the row that begins what it hides
      if (images === undefined || this.folds.some((fold) => fold.first === line)) continue
      // What the terminal measured a piece in, against what this view draws
      const scaleX = images.cellWidth > 0 ? cellWidth / images.cellWidth : 1
      const scaleY = images.cellHeight > 0 ? cellHeight / images.cellHeight : 1
      const top = this.rowsBetween(first, line) * cellHeight
      for (const piece of images.pieces) {
        const picture = this.pictures.get(piece.image)?.picture
        if (!picture) continue
        shown.push({
          picture,
          x: piece.column * cellWidth + piece.offsetX * scaleX,
          y: top + piece.offsetY * scaleY,
          width: piece.width * scaleX,
          height: piece.height * scaleY,
          source: { x: piece.sourceX, y: piece.sourceY, width: piece.sourceWidth, height: piece.sourceHeight },
          above: false,
        })
      }
    }
    return shown
  }

  /** A mark for each command whose prompt is in view, coloured by how the command ended */
  private marksIn(first: number, lines: number[]): NonNullable<Scene['marks']> {
    if (this.gutter === 0) return []
    const prompts = this.commandPrompts
    const last = lines[lines.length - 1]!
    const marks: Array<{ row: number; color: number }> = []
    for (let index = firstAfter(prompts, first - 1); index < prompts.length && prompts[index]!.line <= last; index += 1) {
      const { line, command } = prompts[index]!
      if (this.hiddenIn(line)) continue
      marks.push({ row: this.rowsBetween(first, line), color: this.theme.palette[markColor(command)] ?? this.theme.foreground })
    }
    return marks
  }

  /** Asks the program to clear its screen, the way the keyboard does: control and L */
  clear(): void {
    this.source.key({ action: 'press', code: 'KeyL', modifiers: Modifier.control, text: '', unshifted: 108, composed: false })
  }

  /** Gives the view the keyboard */
  focus(): void {
    this.input.field.focus({ preventScroll: true })
  }

  /** Copies what one command printed, answering whether there was any to copy: a command whose output has left the history has none */
  async copyCommand(command: Command): Promise<boolean> {
    return this.selectOutput(command) ? this.copy() : false
  }

  /**
   * Types `text` into the program as text a keyboard typed: what a program at a prompt would be sent by someone at the
   * keyboard, with no paste and so no confirmation. A view scrolled back follows the screen down to it, as a key does.
   */
  typeText(text: string): void {
    this.source.text(text)
    this.typed()
  }

  /**
   * Pastes text into the program. Text that could run a command — a line ending where the program takes
   * them as typed — goes only if the host's `confirmPaste` says the person agreed.
   *
   * Rejects with `TooLarge` for text the source cannot carry, of which none was sent. A paste into a
   * session that has ended goes nowhere, and says nothing.
   */
  async paste(text: string): Promise<void> {
    try {
      let outcome = await this.source.paste(text)
      if (outcome === 'unsafe' && (await this.options.confirmPaste?.(text))) outcome = await this.source.paste(text, { allowUnsafe: true })
      if (outcome === 'pasted') this.typed()
    } catch (error) {
      if (error instanceof TooLarge) throw error
      // The session ended while the paste was on its way: there is nothing to paste into
    }
  }

  /**
   * Copies what is selected, as a program wrote it. Answers whether it was copied.
   *
   * **The clipboard is written inside the gesture that asked.** The text is the source's to give, which
   * takes a round trip, and a browser lets a page write the clipboard only while it is handling what the
   * person did. So the clipboard is handed the text as a promise there and then, where the browser takes
   * one, rather than being written once the text has come.
   */
  async copy(): Promise<boolean> {
    if (!this.selection.span) return false
    const text = this.selection.text()
    if (this.options.onCopy) {
      const copied = await text.catch(() => '')
      if (copied !== '') this.options.onCopy(copied)
      return copied !== ''
    }
    // Looked at whoever else does or does not: a text nobody waits for must not fail unheard
    text.catch(() => {})
    const clipboard = navigator.clipboard
    if (!clipboard) return false
    try {
      if (typeof ClipboardItem === 'function' && typeof clipboard.write === 'function') {
        const plain = text.then((copied) => {
          if (copied === '') throw new Error('nothing is selected')
          return new Blob([copied], { type: 'text/plain' })
        })
        plain.catch(() => {})
        await clipboard.write([new ClipboardItem({ 'text/plain': plain })])
        return true
      }
      const copied = await text
      if (copied === '') return false
      await clipboard.writeText(copied)
      return true
    } catch {
      return false
    }
  }

  /** Puts the view in `host` and starts its frames */
  mount(host: HTMLElement): void {
    // Mounted where it already is, there is nothing to do; mounted somewhere else, it is moved
    if (this.observer) {
      if (this.element.parentElement === host) return
      this.unmount()
    }
    host.appendChild(this.element)
    this.observer = new ResizeObserver(() => {
      this.fit()
      this.schedule()
    })
    this.observer.observe(host)
    this.watch()
    // The display may not be the one the painter last measured a cell on, or any it has measured one on
    const sized = (window.devicePixelRatio || 1) !== this.pixelRatio
    if (sized) this.size()
    this.fit()
    this.resume()
    // Sizing a canvas clears it, so what the view holds is drawn again without waiting for a frame
    if (sized) this.schedule()
  }

  /**
   * Takes the view off the page and stops its frames. What it holds is kept, and what it was showing stays
   * on its canvas: mounted again, the first frames say what the screen is and which history may have
   * changed meanwhile, and the rest is as it was.
   */
  unmount(): void {
    this.observer?.disconnect()
    this.observer = null
    this.unwatch?.()
    this.unwatch = null
    // Said while there is still an attachment to say it on: a view off the page does not have the keyboard,
    // whether or not the page gets round to saying the field lost it
    this.setFocused(false)
    this.pause()
    this.element.remove()
  }

  /** Measures how many cells fit, and reports it */
  fit(): void {
    const host = this.element.parentElement
    if (!host) return
    this.height = this.element.clientHeight
    const { cellWidth, cellHeight } = this.painter.metrics
    const columns = Math.max(1, Math.floor((host.clientWidth - this.gutter) / cellWidth))
    const rows = Math.max(1, Math.floor(host.clientHeight / cellHeight))
    this.options.onFit?.({ columns, rows, cellWidth, cellHeight })
  }

  /**
   * Sizes the painter for the grid there is on the display there is, which is when a cell is measured.
   * Answers whether a cell is now another size, which is when what fits has changed.
   */
  private size(): boolean {
    const before = this.painter.metrics
    this.pixelRatio = window.devicePixelRatio || 1
    this.painter.resize(this.columns, this.rows, this.pixelRatio, this.gutter)
    const { cellWidth, cellHeight } = this.painter.metrics
    return cellWidth !== before.cellWidth || cellHeight !== before.cellHeight
  }

  /**
   * A cell may be another size: the window moved to a display with other pixels, the page was zoomed, or
   * a font the view draws in finished loading after it had been measured as its fallback.
   */
  private readonly remeasure = () => {
    if (this.size()) this.fit()
    this.schedule()
  }

  /**
   * Fonts finished loading on the page. A cell is measured again when one of them is a family the view
   * draws in; an event that does not say which faces loaded is taken to be about one that is.
   */
  private readonly fontsLoaded = (event: Event) => {
    const faces = (event as Event & { fontfaces?: readonly FontFace[] }).fontfaces
    const named = (family: string) => family.trim().replace(/^["']|["']$/g, '').toLowerCase()
    const own = new Set(this.font.family.split(',').map(named))
    if (!faces || faces.some((face) => own.has(named(face.family)))) this.remeasure()
  }

  /** Listens, while mounted, for what changes the size of a cell without the view being told */
  private watch(): void {
    this.unwatch?.()
    const stop: Array<() => void> = []
    // A resolution query matches one ratio, so it is asked again for each ratio the display comes to have
    const display = () => {
      if (typeof matchMedia !== 'function') return
      const query = matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`)
      const changed = () => {
        query.removeEventListener?.('change', changed)
        display()
        this.remeasure()
      }
      // A browser whose queries cannot be listened to this way is one whose display changing goes unheard
      query.addEventListener?.('change', changed)
      stopDisplay = () => query.removeEventListener?.('change', changed)
    }
    let stopDisplay = () => {}
    display()
    stop.push(() => stopDisplay())
    const fonts = document.fonts
    if (fonts && typeof fonts.addEventListener === 'function') {
      fonts.addEventListener('loadingdone', this.fontsLoaded)
      stop.push(() => fonts.removeEventListener('loadingdone', this.fontsLoaded))
    }
    this.unwatch = () => { for (const one of stop) one() }
  }

  /** The view has, or has lost, the keyboard: an unfocused terminal shows a hollow cursor that does not blink */
  setFocused(focused: boolean): void {
    if (focused === this.focused) return
    this.focused = focused
    this.blinkAgain()
    this.source.focus(focused)
    this.schedule()
  }

  scrollBy(lines: number): void {
    this.viewport.scrollBy(lines)
    this.scrolled()
  }

  scrollTo(line: number): void {
    this.viewport.scrollTo(line)
    this.scrolled()
  }

  /** Shows the screen again and follows it */
  follow(): void {
    this.viewport.follow()
    this.scrolled()
  }

  /** Something outside the rows changed what a row looks like — a selection moved */
  redraw(): void {
    this.schedule()
  }

  dispose(): void {
    if (this.refreshTimer !== undefined) clearTimeout(this.refreshTimer)
    if (this.disposed) return
    this.unmount()
    this.disposed = true
    this.element.removeEventListener('wheel', this.wheel)
    this.element.removeEventListener('pointerdown', this.pointerDown)
    this.element.removeEventListener('pointermove', this.pointerMove)
    this.element.removeEventListener('pointerup', this.pointerUp)
    this.element.removeEventListener('pointercancel', this.pointerLost)
    this.element.removeEventListener('lostpointercapture', this.pointerLost)
    this.element.removeEventListener('pointerleave', this.pointerLeave)
    this.element.removeEventListener('contextmenu', this.contextMenu)
    this.thumb.removeEventListener('pointerdown', this.thumbDown)
    this.thumbRelease?.()
    this.pressed = null
    this.stopDragScroll()
    for (const held of this.pictures.values()) release(held.picture)
    this.pictures.clear()
    this.input.dispose()
    this.painter.dispose()
  }

  private resume(): void {
    if (this.detach || this.disposed) return
    // An attachment that could not be made is over and its session is not: this one asks again. And what
    // was held is left as it is: the attachment's first frames say what of it is not to be trusted, by
    // naming history as stale or by being in another epoch
    this.ended = false
    this.listening = true
    this.attachment += 1
    this.detach = this.source.attach({
      frame: (frame) => this.take(frame),
      ended: (reason) => {
        this.ended = true
        this.options.onEnded?.(reason)
      },
    })
    // A program is told who has the keyboard by the attachment that has it
    if (this.focused) this.source.focus(true)
    // What an attachment before was asked for and could not answer is asked of this one
    this.wantPictures(...this.viewport.visible)
    this.blinkAgain()
  }

  /**
   * Stops everything that goes on by itself: the frames, the paint that was asked for, the blink, and the
   * scrolling of a drag that was past an edge.
   */
  private pause(): void {
    this.detach?.()
    this.detach = null
    this.listening = false
    this.unacknowledged = null
    if (this.scheduled !== null) cancelAnimationFrame(this.scheduled)
    this.scheduled = null
    this.stopDragScroll()
    this.blinkAgain()
    this.blinkText(false)
  }

  /**
   * Starts or stops the clock blinking text goes by. It runs only while text that blinks is in view and
   * the view is taking frames, and whatever it stopped at, text is shown once it has: a view that is
   * not blinking is one in which nothing is hidden.
   *
   * **A clock of its own, not the cursor's.** The cursor's starts over at every keystroke and every
   * frame, so that a cursor that just moved is seen; text that started over that often would never
   * reach the half it is hidden for under a program that keeps printing.
   */
  private blinkText(inView: boolean): void {
    if (inView && this.listening) {
      this.textBlink ??= setInterval(() => {
        this.textShown = !this.textShown
        this.schedule()
      }, BLINK_MS)
      return
    }
    if (this.textBlink) clearInterval(this.textBlink)
    this.textBlink = null
    if (!this.textShown) {
      this.textShown = true
      this.schedule()
    }
  }

  private take(frame: Frame): void {
    const moved = frame.epoch !== this.store.epoch
    const after = moved ? this.viewport.acrossReset() : undefined
    // What is kept longest is what the viewer is at: the screen, or the line a scrolled view holds
    const near = moved || this.viewport.following ? frame.firstActive : this.viewport.top
    const { reset, changed } = this.store.apply(frame, near)
    after?.()
    this.viewport.settle()
    if (reset) {
      this.drawings.clear()
      this.asking.clear()
      // Every line number changed, so what was selected and hovered names lines that are gone
      this.selection.reset()
      this.hovered = null
      this.hoverAt = null
      this.element.style.cursor = 'text'
      this.element.title = ''
      // And the lines its commands were on: asked for again, in this epoch
      this.setCommands([])
      this.commandsSeen = -1
      // And the lines its images were on. Their pixels are the same pictures wherever they are now
      this.placed = { ...this.placed, placements: [] }
      this.imagesSeen = -1
      this.needle = ''
      this.setMatches([])
      this.currentMatch = -1
      this.finding += 1
    } else {
      for (const line of changed) this.drawings.delete(line)
      if (changed.length > 0) this.refreshMatches()
    }
    if (frame.commands !== this.commandsSeen) {
      const seen = (this.commandsSeen = frame.commands)
      const epoch = frame.epoch
      void this.source.commands().then((commands) => {
        if (this.disposed || epoch !== this.store.epoch) return
        this.setCommands(commands)
        this.schedule()
      }, () => {
        // Not heard, so not seen: the next frame asks again
        if (epoch === this.store.epoch && this.commandsSeen === seen) this.commandsSeen = -1
      })
    }
    if (frame.images !== this.imagesSeen) {
      // History that shows part of an image is asked for again: the screen's rows came with the frame
      if (this.imagesSeen !== -1) this.store.doubtPictured()
      const seen = (this.imagesSeen = frame.images)
      const epoch = frame.epoch
      void this.source.images().then((placed) => {
        if (this.disposed || epoch !== this.store.epoch) return
        this.placed = placed
        this.dropPictures()
        // Asked for as soon as they are known to be in view, rather than by the paint that would show them
        this.wantPictures(...this.viewport.visible)
        this.schedule()
      }, () => {
        if (epoch === this.store.epoch && this.imagesSeen === seen) this.imagesSeen = -1
      })
    }
    // The program's colours as a frame tells them are all of them: what it leaves out is the theme's again.
    // A frame that does not tell them leaves them as they were — across a new numbering too, since an
    // attachment's first frame always tells them
    if (frame.colors !== undefined || frame.reverseVideo !== this.reverseVideo) {
      if (frame.colors !== undefined) this.colors = frame.colors
      this.reverseVideo = frame.reverseVideo
      this.recolour()
    }
    this.frame = frame
    this.unacknowledged = frame.sequence
    if (frame.columns !== this.columns || frame.screenRows !== this.rows) {
      this.columns = frame.columns
      this.rows = frame.screenRows
      if (this.size()) this.fit()
    }
    // Output shows the cursor where it now is, at the start of a blink
    this.blinkAgain()
    this.followingMayHaveChanged()
    this.schedule()
  }

  /** Whether the cursor blinks now: the view's setting where it has one, and the program's where it does not */
  private cursorBlinks(): boolean {
    const cursor = this.frame?.cursor
    if (!cursor || cursor.hidden) return false
    switch (this.cursorBlink) {
      case 'on': return true
      case 'off': return false
      case 'program': return cursor.blinking
    }
  }

  /**
   * Shows the cursor and starts its blink over, which is what anything typed and anything printed does:
   * a cursor that has just moved is one a person is looking for. It blinks only while the view has the
   * keyboard and is taking frames.
   */
  private blinkAgain(): void {
    if (this.blink) clearInterval(this.blink)
    this.blink = null
    if (!this.cursorShown) {
      this.cursorShown = true
      this.schedule()
    }
    if (!this.focused || !this.listening || !this.cursorBlinks()) return
    this.blink = setInterval(() => {
      this.cursorShown = !this.cursorShown
      this.schedule()
    }, BLINK_MS)
  }

  /** The person typed, composed or pasted something the program was sent */
  private typed(): void {
    if ((this.options.followOnInput ?? true) && !this.viewport.following) this.follow()
    this.blinkAgain()
  }

  private scrolled(): void {
    this.followingMayHaveChanged()
    this.schedule()
  }

  private followingMayHaveChanged(): void {
    const following = this.viewport.following
    if (following === this.wasFollowing) return
    this.wasFollowing = following
    this.options.onFollowingChange?.(following)
  }

  /**
   * The wheel scrolls history where there is some. A program that asked for the mouse is sent the wheel
   * instead, while the view is showing the screen it draws on, and a full-screen program that did not is
   * sent arrow keys, which is what makes `less` and `man` scroll under a wheel in every terminal.
   */
  private readonly wheel = (event: WheelEvent) => {
    const frame = this.frame
    // With control held the wheel is the browser's: a pinch arrives as one, and zooms the page
    if (!frame || event.ctrlKey) return
    const { cellHeight } = this.painter.metrics
    // A wheel reports pixels, lines or pages; a trackpad reports pixels, which is what makes the view move
    // by fractions of a row
    const lines = event.deltaMode === 1 ? event.deltaY : event.deltaMode === 2 ? event.deltaY * this.rows : event.deltaY / cellHeight
    const toProgram = frame.modes.mouse && !event.shiftKey && this.viewport.following
    if (!toProgram && !frame.modes.alternateScreen) {
      if (this.store.scrollback === 0) return
      event.preventDefault()
      this.scrollBy(lines)
      return
    }
    event.preventDefault()
    // A program is told about whole notches, so fractions are kept until they add up to one — of one
    // direction: what was owed the other way is not paid back out of this one
    if (this.wheelOwed !== 0 && Math.sign(lines) !== Math.sign(this.wheelOwed)) this.wheelOwed = 0
    this.wheelOwed += lines
    const notches = Math.trunc(this.wheelOwed)
    this.wheelOwed -= notches
    for (let sent = 0; sent < Math.min(Math.abs(notches), 20); sent += 1) {
      if (toProgram) this.source.pointer(this.pointerInput(event, notches < 0 ? 'wheelUp' : 'wheelDown', -1))
      else this.source.key({ action: 'press', code: notches < 0 ? 'ArrowUp' : 'ArrowDown', modifiers: 0, text: '', unshifted: 0, composed: false })
    }
  }

  /**
   * Where a point on the page is, in the canvas's own pixels from the grid's corner. A transform on the
   * page — a scaled panel, a zoomed canvas — makes the box the canvas occupies another size than the
   * canvas, and a pointer is reported in the box's.
   */
  private pointIn(point: Point): { x: number; y: number } {
    const canvas = this.painter.canvas
    const box = canvas.getBoundingClientRect()
    const scaleX = canvas.offsetWidth > 0 && box.width > 0 ? box.width / canvas.offsetWidth : 1
    const scaleY = canvas.offsetHeight > 0 && box.height > 0 ? box.height / canvas.offsetHeight : 1
    return { x: (point.clientX - box.left) / scaleX - this.gutter, y: (point.clientY - box.top) / scaleY }
  }

  /** The cell a point is over, as a line and a column: the nearest there is, for a point outside them all */
  private positionOf(point: Point): Position {
    const { cellWidth, cellHeight } = this.painter.metrics
    const { x, y } = this.pointIn(point)
    const { lines } = this.layout()
    const row = Math.floor(y / cellHeight + this.viewport.offset)
    return {
      line: lines[Math.max(0, Math.min(row, lines.length - 1))]!,
      column: Math.max(0, Math.min(Math.floor(x / cellWidth), Math.max(0, this.columns - 1))),
    }
  }

  /** How far a point is past the top edge of the rows, below zero, or past the bottom, above it: in rows */
  private beyond(point: Point): number {
    const { cellHeight } = this.painter.metrics
    const { y } = this.pointIn(point)
    const bottom = this.rows * cellHeight
    return y < 0 ? y / cellHeight : y > bottom ? (y - bottom) / cellHeight : 0
  }

  private pointerInput(event: MouseEvent, action: PointerInput['action'], button: number): PointerInput {
    const { cellWidth, cellHeight } = this.painter.metrics
    const { x, y } = this.pointIn(event)
    return { action, button, modifiers: modifiersOf(event), x: Math.max(0, x), y: Math.max(0, y), cellWidth, cellHeight }
  }

  /**
   * Tells a program where the pointer is, once for each cell it comes to: a program draws by cells, and a
   * pointer crosses one in many more events than that.
   */
  private reportMove(event: MouseEvent, button: number): void {
    const input = this.pointerInput(event, 'move', button)
    const column = Math.floor(input.x / input.cellWidth)
    const row = Math.floor(input.y / input.cellHeight)
    const last = this.reported
    if (last && last.column === column && last.row === row && last.button === button) return
    this.reported = { column, row, button }
    this.source.pointer(input)
  }

  /**
   * How many presses this one is the last of: one, two or three, and then one again. Another press is the
   * same click again when it is the same button, soon after the last and where the last was — by the
   * event's own clock, which is the page's and not the wall's.
   */
  private clicks(event: PointerEvent): number {
    const last = this.lastPress
    const again = last !== null && last.button === event.button && event.timeStamp - last.time <= CLICK_AGAIN_MS
      && Math.abs(event.clientX - last.clientX) <= CLICK_AGAIN_PIXELS && Math.abs(event.clientY - last.clientY) <= CLICK_AGAIN_PIXELS
    const count = again && last.count < 3 ? last.count + 1 : 1
    this.lastPress = { time: event.timeStamp, clientX: event.clientX, clientY: event.clientY, button: event.button, count }
    return count
  }

  /** `0` left, `1` middle, `2` right, as a source numbers them — which is how the page does too */
  private readonly pointerDown = (event: PointerEvent) => {
    if (event.target === this.thumb) return
    this.focus()
    // Keeps the focus this just gave from going to whatever the page would otherwise focus on a click
    event.preventDefault()
    const at = this.positionOf(event)
    // A program that asked for the mouse owns it, where the screen is showing; shift takes it back
    const reporting = this.frame?.modes.mouse === true && !event.shiftKey && this.viewport.following
    this.pressed = { button: event.button, reporting, at, moved: false, block: event.altKey }
    this.element.setPointerCapture?.(event.pointerId)
    if (reporting) {
      const input = this.pointerInput(event, 'press', event.button)
      this.reported = { column: Math.floor(input.x / input.cellWidth), row: Math.floor(input.y / input.cellHeight), button: event.button }
      this.source.pointer(input)
      return
    }
    if (event.button !== 0) return
    if (event.shiftKey && this.selection.span) {
      this.selection.extendTo(at)
      return
    }
    const clicks = this.clicks(event)
    const granularity: Granularity = clicks === 3 ? 'line' : clicks === 2 ? 'word' : 'cell'
    if (granularity === 'cell') this.selection.clear()
    else this.selection.begin(at, granularity)
  }

  private readonly pointerMove = (event: PointerEvent) => {
    const pressed = this.pressed
    if (pressed?.reporting) {
      this.reportMove(event, pressed.button)
      return
    }
    if (!pressed && this.frame?.modes.mouse && !event.shiftKey && this.viewport.following) this.reportMove(event, -1)
    const at = this.positionOf(event)
    if (pressed?.button === 0) {
      // The first move off the cell that was pressed is where a selection starts: a click selects nothing
      if (!pressed.moved && (at.line !== pressed.at.line || at.column !== pressed.at.column)) {
        pressed.moved = true
        if (!this.selection.span) this.selection.begin(pressed.at, 'cell', pressed.block ? 'block' : 'linear')
      }
      if (pressed.moved) {
        this.selection.extendTo(at)
        this.dragScrollTo(event)
      }
      return
    }
    this.hover(at)
  }

  private readonly pointerUp = (event: PointerEvent) => {
    const pressed = this.released(event)
    if (!pressed || pressed.reporting) return
    // A selection the person made — dragged out, or a word or line on a double or triple click — is copied where asked
    if (pressed.button === 0 && this.copyOnSelect && this.selection.span) void this.copy()
    // A click that did not drag, with the platform's modifier held, opens what it is on
    const opening = this.mac ? event.metaKey : event.ctrlKey
    if (pressed.button === 0 && !pressed.moved && opening) {
      const link = this.linkOn(this.positionOf(event))
      if (link) this.options.onLink?.(link, event)
    }
  }

  /**
   * The pointer was taken away with a button down — the system cancelled it for a gesture of its own, or
   * something else took its capture — and no `pointerup` will say so.
   */
  private readonly pointerLost = (event: PointerEvent) => {
    this.released(event)
  }

  /**
   * The button that was down no longer is, however that came about. A program that was sent its press is
   * sent its release — it would otherwise go on dragging for ever — and nothing goes on scrolling.
   */
  private released(event: PointerEvent): Pressed | null {
    const pressed = this.pressed
    this.pressed = null
    this.stopDragScroll()
    if (!pressed) return null
    if (this.element.hasPointerCapture?.(event.pointerId)) this.element.releasePointerCapture(event.pointerId)
    if (pressed.reporting) {
      this.reported = null
      this.source.pointer(this.pointerInput(event, 'release', pressed.button))
    }
    return pressed
  }

  /**
   * A selection being dragged has its pointer at `point`. Past the top or bottom edge, the view scrolls
   * that way for as long as the pointer is held there, faster the further past it is.
   */
  private dragScrollTo(point: Point): void {
    if (this.beyond(point) === 0) return this.stopDragScroll()
    const kept = { clientX: point.clientX, clientY: point.clientY }
    if (this.dragScroll) this.dragScroll.point = kept
    else this.dragScroll = { point: kept, at: null, frame: requestAnimationFrame(this.dragScrollStep) }
  }

  private readonly dragScrollStep = (time: number) => {
    const drag = this.dragScroll
    if (!drag) return
    // The first step has no step before it to measure from, and is taken to be one frame long; a step
    // after a long gap — a tab that was hidden — is not a jump of everything that gap would have scrolled
    const elapsed = drag.at === null ? 16 : Math.min(Math.max(time - drag.at, 0), 100)
    drag.at = time
    const top = this.viewport.top
    this.scrollBy((this.beyond(drag.point) * elapsed * DRAG_LINES_PER_SECOND) / 1000)
    this.selection.extendTo(this.positionOf(drag.point))
    // With nowhere further to scroll there is nothing to go on doing until the pointer moves again
    if (elapsed > 0 && this.viewport.top === top) {
      this.dragScroll = null
      return
    }
    drag.frame = requestAnimationFrame(this.dragScrollStep)
  }

  private stopDragScroll(): void {
    if (this.dragScroll) cancelAnimationFrame(this.dragScroll.frame)
    this.dragScroll = null
  }

  private readonly pointerLeave = () => this.hover(null)

  /**
   * A program that owns the mouse is sent the right button, where the screen it draws on is showing;
   * otherwise the page's menu is the host's to show.
   */
  private readonly contextMenu = (event: MouseEvent) => {
    if (this.frame?.modes.mouse && !event.shiftKey && this.viewport.following) event.preventDefault()
  }

  private linkOn(at: Position): Link | null {
    const row = this.store.row(at.line)
    return row ? linkAt(row, at.column, this.detectors, (line) => this.store.row(line)) : null
  }

  /** Marks the link under the pointer, which is how a person finds out a piece of text opens something */
  private hover(at: Position | null): void {
    // Over the same cell of the same row as when it was last worked out, the answer is the same
    const over = at ? { line: at.line, column: at.column, row: this.store.row(at.line) } : null
    const last = this.hoverAt
    this.hoverAt = over
    if (over && last && over.line === last.line && over.column === last.column && over.row === last.row) return
    const link = at ? this.linkOn(at) : null
    const before = this.hovered
    const same = before !== null && link !== null && at !== null && before.line === at.line && before.link.from === link.from && before.link.to === link.to
    if (same || (!before && !link)) return
    const row = at ? this.store.row(at.line) : undefined
    this.hovered = link && at && row ? { line: at.line, link, runs: linkRuns(link, row, at.column, (line) => this.store.row(line)) } : null
    this.element.style.cursor = link ? 'pointer' : 'text'
    this.element.title = link && this.previews(link) ? link.target : ''
    this.schedule()
  }

  /** Whether hovering a link shows where it opens, by the `linkPreview` the host chose */
  private previews(link: Link): boolean {
    return this.linkPreview === 'all' || (this.linkPreview === 'osc8' && link.kind === 'declared')
  }

  /**
   * The scrollbar's thumb for where the view is: how tall, and how far down. It stands for the screenful in
   * view within everything there is, never shorter than can be grabbed and never past the end of its
   * track — so the top of its travel is the oldest line and the bottom is the screen.
   */
  private thumbAt(): { height: number; top: number; travel: number } | null {
    const { total, position, shown } = this.viewport.extent
    if (total <= shown || this.height <= 0) return null
    const height = Math.min(this.height, Math.max(THUMB_MIN, (shown / total) * this.height))
    const travel = this.height - height
    return { height, top: (position / (total - shown)) * travel, travel }
  }

  /** Dragging the scrollbar's thumb moves through history in proportion */
  private readonly thumbDown = (event: PointerEvent) => {
    event.preventDefault()
    event.stopPropagation()
    this.thumbRelease?.()
    const start = { y: event.clientY, top: this.viewport.top }
    const move = (moved: PointerEvent) => {
      const { total, shown } = this.viewport.extent
      const travel = this.thumbAt()?.travel ?? 0
      if (travel > 0) this.scrollTo(start.top + ((moved.clientY - start.y) / travel) * (total - shown))
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      window.removeEventListener('pointercancel', up)
      this.thumbRelease = null
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
    window.addEventListener('pointercancel', up)
    this.thumbRelease = up
  }

  /** Asks for one paint on the next animation frame. A view that is taking no frames is not on the page, and draws nothing */
  private schedule(): void {
    if (this.scheduled !== null || this.disposed || !this.frame || !this.listening) return
    this.scheduled = requestAnimationFrame(() => {
      this.scheduled = null
      this.paint()
    })
  }

  private paint(): void {
    const frame = this.frame
    if (!frame) return
    try {
      const { first, lines } = this.layout()
      // The absolute lines the window covers, folded ones included: what is asked of the source and what is held
      const span = lines[lines.length - 1]! - first + 1
      // Decided before anything is drawn, so a blink that has just stopped draws its text shown, once
      let blinking = false
      for (const line of lines) {
        const row = this.store.row(line)
        if (row !== undefined && blinksIn(row)) {
          blinking = true
          break
        }
      }
      this.blinkText(blinking)
      const rows: Array<RowDrawing | null> = lines.map((line) => {
        const fold = this.folds.find((known) => known.first === line)
        return fold ? drawRow(this.foldRow(fold), this.theme) : this.drawingOf(line)
      })
      // What a scroll of a screenful either way would show stays drawn; the rest is not held on to
      for (const line of this.drawings.keys()) {
        if (line < first - span || line >= first + span * 2) this.drawings.delete(line)
      }
      this.ask(first, span)
      this.wantPictures(first, span)

      const cursor = this.cursorIn(frame, lines)
      const scene: Scene = {
        theme: this.theme, rows, offset: this.viewport.offset, cursor,
        marks: this.marksIn(first, lines), images: this.imagesIn(first, span, lines),
      }
      this.painter.paint(scene)
      this.afterPaint(frame, first, lines)
    } finally {
      // Whatever became of the paint, the frame has been taken: one that is never acknowledged is the
      // last the source sends
      if (this.unacknowledged !== null && !this.ended) this.source.acknowledge(this.unacknowledged)
      this.unacknowledged = null
    }
  }

  /**
   * What follows a paint and is not paint: the text a page reader reads, the scrollbar, where an input
   * method's window goes, and the mark that says how far has been drawn.
   */
  private afterPaint(frame: Frame, first: number, lines: number[]): void {
    const count = lines.length
    // One element a row, touched only when its row is a different one from what it last showed
    while (this.mirror.childElementCount < count) this.mirror.appendChild(document.createElement('div'))
    while (this.mirror.childElementCount > count) this.mirror.lastElementChild!.remove()
    this.mirrored.length = count
    for (let index = 0; index < count; index += 1) {
      const line = lines[index]!
      const fold = this.folds.find((known) => known.first === line)
      const row = fold ? this.foldRow(fold) : (this.store.row(line) ?? null)
      if (this.mirrored[index] === row) continue
      this.mirrored[index] = row
      this.mirror.children[index]!.textContent = row ? textOf(row) : ''
    }

    const thumb = this.thumbAt()
    const next = { height: thumb?.height ?? 0, top: thumb?.top ?? 0, shown: thumb !== null && !this.viewport.following }
    const last = this.thumbDrawn
    if (next.height !== last.height) this.thumb.style.height = `${next.height}px`
    if (next.top !== last.top) this.thumb.style.top = `${next.top}px`
    if (next.shown !== last.shown) {
      this.thumb.style.opacity = next.shown ? '0.35' : '0'
      this.thumb.style.pointerEvents = next.shown ? 'auto' : 'none'
    }
    this.thumbDrawn = next

    const { cellWidth, cellHeight } = this.painter.metrics
    // Where the cursor is, drawn or not: a program that hides it still puts it where typing goes
    if (frame.cursor) {
      const row = this.rowsBetween(first, frame.firstActive + frame.cursor.row)
      this.input.moveTo(this.gutter + frame.cursor.column * cellWidth, Math.max(0, row - this.viewport.offset) * cellHeight, cellHeight)
    }
    // How far a page looking for output to be on screen can count on: this frame, and these lines
    if (frame.sequence !== this.drawn) {
      this.drawn = frame.sequence
      this.element.dataset.terminalDrawn = String(frame.sequence)
    }
  }

  private drawingOf(line: number): RowDrawing | null {
    const row = this.store.row(line)
    if (!row) return null
    const selected: SelectedCells | undefined = this.selection.on(line)
    const hovered = this.hovered?.runs.filter((run) => run.line === line) ?? []
    const matched = this.matches.length > 0 ? this.matchesOn(line) : []
    // Which half of a blink it is, for a row that has text to hide in one of them
    const blinkedOff = !this.textShown && blinksIn(row)
    const key = `${selected ? `${selected.from}-${selected.to}` : ''}|${hovered.map((run) => `${run.from}-${run.to}`).join(',')}`
      + `|${matched.map((match) => `${match.from}-${match.to}${match.current ? '!' : ''}`).join(',')}|${blinkedOff ? 'off' : ''}`
    const kept = this.drawings.get(line)
    if (kept && kept.row === row && kept.selected === key) return kept.drawing
    let drawing = drawRow(row, this.theme, selected, blinkedOff)
    // Behind what a search found, over whatever background the cells had: later rectangles cover earlier
    if (matched.length > 0) {
      const { matchBackground = 0x623315, currentMatchBackground = 0x9e6a03 } = this.theme
      drawing = {
        ...drawing,
        backgrounds: [
          ...drawing.backgrounds,
          ...matched.map((match) => ({ column: match.from, cells: match.to - match.from, color: match.current ? currentMatchBackground : matchBackground })),
        ],
      }
    }
    // A link under the pointer is underlined, whatever it looked like before
    if (hovered.length > 0) {
      drawing = {
        ...drawing,
        lines: [...drawing.lines, ...hovered.map((run) => ({ kind: 'single' as const, column: run.from, cells: run.to - run.from, color: this.theme.foreground }))],
      }
    }
    this.drawings.set(line, { row, selected: key, drawing })
    return drawing
  }

  /**
   * Asks for the history in view, and within a screenful of it, that is not held.
   *
   * **By the page, once a page.** What is asked for is the part of a whole page that is missing, not the
   * few lines that just came into view: scrolling a line at a time then asks once in `PAGE_LINES` lines
   * rather than once a line, and a page that is on its way is not asked for again however the view moves
   * meanwhile.
   */
  private ask(first: number, count: number): void {
    if (this.ended) return
    for (const [from, length] of this.store.missing(first - count, count * 3)) {
      for (let page = Math.floor(from / PAGE_LINES); page * PAGE_LINES < from + length; page += 1) this.askPage(page)
    }
  }

  /**
   * Asks for what a page lacks, from its first missing line to its last.
   *
   * **The answer says how far it got.** A line it covers and has no row for is one the owner does not
   * hold, which the store remembers, so it is not asked for again. A line past what it covers was not
   * answered — an answer holds what one message can — and is still missing, so the paint the answer
   * prompts asks for the rest. An answer that told nothing prompts no paint, and so no asking in a circle.
   */
  private askPage(page: number): void {
    if (this.asking.has(page)) return
    const runs = this.store.missing(page * PAGE_LINES, PAGE_LINES)
    const last = runs.at(-1)
    if (!last) return
    const from = runs[0]![0]
    const to = last[0] + last[1]
    this.asking.add(page)
    const epoch = this.store.epoch
    void this.source.fetch(from, to - from).then((fetched) => {
      // After a reflow the page is one of other lines, which this answer says nothing about — and the
      // page of that number being asked for may be the new one
      if (this.disposed || epoch !== this.store.epoch) return
      this.asking.delete(page)
      const told = this.store.absorb(fetched, this.viewport.top)
      for (const line of told) this.drawings.delete(line)
      if (told.length > 0) this.schedule()
    }, () => {
      // No answer: the lines were numbered again while it was asked, or the session is over. Whatever
      // paints next asks in the numbering there is then
      if (epoch === this.store.epoch) this.asking.delete(page)
    })
  }

  private cursorIn(frame: Frame, lines: number[]): Scene['cursor'] {
    const cursor = frame.cursor
    if (!cursor || cursor.hidden || !this.cursorShown) return undefined
    const line = frame.firstActive + cursor.row
    const index = lines.indexOf(line)
    if (index === -1) return undefined
    const row = this.store.row(line)
    const at = row ? row.column.indexOf(cursor.column) : -1
    const style = row && at !== -1 ? row.styles[row.style[at]!] : undefined
    return {
      column: cursor.column,
      row: index,
      cells: row && at !== -1 ? row.width[at]! : 1,
      shape: this.focused ? cursor.shape : this.cursorInactive,
      text: row && at !== -1 ? { text: row.text[at]!, bold: ((style?.flags ?? 0) & 1) !== 0, italic: ((style?.flags ?? 0) & 2) !== 0 } : undefined,
    }
  }
}

const sameSpan = (one: Span, other: Span): boolean =>
  one.start.line === other.start.line && one.start.column === other.start.column && one.end.line === other.end.line && one.end.column === other.end.column

/** `0xRRGGBB` as the page writes a colour */
function hex(color: number): string {
  return `#${color.toString(16).padStart(6, '0')}`
}

/**
 * Which of the theme's sixteen colours marks a command: grey (8) for one still to run, blue (4) for one
 * running, green (2) for one that ended with zero and red (1) for one that ended with anything else.
 *
 * **A status is drawn as the status it is, whoever reported it.** The mark is for a person, who is
 * looking at the output it stands beside and to whom everything on a screen is what a program printed:
 * a session with no key — a shell on a target, `cmd` — would otherwise draw every command that
 * succeeded as a warning. What tells a status that was vouched for from one that was only claimed is
 * the command's `outcome`, which is what a program reads. A command whose end nothing reported, or
 * that ended with no status, is grey.
 */
function markColor(command: Command): number {
  if (command.startedMs === null) return 8
  const { outcome } = command
  switch (outcome.kind) {
    case 'running': return 4
    case 'exited': return outcome.code === 0 ? 2 : 1
    case 'unknown': return 8
    case 'claimed': return outcome.code === null ? 8 : outcome.code === 0 ? 2 : 1
  }
}

/** The index of the first of `items`, which are in the order of their lines, that is on a line after `line` */
function firstAfter(items: ReadonlyArray<{ readonly line: number }>, line: number): number {
  let low = 0
  let high = items.length
  while (low < high) {
    const middle = (low + high) >> 1
    if (items[middle]!.line <= line) low = middle + 1
    else high = middle
  }
  return low
}

/** Gives back what a picture holds, where it holds anything: a bitmap keeps its pixels until it is closed */
function release(picture: CanvasImageSource | null): void {
  if (picture && 'close' in picture && typeof picture.close === 'function') picture.close()
}

/** A row as the text on it, with the blank cells between its characters as spaces */
function textOf(row: Row): string {
  let text = ''
  let column = 0
  for (let index = 0; index < row.text.length; index += 1) {
    const start = row.column[index]!
    if (start > column) text += ' '.repeat(start - column)
    text += row.text[index]!
    column = start + row.width[index]!
  }
  return text
}

/**
 * Pixels as something a canvas draws. A bitmap where the browser makes them — decoded off the thread that
 * paints, and drawn from the GPU — and a canvas holding the pixels where it does not.
 */
async function pictureOf(read: Picture): Promise<CanvasImageSource | null> {
  if (read.width <= 0 || read.height <= 0 || read.pixels.length < read.width * read.height * 4) return null
  const data = new ImageData(read.pixels as Uint8ClampedArray<ArrayBuffer>, read.width, read.height)
  if (typeof createImageBitmap === 'function') return createImageBitmap(data)
  const canvas = document.createElement('canvas')
  canvas.width = read.width
  canvas.height = read.height
  canvas.getContext('2d')?.putImageData(data, 0, 0)
  return canvas
}
