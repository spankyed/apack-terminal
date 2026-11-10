import { CanvasPainter } from './canvas-painter.ts'
import { drawRow, type RowDrawing, type SelectedCells, type Theme } from './draw.ts'
import { Input, modifiersOf } from './input.ts'
import { linkAt, urls, type Detector, type Link } from './links.ts'
import type { Font, Painter, Scene } from './painter.ts'
import { RowStore } from './row-store.ts'
import { Selection, type Granularity } from './selection.ts'
import { Modifier, type Command, type Frame, type PointerInput, type Position, type Row, type Span, type TerminalSource } from './source.ts'
import { Viewport } from './viewport.ts'

/** How long the cursor is shown, and then hidden, while it blinks */
export const BLINK_MS = 530

export interface ViewOptions {
  readonly font: Font
  readonly theme: Theme
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
  /** The session ended */
  readonly onEnded?: () => void
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
  /** The person opened a link: clicked it with the platform's modifier held */
  readonly onLink?: (link: Link, event: MouseEvent) => void
  /** What else on a row can be opened, beyond URLs and what a program declared */
  readonly detectors?: readonly Detector[]
  /** Whether anything is selected changed, which a host draws its copy control from */
  readonly onSelectionChange?: (selected: boolean) => void
  /** Text was copied. Left out, it is written to the clipboard the page has */
  readonly onCopy?: (text: string) => void
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

/**
 * A terminal, drawn.
 *
 * **It draws when something changed and at no other time.** A frame arrives, the user scrolls, a
 * selection moves, the cursor blinks: each asks for one paint on the next animation frame, and with
 * nothing asking there is no loop running. A frame is acknowledged once it has been painted, which is what
 * lets the source send the next — so a view that is slow to draw is sent fewer frames rather than more
 * than it can use, and a view that is not mounted is sent none.
 */
export class TerminalView {
  readonly element: HTMLDivElement
  readonly store = new RowStore()
  readonly viewport = new Viewport(this.store)

  private readonly painter: Painter
  private readonly drawings = new Map<number, Kept>()
  private readonly asked = new Set<number>()
  private frame: Frame | null = null
  /** The frame to acknowledge once it is on the screen */
  private unacknowledged: number | null = null
  private detach: (() => void) | null = null
  private scheduled: number | null = null
  private observer: ResizeObserver | null = null
  private blink: ReturnType<typeof setInterval> | null = null
  private cursorShown = true
  private focused = false
  private wasFollowing = true
  private disposed = false
  private wasSelected = false
  /** Where a search found its text, newest first, and which of them the person is on */
  private matches: readonly Span[] = []
  private currentMatch = -1
  private finding = 0
  private readonly gutter: number
  /** The commands the shell has reported, and the frame's count that said to ask for them */
  private commandList: readonly Command[] = []
  private commandsSeen = -1
  private readonly mac: boolean
  private readonly detectors: readonly Detector[]
  private readonly input: Input
  /** The rows on screen as text, for whatever reads a page rather than looks at it */
  private readonly mirror: HTMLDivElement
  private readonly mirrored: Array<Row | null | undefined> = []
  private readonly thumb: HTMLDivElement
  /** The link the pointer is over, and the line it is on */
  private hovered: { line: number; link: Link } | null = null
  /** A button is down: what it is doing, and where it went down */
  private pressed: { button: number; reporting: boolean; at: Position; moved: boolean } | null = null
  /** Fractions of a wheel notch not yet sent to a program that takes whole ones */
  private wheelOwed = 0
  /** What is selected. It asks the source what a word or a line is and draws nothing itself */
  readonly selection: Selection

  constructor(private readonly source: TerminalSource, private readonly options: ViewOptions) {
    this.mac = options.mac ?? /Mac|iPhone|iPad/.test(navigator.platform)
    this.gutter = options.gutter ?? 0
    this.detectors = [urls, ...(options.detectors ?? [])]
    this.element = document.createElement('div')
    this.element.style.cssText = 'position:relative;width:100%;height:100%;overflow:hidden;cursor:text'
    this.element.style.background = `#${options.theme.background.toString(16).padStart(6, '0')}`
    this.painter = (options.painter ?? ((font) => new CanvasPainter(font)))(options.font)
    this.painter.canvas.style.display = 'block'
    this.element.appendChild(this.painter.canvas)

    this.selection = new Selection(source, () => this.columns, () => {
      const selected = this.selection.span !== null
      if (selected !== this.wasSelected) {
        this.wasSelected = selected
        this.options.onSelectionChange?.(selected)
      }
      this.schedule()
    })
    this.input = new Input(source, {
      mac: this.mac,
      intercept: (event) => this.shortcut(event) || (options.onKey?.(event) ?? false),
      paste: (text) => { void this.paste(text) },
      copy: () => { void this.copy() },
      focused: (focused) => this.setFocused(focused),
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
    this.thumb.style.cssText = 'position:absolute;right:2px;width:6px;border-radius:3px;top:0;height:0;'
      + 'background:currentColor;opacity:0;transition:opacity 120ms;cursor:default;z-index:2'
    this.thumb.style.color = `#${options.theme.foreground.toString(16).padStart(6, '0')}`
    this.element.appendChild(this.thumb)

    this.element.addEventListener('wheel', this.wheel, { passive: false })
    this.element.addEventListener('pointerdown', this.pointerDown)
    this.element.addEventListener('pointermove', this.pointerMove)
    this.element.addEventListener('pointerup', this.pointerUp)
    this.element.addEventListener('pointerleave', this.pointerLeave)
    this.element.addEventListener('contextmenu', this.contextMenu)
    this.thumb.addEventListener('pointerdown', this.thumbDown)
  }

  /** Selects everything the terminal holds, history included */
  selectAll(): void {
    if (!this.frame) return
    this.selection.select({
      start: { line: this.store.oldest, column: 0 },
      end: { line: this.store.firstActive + this.rows - 1, column: Math.max(0, this.columns - 1) },
    })
  }

  /**
   * Finds every place `needle` appears, in the screen and all of its history, and goes to the newest.
   * Answers how many there are. An empty needle clears the search.
   */
  async find(needle: string): Promise<number> {
    const asked = (this.finding += 1)
    let found: readonly Span[] = []
    if (needle !== '') {
      try {
        found = await this.source.search(needle)
      } catch {
        found = []
      }
    }
    // A search that was overtaken by a later one has nothing to say
    if (asked !== this.finding) return this.matches.length
    this.matches = found
    this.currentMatch = found.length > 0 ? 0 : -1
    this.drawings.clear()
    this.showMatch()
    return found.length
  }

  /** Which match the person is on, counting from one at the newest, and how many there are */
  get found(): { current: number; total: number } {
    return { current: this.currentMatch + 1, total: this.matches.length }
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

  /** The cells of `line` that matches cover, each with whether it is the match the person is on */
  private matchesOn(line: number): Array<{ from: number; to: number; current: boolean }> {
    const on: Array<{ from: number; to: number; current: boolean }> = []
    this.matches.forEach((match, index) => {
      if (line < match.start.line || line > match.end.line) return
      on.push({
        from: line === match.start.line ? match.start.column : 0,
        to: line === match.end.line ? match.end.column + 1 : this.columns,
        current: index === this.currentMatch,
      })
    })
    return on
  }

  /** The commands the shell has reported, as the view last heard them */
  get commands(): readonly Command[] {
    return this.commandList
  }

  /**
   * The command whose prompt, line or output is on `line`: the last one that began at or before it.
   * `null` above the first command, and for a shell that marks none.
   */
  commandAt(line: number): Command | null {
    let found: Command | null = null
    for (const command of this.commandList) {
      const begins = command.prompt ?? command.output
      if (begins !== null && begins <= line) found = command
    }
    return found
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
    const prompts = this.commandList.map((command) => command.prompt).filter((line): line is number => line !== null)
    const target = direction === -1 ? prompts.filter((line) => line < top).at(-1) : prompts.find((line) => line > top)
    if (target !== undefined) this.scrollTo(target)
    else if (direction === 1) this.follow()
  }

  /** The keys that are the view's own: moving between commands */
  private shortcut(event: KeyboardEvent): boolean {
    const held = this.mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && event.shiftKey
    if (!held || event.altKey || (event.code !== 'ArrowUp' && event.code !== 'ArrowDown')) return false
    this.scrollToCommand(event.code === 'ArrowUp' ? -1 : 1)
    return true
  }

  /** A mark for each command whose prompt is in view, coloured by how the command ended */
  private marksIn(first: number, count: number): NonNullable<Scene['marks']> {
    if (this.gutter === 0) return []
    const { palette } = this.options.theme
    const marks: Array<{ row: number; color: number }> = []
    for (const command of this.commandList) {
      const line = command.prompt
      if (line === null || line < first || line >= first + count) continue
      // Still to run, running, succeeded, failed — and ended with the shell not saying how
      const color = command.startedMs === null ? palette[8]
        : command.finishedMs === null ? palette[4]
          : command.exitStatus === null ? palette[8] : command.exitStatus === 0 ? palette[2] : palette[1]
      marks.push({ row: line - first, color: color ?? this.options.theme.foreground })
    }
    return marks
  }

  /** Asks the program to clear its screen, the way the keyboard does: control and L */
  clear(): void {
    this.source.key({ action: 'press', code: 'KeyL', modifiers: Modifier.control, text: '', unshifted: 108 })
  }

  /** Gives the view the keyboard */
  focus(): void {
    this.input.field.focus({ preventScroll: true })
  }

  /**
   * Pastes text into the program. Text that could run a command — a line ending where the program takes
   * them as typed — goes only if the host's `confirmPaste` says the person agreed.
   */
  async paste(text: string): Promise<void> {
    try {
      if ((await this.source.paste(text)) !== 'unsafe') return
      if (await this.options.confirmPaste?.(text)) await this.source.paste(text, { allowUnsafe: true })
    } catch {
      // The session ended while the paste was on its way: there is nothing to paste into
    }
  }

  /** Copies what is selected, as a program wrote it. Answers whether there was anything to copy */
  async copy(): Promise<boolean> {
    if (!this.selection.span) return false
    let text: string
    try {
      text = await this.selection.text()
    } catch {
      return false
    }
    if (text === '') return false
    if (this.options.onCopy) this.options.onCopy(text)
    else await navigator.clipboard?.writeText(text).catch(() => {})
    return true
  }

  /** Puts the view in `host` and starts its frames */
  mount(host: HTMLElement): void {
    host.appendChild(this.element)
    this.observer = new ResizeObserver(() => this.fit())
    this.observer.observe(host)
    this.fit()
    this.resume()
  }

  /**
   * Takes the view off the page and stops its frames. What it has been sent is kept, so mounting it again
   * draws at once and asks only for what changed meanwhile.
   */
  unmount(): void {
    this.observer?.disconnect()
    this.observer = null
    this.pause()
    this.element.remove()
  }

  /** Measures how many cells fit, and reports it */
  fit(): void {
    const host = this.element.parentElement
    if (!host) return
    const { cellWidth, cellHeight } = this.painter.metrics
    const columns = Math.max(1, Math.floor((host.clientWidth - this.gutter) / cellWidth))
    const rows = Math.max(1, Math.floor(host.clientHeight / cellHeight))
    this.options.onFit?.({ columns, rows, cellWidth, cellHeight })
  }

  /** The view has, or has lost, the keyboard: an unfocused terminal shows a hollow cursor that does not blink */
  setFocused(focused: boolean): void {
    if (focused === this.focused) return
    this.focused = focused
    this.cursorShown = true
    if (this.blink) clearInterval(this.blink)
    this.blink = null
    if (focused && this.frame?.cursor?.blinking) this.startBlinking()
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
    if (this.disposed) return
    this.disposed = true
    this.unmount()
    this.element.removeEventListener('wheel', this.wheel)
    this.element.removeEventListener('pointerdown', this.pointerDown)
    this.element.removeEventListener('pointermove', this.pointerMove)
    this.element.removeEventListener('pointerup', this.pointerUp)
    this.element.removeEventListener('pointerleave', this.pointerLeave)
    this.element.removeEventListener('contextmenu', this.contextMenu)
    if (this.blink) clearInterval(this.blink)
    this.input.dispose()
    this.painter.dispose()
  }

  private resume(): void {
    if (this.detach || this.disposed) return
    // Rows that were on the screen when the frames stopped may have changed since and scrolled away, and
    // nothing said so. History older than that cannot have changed
    this.store.forgetFrom(this.store.firstActive)
    this.detach = this.source.attach({
      frame: (frame) => this.take(frame),
      ended: () => this.options.onEnded?.(),
    })
  }

  private pause(): void {
    this.detach?.()
    this.detach = null
    this.unacknowledged = null
    if (this.scheduled !== null) cancelAnimationFrame(this.scheduled)
    this.scheduled = null
  }

  private take(frame: Frame): void {
    const after = frame.epoch !== this.store.epoch ? this.viewport.acrossReset() : undefined
    const { reset, changed } = this.store.apply(frame)
    after?.()
    if (reset) {
      this.drawings.clear()
      this.asked.clear()
      // Every line number changed, so what was selected and hovered names lines that are gone
      this.selection.reset()
      this.hovered = null
      // And the lines its commands were on: asked for again, in this epoch
      this.commandList = []
      this.commandsSeen = -1
      this.matches = []
      this.currentMatch = -1
      this.finding += 1
    } else {
      for (const line of changed) this.drawings.delete(line)
    }
    const blinking = frame.cursor?.blinking === true
    if (blinking !== (this.frame?.cursor?.blinking === true) && this.focused) {
      if (this.blink) clearInterval(this.blink)
      this.blink = null
      this.cursorShown = true
      if (blinking) this.startBlinking()
    }
    if (frame.commands !== this.commandsSeen) {
      this.commandsSeen = frame.commands
      const epoch = frame.epoch
      void this.source.commands().then((commands) => {
        if (this.disposed || epoch !== this.store.epoch) return
        this.commandList = commands
        this.schedule()
      }, () => {})
    }
    this.frame = frame
    this.unacknowledged = frame.sequence
    if (frame.columns !== this.columns || frame.screenRows !== this.rows) {
      this.columns = frame.columns
      this.rows = frame.screenRows
      this.painter.resize(frame.columns, frame.screenRows, window.devicePixelRatio || 1, this.gutter)
    }
    this.followingMayHaveChanged()
    this.schedule()
  }

  private columns = 0
  private rows = 0

  private startBlinking(): void {
    this.blink = setInterval(() => {
      this.cursorShown = !this.cursorShown
      this.schedule()
    }, BLINK_MS)
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
   * instead, and a full-screen program that did not is sent arrow keys, which is what makes `less` and
   * `man` scroll under a wheel in every terminal.
   */
  private readonly wheel = (event: WheelEvent) => {
    const frame = this.frame
    if (!frame) return
    const { cellHeight } = this.painter.metrics
    // A wheel reports pixels, lines or pages; a trackpad reports pixels, which is what makes the view move
    // by fractions of a row
    const lines = event.deltaMode === 1 ? event.deltaY : event.deltaMode === 2 ? event.deltaY * this.rows : event.deltaY / cellHeight
    const toProgram = frame.modes.mouse && !event.shiftKey
    if (!toProgram && !frame.modes.alternateScreen) {
      if (this.store.scrollback === 0) return
      event.preventDefault()
      this.scrollBy(lines)
      return
    }
    event.preventDefault()
    // A program is told about whole notches, so fractions are kept until they add up to one
    this.wheelOwed += lines
    const notches = Math.trunc(this.wheelOwed)
    this.wheelOwed -= notches
    for (let sent = 0; sent < Math.min(Math.abs(notches), 20); sent += 1) {
      if (toProgram) this.source.pointer(this.pointerInput(event, notches < 0 ? 'wheelUp' : 'wheelDown', -1))
      else this.source.key({ action: 'press', code: notches < 0 ? 'ArrowUp' : 'ArrowDown', modifiers: 0, text: '', unshifted: 0 })
    }
  }

  /** Where the pointer is, in pixels from the grid's corner */
  private pointIn(event: MouseEvent): { x: number; y: number } {
    const box = this.painter.canvas.getBoundingClientRect()
    return { x: event.clientX - box.left - this.gutter, y: event.clientY - box.top }
  }

  /** The cell the pointer is over, as a line and a column */
  private positionOf(event: MouseEvent): Position {
    const { cellWidth, cellHeight } = this.painter.metrics
    const { x, y } = this.pointIn(event)
    const [first] = this.viewport.visible
    const row = Math.floor(y / cellHeight + this.viewport.offset)
    return {
      line: first + Math.max(0, Math.min(row, this.rows)),
      column: Math.max(0, Math.min(Math.floor(x / cellWidth), Math.max(0, this.columns - 1))),
    }
  }

  private pointerInput(event: MouseEvent, action: PointerInput['action'], button: number): PointerInput {
    const { cellWidth, cellHeight } = this.painter.metrics
    const { x, y } = this.pointIn(event)
    return { action, button, modifiers: modifiersOf(event), x: Math.max(0, x), y: Math.max(0, y), cellWidth, cellHeight }
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
    this.pressed = { button: event.button, reporting, at, moved: false }
    this.element.setPointerCapture?.(event.pointerId)
    if (reporting) {
      this.source.pointer(this.pointerInput(event, 'press', event.button))
      return
    }
    if (event.button !== 0) return
    if (event.shiftKey && this.selection.span) {
      this.selection.extendTo(at)
      return
    }
    const granularity: Granularity = event.detail >= 3 ? 'line' : event.detail === 2 ? 'word' : 'cell'
    if (granularity === 'cell') this.selection.clear()
    else this.selection.begin(at, granularity)
  }

  private readonly pointerMove = (event: PointerEvent) => {
    const pressed = this.pressed
    if (pressed?.reporting) {
      this.source.pointer(this.pointerInput(event, 'move', pressed.button))
      return
    }
    if (!pressed && this.frame?.modes.mouse && !event.shiftKey && this.viewport.following) {
      this.source.pointer(this.pointerInput(event, 'move', -1))
    }
    const at = this.positionOf(event)
    if (pressed?.button === 0) {
      // The first move off the cell that was pressed is where a selection starts: a click selects nothing
      if (!pressed.moved && (at.line !== pressed.at.line || at.column !== pressed.at.column)) {
        pressed.moved = true
        if (!this.selection.span) this.selection.begin(pressed.at, 'cell')
      }
      if (pressed.moved) this.selection.extendTo(at)
      return
    }
    this.hover(at)
  }

  private readonly pointerUp = (event: PointerEvent) => {
    const pressed = this.pressed
    this.pressed = null
    this.element.releasePointerCapture?.(event.pointerId)
    if (!pressed) return
    if (pressed.reporting) {
      this.source.pointer(this.pointerInput(event, 'release', pressed.button))
      return
    }
    // A click that did not drag, with the platform's modifier held, opens what it is on
    const opening = this.mac ? event.metaKey : event.ctrlKey
    if (pressed.button === 0 && !pressed.moved && opening) {
      const link = this.linkOn(this.positionOf(event))
      if (link) this.options.onLink?.(link, event)
    }
  }

  private readonly pointerLeave = () => this.hover(null)

  /** A program that owns the mouse is sent the right button; otherwise the page's menu is the host's to show */
  private readonly contextMenu = (event: MouseEvent) => {
    if (this.frame?.modes.mouse && !event.shiftKey) event.preventDefault()
  }

  private linkOn(at: Position): Link | null {
    const row = this.store.row(at.line)
    return row ? linkAt(row, this.store.links, at.column, this.detectors) : null
  }

  /** Marks the link under the pointer, which is how a person finds out a piece of text opens something */
  private hover(at: Position | null): void {
    const link = at ? this.linkOn(at) : null
    const before = this.hovered
    const same = before !== null && link !== null && at !== null && before.line === at.line && before.link.from === link.from && before.link.to === link.to
    if (same || (!before && !link)) return
    this.hovered = link && at ? { line: at.line, link } : null
    this.element.style.cursor = link ? 'pointer' : 'text'
    this.element.title = link ? link.target : ''
    this.schedule()
  }

  /** Dragging the scrollbar's thumb moves through history in proportion */
  private readonly thumbDown = (event: PointerEvent) => {
    event.preventDefault()
    event.stopPropagation()
    const start = { y: event.clientY, top: this.viewport.top }
    const move = (moved: PointerEvent) => {
      const { total } = this.viewport.extent
      const lines = ((moved.clientY - start.y) / Math.max(1, this.element.clientHeight)) * total
      this.scrollTo(start.top + lines)
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
  }

  private schedule(): void {
    if (this.scheduled !== null || this.disposed || !this.frame) return
    this.scheduled = requestAnimationFrame(() => {
      this.scheduled = null
      this.paint()
    })
  }

  private paint(): void {
    const frame = this.frame
    if (!frame) return
    const [first, count] = this.viewport.visible
    const rows: Array<RowDrawing | null> = []
    for (let line = first; line < first + count; line += 1) rows.push(this.drawingOf(line))
    this.ask(first, count)

    const cursor = this.cursorIn(frame, first, count)
    const scene: Scene = { theme: this.options.theme, rows, offset: this.viewport.offset, cursor, marks: this.marksIn(first, count) }
    this.painter.paint(scene)
    this.afterPaint(frame, first, count)

    if (this.unacknowledged !== null) {
      this.source.acknowledge(this.unacknowledged)
      this.unacknowledged = null
    }
  }

  /**
   * What follows a paint and is not paint: the text a page reader reads, the scrollbar, where an input
   * method's window goes, and the mark that says how far has been drawn.
   */
  private afterPaint(frame: Frame, first: number, count: number): void {
    // One element a row, touched only when its row is a different one from what it last showed
    while (this.mirror.childElementCount < count) this.mirror.appendChild(document.createElement('div'))
    while (this.mirror.childElementCount > count) this.mirror.lastElementChild!.remove()
    this.mirrored.length = count
    for (let index = 0; index < count; index += 1) {
      const row = this.store.row(first + index) ?? null
      if (this.mirrored[index] === row) continue
      this.mirrored[index] = row
      this.mirror.children[index]!.textContent = row ? textOf(row) : ''
    }

    const { total, position, shown } = this.viewport.extent
    const scrollable = total > shown
    const height = this.element.clientHeight
    this.thumb.style.height = scrollable ? `${Math.max(20, (shown / total) * height)}px` : '0'
    this.thumb.style.top = scrollable ? `${(position / total) * height}px` : '0'
    this.thumb.style.opacity = scrollable && !this.viewport.following ? '0.35' : '0'

    const { cellWidth, cellHeight } = this.painter.metrics
    if (frame.cursor) {
      const row = frame.firstActive + frame.cursor.row - first
      this.input.moveTo(frame.cursor.column * cellWidth, Math.max(0, row - this.viewport.offset) * cellHeight, cellHeight)
    }
    // How far a page looking for output to be on screen can count on: this frame, and these lines
    this.element.dataset.terminalDrawn = String(frame.sequence)
  }

  private drawingOf(line: number): RowDrawing | null {
    const row = this.store.row(line)
    if (!row) return null
    const selected: SelectedCells | undefined = this.selection.on(line)
    const hovered = this.hovered?.line === line ? this.hovered.link : null
    const matched = this.matches.length > 0 ? this.matchesOn(line) : []
    const key = `${selected ? `${selected.from}-${selected.to}` : ''}|${hovered ? `${hovered.from}-${hovered.to}` : ''}`
      + `|${matched.map((match) => `${match.from}-${match.to}${match.current ? '!' : ''}`).join(',')}`
    const kept = this.drawings.get(line)
    if (kept && kept.row === row && kept.selected === key) return kept.drawing
    let drawing = drawRow(row, this.store.styles, this.options.theme, selected)
    // Behind what a search found, over whatever background the cells had: later rectangles cover earlier
    if (matched.length > 0) {
      const { matchBackground = 0x623315, currentMatchBackground = 0x9e6a03 } = this.options.theme
      drawing = {
        ...drawing,
        backgrounds: [
          ...drawing.backgrounds,
          ...matched.map((match) => ({ column: match.from, cells: match.to - match.from, color: match.current ? currentMatchBackground : matchBackground })),
        ],
      }
    }
    // A link under the pointer is underlined, whatever it looked like before
    if (hovered) {
      drawing = {
        ...drawing,
        lines: [...drawing.lines, { kind: 'single', column: hovered.from, cells: hovered.to - hovered.from, color: this.options.theme.foreground }],
      }
    }
    this.drawings.set(line, { row, selected: key, drawing })
    return drawing
  }

  /** Asks for the lines in view that are not held, once each */
  private ask(first: number, count: number): void {
    for (const [from, length] of this.store.missing(first, count)) {
      if (this.asked.has(from)) continue
      this.asked.add(from)
      const epoch = this.store.epoch
      void this.source.fetch(from, length).then((fetched) => {
        this.asked.delete(from)
        if (this.disposed || epoch !== this.store.epoch) return
        for (const line of this.store.absorb(fetched)) this.drawings.delete(line)
        this.schedule()
      }, () => { this.asked.delete(from) })
    }
  }

  private cursorIn(frame: Frame, first: number, count: number): Scene['cursor'] {
    const cursor = frame.cursor
    if (!cursor || !this.cursorShown) return undefined
    const line = frame.firstActive + cursor.row
    if (line < first || line >= first + count) return undefined
    const row = this.store.row(line)
    const at = row ? row.column.indexOf(cursor.column) : -1
    const style = row && at !== -1 ? this.store.styles[row.style[at]!] : undefined
    return {
      column: cursor.column,
      row: line - first,
      cells: row && at !== -1 ? row.width[at]! : 1,
      shape: this.focused ? cursor.shape : 'hollow',
      text: row && at !== -1 ? { text: row.text[at]!, bold: ((style?.flags ?? 0) & 1) !== 0, italic: ((style?.flags ?? 0) & 2) !== 0 } : undefined,
    }
  }
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
