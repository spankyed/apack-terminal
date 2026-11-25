// A view: when it draws, when it says a frame is drawn, and what it asks for.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { CanvasPainter } from '../src/canvas-painter.ts'
import type { Clock } from '../src/clock.ts'
import type { Theme } from '../src/draw.ts'
import type { Painter, Scene } from '../src/painter.ts'
import { PLAIN, StyleFlag, paletteColor, type Fetched } from '../src/source.ts'
import { BLINK_MS, TerminalView } from '../src/view.ts'
import { recordingContext } from './_canvas.ts'
import { TestClock } from './_clock.ts'
import { framesWaiting, installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row, PALETTE } from './_support.ts'
import { mountedView, settled } from './_view.ts'

const theme: Theme = {
  background: 0x1e1e1e, palette: PALETTE, foreground: 0xd4d4d4, cursor: 0xffffff, cursorText: 0x000000,
  selectionBackground: 0x264f78, selectionForeground: null,
}

/** A painter that keeps the scenes it was asked to paint, with cells 10 by 20 */
class RecordingPainter implements Painter {
  readonly canvas = document.createElement('canvas')
  readonly metrics = { cellWidth: 10, cellHeight: 20, baseline: 16 }
  readonly scenes: Scene[] = []
  readonly sizes: Array<[number, number]> = []
  resize(columns: number, rows: number): void { this.sizes.push([columns, rows]) }
  setFont(): void {}
  paint(scene: Scene): void { this.scenes.push(scene) }
  dispose(): void {}
}

function mounted(options: { onFollowingChange?: (following: boolean) => void; onFit?: (fit: unknown) => void } = {}) {
  const source = new FakeSource()
  const painter = new RecordingPainter()
  const view = new TerminalView(source, { font: { family: 'monospace', size: 14 }, theme, painter: () => painter, ...options })
  const host = document.createElement('div')
  document.body.appendChild(host)
  view.mount(host)
  return { source, painter, view, host }
}

/** The characters a painted row would draw, or `null` for a blank one */
const textOf = (scene: Scene | undefined, index: number) =>
  scene?.rows[index]?.glyphs.map((glyph) => glyph.text).join('') ?? null

beforeEach(() => installFrames())

describe('when a view draws', () => {
  it('is once for a frame, on the next animation frame, and not again until something changes', () => {
    const { source, painter } = mounted()
    expect(framesWaiting(), 'a view with nothing to show asked to draw').toBe(0)

    source.send(frame({ rows: [row(0, 'hello')] }))
    expect(painter.scenes, 'drew inside the message that carried the frame').toHaveLength(0)

    runFrames()
    expect(painter.scenes).toHaveLength(1)
    expect(textOf(painter.scenes[0], 0)).toBe('hello')
    expect(framesWaiting(), 'a view with nothing new went on drawing').toBe(0)
  })

  it('is once for two frames that arrive before it has drawn', () => {
    const { source, painter } = mounted()
    source.send(frame({ sequence: 1, rows: [row(0, 'one')] }))
    source.send(frame({ sequence: 2, rows: [row(0, 'two')] }))

    expect(runFrames()).toBe(1)
    expect(textOf(painter.scenes.at(-1), 0)).toBe('two')
  })
})

describe('saying a frame is drawn', () => {
  it('happens after the paint and not before, which is what paces the source', () => {
    const { source } = mounted()
    source.send(frame({ sequence: 7, rows: [row(0, 'x')] }))
    expect(source.acknowledged, 'acknowledged a frame nobody has seen').toEqual([])

    runFrames()
    expect(source.acknowledged).toEqual([7])
  })

  it('names the last frame taken, once', () => {
    const { source } = mounted()
    source.send(frame({ sequence: 1 }))
    source.send(frame({ sequence: 2 }))
    runFrames()

    expect(source.acknowledged).toEqual([2])
  })

  it('happens when the paint failed too, or no frame would ever follow it', () => {
    const { source, painter } = mounted()
    painter.paint = () => { throw new Error('the canvas lost its context') }
    source.send(frame({ sequence: 3, rows: [row(0, 'x')] }))

    expect(() => runFrames()).toThrow('the canvas lost its context')
    expect(source.acknowledged).toEqual([3])
  })

  it('does not happen once the session has ended, which nothing is listening for', () => {
    const { source } = mounted()
    source.send(frame({ sequence: 4 }))
    source.listener!.ended()
    runFrames()

    expect(source.acknowledged).toEqual([])
  })
})

describe('a screen that arrives over several frames', () => {
  it('is drawn row by row as they come, and whole once the last has', () => {
    const { source, painter } = mounted()
    source.send(frame({ sequence: 1, rows: [row(0, 'before'), row(1, 'the'), row(2, 'reflow'), row(3, 'happened')] }))
    runFrames()

    // A new numbering, and only the first two rows of it fit the message
    source.send(frame({ sequence: 2, epoch: 2, rows: [row(0, 'one'), row(1, 'two')] }))
    runFrames()
    const rows = () => painter.scenes.at(-1)!.rows.map((_, index) => textOf(painter.scenes.at(-1), index))
    expect(rows(), 'a row of the numbering before was drawn, or one not sent yet was asked for').toEqual(['one', 'two', null, null])
    expect(source.fetched).toEqual([])
    expect(source.acknowledged).toEqual([1, 2])

    // The rest is owed once that is acknowledged, in a frame of its own
    source.send(frame({ sequence: 3, epoch: 2, rows: [row(2, 'three'), row(3, 'four')] }))
    runFrames()
    expect(rows()).toEqual(['one', 'two', 'three', 'four'])
  })
})

describe('a view that is not mounted', () => {
  it('is sent nothing, and asks again when it is mounted', () => {
    const { source, view, host, painter } = mounted()
    source.send(frame({ rows: [row(0, 'kept')] }))
    runFrames()

    view.unmount()
    expect(source.listener, 'an unmounted view went on taking frames').toBeNull()

    view.mount(host)
    expect(source.attached).toBe(2)
    source.send(frame({ sequence: 2, rows: [row(0, 'kept')] }))
    runFrames()
    expect(textOf(painter.scenes.at(-1), 0)).toBe('kept')
  })

  it('starts over when it is mounted again into another numbering, which the first frame of the attachment says', () => {
    const { source, view, host, painter } = mounted()
    source.send(frame({ firstActive: 10, scrollback: 10, rows: [row(5, 'history'), row(10, 'was live')] }))
    runFrames()

    view.unmount()
    view.mount(host)
    // What the session holds may be other lines by now, and these are other numbers for them
    source.send(frame({ sequence: 1, epoch: 2, firstActive: 3, scrollback: 3, rows: [row(3, 'live-now')] }))
    runFrames()

    expect(view.store.row(5), 'a line numbered in the attachment before was kept').toBeUndefined()
    expect(view.store.row(10)).toBeUndefined()
    expect(textOf(painter.scenes.at(-1), 0)).toBe('live-now')
  })

  it('asks again when mounted again after a session that could not be shown, and hears what the source said of it', () => {
    const reasons: Array<string | undefined> = []
    const { source, view, host } = mountedView({ onEnded: (reason) => { reasons.push(reason) } })
    source.listener!.ended('the session could not be drawn')
    expect(reasons).toEqual(['the session could not be drawn'])

    view.unmount()
    view.mount(host)
    source.send(frame({ sequence: 9, rows: [row(0, 'shown after all')] }))
    runFrames()
    expect(source.acknowledged, 'a view attached again went on as if its session were over').toEqual([9])
  })

  it('tells the program it lost the keyboard while there is still an attachment to say it on, and that it has it on the next', () => {
    const said: string[] = []
    const source = new (class extends FakeSource {
      override attach(listener: Parameters<FakeSource['attach']>[0]): () => void {
        said.push('attach')
        const detach = super.attach(listener)
        return () => { said.push('detach'); detach() }
      }
      override focus(gained: boolean): void { said.push(`focus ${gained}`) }
    })()
    const { view, host } = mountedView({}, source)
    view.setFocused(true)
    view.unmount()
    expect(said).toEqual(['attach', 'focus true', 'focus false', 'detach'])

    view.mount(host)
    view.setFocused(true)
    expect(said.slice(4)).toEqual(['attach', 'focus true'])
  })

  it('draws and acknowledges a frame its source hands over while it is still attaching', () => {
    const source = new (class extends FakeSource {
      override attach(listener: Parameters<FakeSource['attach']>[0]): () => void {
        const detach = super.attach(listener)
        listener.frame(frame({ sequence: this.attached, rows: [row(0, `attachment-${this.attached}`)] }))
        return detach
      }
    })()
    const { view, host, painter } = mountedView({}, source)
    runFrames()
    view.unmount()
    view.mount(host)
    runFrames()
    expect(textOf(painter.scenes.at(-1), 0)).toBe('attachment-2')
    expect(source.acknowledged).toEqual([1, 2])
  })

  it('is mounted once however often it is told to be, and moved when told to be somewhere else', () => {
    const watching = new Set<object>()
    globalThis.ResizeObserver = class { observe() { watching.add(this) } unobserve() {} disconnect() { watching.delete(this) } } as unknown as typeof ResizeObserver
    const { source, view, host } = mountedView()
    view.mount(host)
    expect(source.attached, 'mounted where it already was, it attached again').toBe(1)
    expect(watching.size).toBe(1)

    const elsewhere = document.createElement('div')
    document.body.appendChild(elsewhere)
    view.mount(elsewhere)
    expect(view.element.parentElement).toBe(elsewhere)
    expect(source.attached).toBe(2)
    expect(watching.size, 'the size of where it used to be is still watched').toBe(1)
  })

  it('draws nothing, whatever changes about it', () => {
    const { source, view } = mounted()
    source.send(frame({ rows: [row(0, 'kept')] }))
    runFrames()
    view.unmount()

    view.redraw()
    view.selectAll()
    view.scrollBy(-1)
    expect(framesWaiting()).toBe(0)
  })
})

describe('the grid a view draws', () => {
  it('is the one its frames describe, whatever fits where it is mounted', () => {
    const fits: unknown[] = []
    const source = new FakeSource()
    const painter = new RecordingPainter()
    const view = new TerminalView(source, { font: { family: 'monospace', size: 14 }, theme, painter: () => painter, onFit: (fit) => fits.push(fit) })
    const host = document.createElement('div')
    Object.defineProperty(host, 'clientWidth', { value: 200, configurable: true })
    Object.defineProperty(host, 'clientHeight', { value: 80, configurable: true })
    document.body.appendChild(host)
    view.mount(host)
    source.send(frame({ columns: 132, screenRows: 43 }))

    expect(painter.sizes.at(-1)).toEqual([132, 43])
    expect(fits.length, 'mounting did not report what fits').toBeGreaterThan(0)
  })
})

describe('what fits where a view is mounted', () => {
  const font = { family: 'monospace', size: 14 }
  const display = (ratio: number) => Object.defineProperty(window, 'devicePixelRatio', { value: ratio, configurable: true })
  /** The resolution queries a page was asked to watch, each with whoever is listening to it */
  const watched: Array<{ query: string; listeners: Set<() => void> }> = []

  beforeEach(() => {
    watched.length = 0
    window.matchMedia = ((query: string) => {
      const listeners = new Set<() => void>()
      watched.push({ query, listeners })
      return { addEventListener: (_: string, heard: () => void) => listeners.add(heard), removeEventListener: (_: string, heard: () => void) => listeners.delete(heard) }
    }) as unknown as typeof window.matchMedia
    Object.defineProperty(document, 'fonts', { value: new EventTarget(), configurable: true })
  })
  afterEach(() => {
    display(1)
    Reflect.deleteProperty(window, 'matchMedia')
    Reflect.deleteProperty(document, 'fonts')
  })

  /** A view drawing on a canvas whose characters measure 8.4 pixels, in a host 170 pixels wide and 80 tall */
  function fitted() {
    const { canvas, cell } = recordingContext(8.4)
    const fits: Array<{ columns: number; rows: number; cellWidth: number }> = []
    const source = new FakeSource()
    const view = new TerminalView(source, { font, theme, painter: (given) => new CanvasPainter(given, canvas), onFit: (fit) => fits.push(fit) })
    const host = document.createElement('div')
    Object.defineProperty(host, 'clientWidth', { value: 170 })
    Object.defineProperty(host, 'clientHeight', { value: 80 })
    document.body.appendChild(host)
    view.mount(host)
    return { view, source, host, canvas, cell, fits }
  }

  it('is counted in cells as the display draws them, from the first report', () => {
    display(2)
    const { fits } = fitted()
    // 8.4 is 8.5 at two device pixels to one: twenty fit in 170, where twenty-one of a cell of 8 would
    expect(fits[0]).toMatchObject({ columns: 20, rows: 4, cellWidth: 8.5 })
    expect(fits.every((fit) => fit.columns === 20), 'reported first in the cells of some other display').toBe(true)
  })

  it('is counted again when the view comes to be on a display with other pixels, and the canvas is sized for it', () => {
    display(2)
    const { source, canvas, fits } = fitted()
    source.send(frame({ rows: [row(0, 'x')] }))
    runFrames()
    expect(canvas.width).toBe(340)
    expect(watched.at(-1)?.query).toBe('(resolution: 2dppx)')

    display(1)
    for (const heard of [...watched.at(-1)!.listeners]) heard()

    expect(fits.at(-1)).toMatchObject({ columns: 21, cellWidth: 8 })
    expect(canvas.width).toBe(160)
    expect(framesWaiting(), 'a canvas that was sized, and so cleared, was not drawn again').toBe(1)
    expect(watched.at(-1)?.query, 'the next change of display would go unheard').toBe('(resolution: 1dppx)')
  })

  it('is counted again when a font finishes loading after the view measured its fallback', () => {
    const { source, cell, fits } = fitted()
    source.send(frame({ rows: [row(0, 'x')] }))
    runFrames()
    expect(fits.at(-1)).toMatchObject({ columns: 21 })

    cell.width = 10
    document.fonts.dispatchEvent(new Event('loadingdone'))

    expect(fits.at(-1)).toMatchObject({ columns: 17, cellWidth: 10 })
    expect(framesWaiting()).toBe(1)
  })

  it('is not counted again for a font the view does not draw in', () => {
    const { source, cell, fits } = fitted()
    source.send(frame({ rows: [row(0, 'x')] }))
    runFrames()
    const reported = fits.length
    const loaded = (family: string) => Object.assign(new Event('loadingdone'), { fontfaces: [{ family }] })

    cell.width = 10
    document.fonts.dispatchEvent(loaded('"Some Icons"'))
    expect(fits, 'an icon font loading measured the terminal again').toHaveLength(reported)
    expect(framesWaiting()).toBe(0)

    document.fonts.dispatchEvent(loaded('"Monospace"'))
    expect(fits.at(-1)).toMatchObject({ columns: 17 })
  })

  it('is counted again for another typeface', () => {
    const { view, cell, fits } = fitted()
    cell.width = 17
    view.setFont({ family: 'serif', size: 20 })
    expect(fits.at(-1)).toMatchObject({ columns: 10, cellWidth: 17 })
  })

  it('is not listened for by a view that is off the page', () => {
    const { view, fits } = fitted()
    view.unmount()
    const reported = fits.length
    expect(watched.every((query) => query.listeners.size === 0), 'still listening for the display to change').toBe(true)
    document.fonts.dispatchEvent(new Event('loadingdone'))
    expect(fits).toHaveLength(reported)
  })
})

describe('the colours a view draws in', () => {
  it('can be changed, and every row held is drawn again in them without being asked for', () => {
    const view = mountedView()
    view.source.send(frame({ rows: [row(0, 'red', { style: 1, styles: [{ ...PLAIN, fg: paletteColor(1) }] })] }))
    expect(view.painted().rows[0]?.glyphs[0]?.color).toBe(PALETTE[1])

    const light: Theme = { ...theme, background: 0xffffff, foreground: 0x000000, palette: PALETTE.map((color) => color ^ 0xffffff) }
    view.view.setTheme(light)

    expect(framesWaiting(), 'a change of theme did not ask to draw').toBe(1)
    const scene = view.painted()
    expect(scene.theme).toEqual(light)
    expect(scene.rows[0]?.glyphs[0]?.color).toBe(light.palette[1])
    expect(view.view.element.style.background).toBe('rgb(255, 255, 255)')
    expect(view.source.fetched).toEqual([])
  })
})

describe('a theme a host changed where it stands', () => {
  it('is drawn in when it is given again, the background behind rows that are not there included', () => {
    const changing = { ...theme }
    const view = mountedView({ theme: changing })
    view.source.send(frame({ rows: [row(0, 'x')] }))
    const before = view.painted().theme

    changing.background = 0xffffff
    changing.foreground = 0x000000
    view.view.setTheme(changing)

    const scene = view.painted()
    expect(scene.theme, 'a painter that knows a theme by what it is would take it for the same').not.toBe(before)
    expect([scene.theme.background, scene.rows[0]?.glyphs[0]?.color]).toEqual([0xffffff, 0x000000])
    expect(view.view.element.style.background).toBe('rgb(255, 255, 255)')
  })
})

describe('the colours a program set for itself', () => {
  const none = { foreground: null, background: null, cursor: null, palette: [] }
  /** A view showing a plain character, one in the palette's second colour, and one in a colour of its own */
  function coloured() {
    const view = mountedView()
    const rows = [row(0, 'p'), row(1, 'n', { style: 1, styles: [{ ...PLAIN, fg: paletteColor(1) }] }), row(2, 'x', { style: 1, styles: [{ ...PLAIN, fg: 0x123456, bg: 0x654321 }] })]
    view.source.send(frame({ rows, colors: none, cursor: { column: 0, row: 3, shape: 'block', blinking: false, hidden: false } }))
    const colours = () => {
      const scene = view.painted()
      return { background: scene.theme.background, cursor: scene.theme.cursor, glyphs: scene.rows.slice(0, 3).map((drawn) => drawn?.glyphs[0]?.color) }
    }
    return { ...view, colours }
  }
  const themed = { background: theme.background, cursor: theme.cursor, glyphs: [theme.foreground, PALETTE[1], 0x123456] }

  it('are what its rows are drawn in, with no row sent again, until it puts them back', () => {
    const view = coloured()
    expect(view.colours()).toEqual(themed)

    view.source.send(frame({ sequence: 2, colors: { foreground: 0xeeeeee, background: 0x102030, cursor: 0xff0000, palette: [] } }))
    expect(framesWaiting(), 'a change of colour did not ask to draw').toBe(1)
    expect(view.colours()).toEqual({ background: 0x102030, cursor: 0xff0000, glyphs: [0xeeeeee, PALETTE[1], 0x123456] })
    expect(view.view.element.style.background, 'the page around the canvas stayed the theme\'s').toBe('rgb(16, 32, 48)')
    expect(view.field.style.color).toBe('rgb(238, 238, 238)')

    // All of them, every time: one this does not name is the theme's again
    view.source.send(frame({ sequence: 3, colors: { ...none, cursor: 0x00ff00 } }))
    expect(view.colours()).toEqual({ ...themed, cursor: 0x00ff00 })

    view.source.send(frame({ sequence: 4, colors: none }))
    expect(view.colours()).toEqual(themed)
    expect(view.view.element.style.background).toBe('rgb(30, 30, 30)')
  })

  it('stay as they are through frames that do not say, a new numbering among them', () => {
    const view = coloured()
    view.source.send(frame({ sequence: 2, colors: { ...none, background: 0x102030 } }))
    view.source.send(frame({ sequence: 3, rows: [row(0, 'q')] }))
    expect(view.colours().background).toBe(0x102030)

    view.source.send(frame({ sequence: 4, epoch: 2, rows: [row(0, 'q')] }))
    expect(view.colours().background, 'a reflow put the colours back').toBe(0x102030)
  })

  it('include the numbered colours it redefined, which change how a character in one is drawn', () => {
    const view = coloured()
    view.source.send(frame({ sequence: 2, colors: { ...none, palette: [{ index: 1, rgb: 0xabcdef }, { index: 200, rgb: 0x010203 }] } }))
    expect(view.colours().glyphs).toEqual([theme.foreground, 0xabcdef, 0x123456])
    expect(view.painted().theme.palette[2], 'a colour it left alone').toBe(PALETTE[2])
  })

  it('stay the program\'s when the host changes its theme under them', () => {
    const view = coloured()
    view.source.send(frame({ sequence: 2, colors: { ...none, background: 0x102030, palette: [{ index: 1, rgb: 0xabcdef }] } }))
    const light: Theme = { ...theme, background: 0xffffff, foreground: 0x000000, cursor: 0x333333, palette: PALETTE.map((color) => color ^ 0xffffff) }
    view.view.setTheme(light)

    expect(view.colours()).toEqual({ background: 0x102030, cursor: 0x333333, glyphs: [0x000000, 0xabcdef, 0x123456] })
    view.source.send(frame({ sequence: 3, colors: none }))
    expect(view.painted().theme).toEqual(light)
  })

  it('change places, the two defaults and nothing else, while it has the screen in reverse video', () => {
    const view = coloured()
    view.source.send(frame({ sequence: 2, reverseVideo: true }))
    const scene = view.painted()
    expect([scene.theme.background, scene.theme.foreground]).toEqual([theme.foreground, theme.background])
    expect(scene.rows[0]?.glyphs[0]?.color, 'text with no colour of its own').toBe(theme.background)
    expect(scene.rows[2]?.glyphs[0]?.color, 'text with a colour of its own').toBe(0x123456)
    expect(scene.rows[2]?.backgrounds).toEqual([{ column: 0, cells: 1, color: 0x654321 }])
    expect(view.view.element.style.background).toBe('rgb(212, 212, 212)')

    // Over colours of the program's own, it is those that change places
    view.source.send(frame({ sequence: 3, reverseVideo: true, colors: { ...none, foreground: 0xeeeeee, background: 0x102030 } }))
    expect([view.painted().theme.background, view.painted().theme.foreground]).toEqual([0xeeeeee, 0x102030])

    view.source.send(frame({ sequence: 4, reverseVideo: false }))
    expect(view.colours().background).toBe(0x102030)
  })
})

describe('an attachment\'s first frame', () => {
  it('may say history is stale, and what a view kept of it from before is let go', async () => {
    const { source, view } = mounted()
    for (let line = 0; line < 100; line += 1) source.history.set(line, row(line, `line-${line}`))
    // The first frame of all, for a session that already has history
    source.send(frame({ firstActive: 100, scrollback: 100, staleFrom: 0, rows: [row(100, 'now')] }))
    runFrames()
    expect(view.store.row(100)?.text.join('')).toBe('now')
    expect(view.store.missing(0, 100)).toEqual([[0, 100]])

    view.scrollBy(-10)
    runFrames()
    await settled()
    expect(view.store.row(90)?.text.join(''), 'history named stale was not fetched when scrolled to').toBe('line-90')
  })
})

describe('scrolling back', () => {
  /** A view over 100 lines of history, of which the store holds only the screen */
  function withHistory() {
    const made = mounted()
    for (let line = 0; line < 100; line += 1) made.source.history.set(line, row(line, `line-${line}`))
    made.source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(100, 'now')] }))
    runFrames()
    return made
  }

  it('asks for the lines it does not hold, once, and draws them when they come', async () => {
    const { source, view, painter } = withHistory()
    view.scrollBy(-10)
    runFrames()
    // The whole of the page those lines are in that it lacks, which here is all the history there is
    expect(source.fetched).toEqual([[0, 100]])
    expect(textOf(painter.scenes.at(-1), 0), 'a line not yet held is drawn blank').toBeNull()

    view.redraw()
    runFrames()
    expect(source.fetched, 'asked again for lines already on their way').toHaveLength(1)

    await Promise.resolve()
    await Promise.resolve()
    runFrames()
    expect(textOf(painter.scenes.at(-1), 0)).toBe("line-90")
  })

  it('asks a page at a time, however slowly the view moves through the history', async () => {
    const made = mounted()
    for (let line = 0; line < 2000; line += 1) made.source.history.set(line, row(line, `line-${line}`))
    made.source.send(frame({ firstActive: 2000, scrollback: 2000, rows: [row(2000, 'now')] }))
    runFrames()

    // Three hundred lines, one line and one paint at a time
    for (let step = 0; step < 300; step += 1) {
      made.view.scrollBy(-1)
      runFrames()
      await Promise.resolve()
      await Promise.resolve()
    }
    runFrames()

    // The end of the page the screen's last history is in, then the whole page before it — each once
    expect(made.source.fetched).toEqual([[1792, 208], [1536, 256]])
    expect(textOf(made.painter.scenes.at(-1), 0)).toBe('line-1700')
  })

  it('does not ask again on every paint for lines an answer did not have', async () => {
    const { source, view } = mounted()
    // The owner says it holds a hundred lines of history, and answers for them with none
    source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(100, 'now')] }))
    view.scrollBy(-10)
    runFrames()
    await settled()
    for (let paint = 0; paint < 3; paint += 1) {
      view.redraw()
      runFrames()
      await settled()
    }
    expect(source.fetched, 'asked again for what the owner does not have').toHaveLength(1)

    // Until the owner says what it holds has changed
    source.history.set(95, row(95, 'here after all'))
    source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, staleFrom: 90 }))
    runFrames()
    expect(source.fetched).toHaveLength(2)
  })

  it('asks in the new numbering after a reflow, once, whenever the answer about the old one comes', async () => {
    const answers: Array<() => void> = []
    const source = new (class extends FakeSource {
      override fetch(first: number, count: number): Promise<Fetched> {
        const epoch = this.epoch
        this.fetched.push([first, count])
        return new Promise((resolve) => { answers.push(() => resolve({ epoch, first, count, rows: [] })) })
      }
    })()
    const { view } = mountedView({}, source)
    source.send(frame({ firstActive: 100, scrollback: 100 }))
    view.scrollBy(-10)
    runFrames()
    expect(source.fetched).toHaveLength(1)

    // Reflowed while that was on its way: the same lines are asked for as what they now are
    source.epoch = 2
    source.send(frame({ sequence: 2, epoch: 2, firstActive: 100, scrollback: 100 }))
    runFrames()
    expect(source.fetched).toHaveLength(2)

    answers[0]!()
    await settled()
    view.redraw()
    runFrames()
    expect(source.fetched, 'the answer about the old lines let the new ones be asked for twice').toHaveLength(2)
  })

  it('asks again for the lines an answer did not reach, and not for the ones it reached and did not hold', async () => {
    // An owner that answers for at most forty lines at a time, and holds only the even ones
    const asked: Array<[number, number]> = []
    const source = new (class extends FakeSource {
      override async fetch(first: number, count: number): Promise<Fetched> {
        asked.push([first, count])
        const answered = await super.fetch(first, Math.min(count, 40))
        return { ...answered, rows: answered.rows.filter((held) => held.line % 2 === 0) }
      }
    })()
    for (let line = 0; line < 100; line += 1) source.history.set(line, row(line, `line-${line}`))
    const { view, painter } = mountedView({}, source)
    source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(100, 'now')] }))
    view.scrollBy(-10)
    for (let round = 0; round < 6; round += 1) {
      runFrames()
      await settled()
    }

    expect(asked, 'each answer was followed by a question about what it left, and no other').toEqual([[0, 100], [40, 60], [80, 20]])
    expect(painter.scenes.at(-1)?.rows.map((drawn) => drawn?.glyphs.map((glyph) => glyph.text).join('') ?? null))
      .toEqual(['line-90', null, 'line-92', null])
  })

  it('asks in the numbering there is when an answer never came, without anyone being told it failed', async () => {
    const source = new (class extends FakeSource {
      refusing = true
      override async fetch(first: number, count: number): Promise<Fetched> {
        if (!this.refusing) return super.fetch(first, count)
        this.fetched.push([first, count])
        throw new Error('the lines have been numbered again')
      }
    })()
    source.history.set(90, row(90, 'line-90'))
    const { view, painter } = mountedView({}, source)
    source.send(frame({ firstActive: 100, scrollback: 100 }))
    view.scrollBy(-10)
    // Twice over, so that everything else a first frame sets going has been drawn
    for (let round = 0; round < 2; round += 1) {
      runFrames()
      await settled()
    }
    expect(framesWaiting(), 'a question with no answer was asked again in a circle').toBe(0)

    source.refusing = false
    view.redraw()
    runFrames()
    await settled()
    runFrames()
    expect(painter.scenes.at(-1)?.rows[0]?.glyphs.map((glyph) => glyph.text).join('')).toBe('line-90')
  })

  it('holds its place while output arrives, and says when it stops and starts following', () => {
    const following: boolean[] = []
    const { source, view, painter } = mounted({ onFollowingChange: (now) => following.push(now) })
    source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(95, 'reading-this')] }))
    view.scrollTo(95)
    runFrames()

    source.send(frame({ sequence: 2, firstActive: 500, scrollback: 500 }))
    runFrames()

    expect(textOf(painter.scenes.at(-1), 0), 'output moved the view').toBe('reading-this')
    view.follow()
    expect(following).toEqual([false, true])
  })

  it('moves by a fraction of a row for a trackpad, and the scene says by how much', () => {
    const { view, painter } = withHistory()
    view.element.dispatchEvent(new WheelEvent('wheel', { deltaY: -30, deltaMode: 0, cancelable: true }))
    runFrames()

    expect(view.viewport.top).toBe(98.5)
    expect(painter.scenes.at(-1)?.offset).toBeCloseTo(0.5)
    expect(painter.scenes.at(-1)?.rows, 'between two lines needs a row more').toHaveLength(5)
  })
})

describe('the cursor', () => {
  const withCursor = (blinking: boolean) =>
    frame({ rows: [row(0, 'ab')], cursor: { column: 1, row: 0, shape: 'bar', blinking, hidden: false } })

  it('is hollow while the view does not have the keyboard, and takes its shape when it does', () => {
    const { source, view, painter } = mounted()
    source.send(withCursor(false))
    runFrames()
    expect(painter.scenes.at(-1)?.cursor).toMatchObject({ column: 1, row: 0, shape: 'hollow', text: { text: 'b' } })

    view.setFocused(true)
    runFrames()
    expect(painter.scenes.at(-1)?.cursor?.shape).toBe('bar')
    expect(source.focus_, 'the source was not told the view has the keyboard').toEqual([true])
  })

  describe('that blinks', () => {
    function blinking() {
      const clock = new TestClock()
      const view = mountedView({ clock })
      view.view.setFocused(true)
      view.source.send(withCursor(true))
      return { ...view, clock }
    }

    it('is shown and hidden in turn, and shown from the start of a turn by anything typed or printed', () => {
      const view = blinking()
      expect(view.painted().cursor).toBeDefined()
      view.clock.advance(BLINK_MS)
      expect(view.painted().cursor, 'did not blink').toBeUndefined()

      // Typed half way through the hidden half: shown at once, and for a whole turn from then
      view.clock.advance(BLINK_MS / 2)
      view.key('keydown', { code: 'KeyA', key: 'a' })
      expect(view.painted().cursor).toBeDefined()
      view.clock.advance(BLINK_MS - 1)
      expect(view.painted().cursor, 'hidden again part way through its turn').toBeDefined()
      view.clock.advance(1)
      expect(view.painted().cursor).toBeUndefined()

      view.source.send({ ...withCursor(true), sequence: 2 })
      expect(view.painted().cursor, 'output left the cursor hidden').toBeDefined()
    })

    it('stops with the frames when the view is taken off the page, and starts again with them', () => {
      const view = blinking()
      runFrames()
      view.view.unmount()
      expect(view.clock.pending, 'a view that is not on the page went on blinking').toBe(0)

      view.view.mount(view.host)
      view.view.setFocused(true)
      view.source.send({ ...withCursor(true), sequence: 2 })
      view.clock.advance(BLINK_MS)
      expect(view.painted().cursor).toBeUndefined()
    })
  })

  it('is drawn in the inactive style the view is given while the view does not have the keyboard', () => {
    const { source, view, painter } = mountedView({ cursorInactive: 'underline' })
    source.send(withCursor(false))
    runFrames()
    expect(painter.scenes.at(-1)?.cursor?.shape).toBe('underline')

    view.setFocused(true)
    runFrames()
    expect(painter.scenes.at(-1)?.cursor?.shape, 'the program\'s shape once focused').toBe('bar')
  })

  describe('whose blink is the source\'s to say', () => {
    function focused(blinking: boolean) {
      const clock = new TestClock()
      const view = mountedView({ clock })
      view.view.setFocused(true)
      view.source.send(withCursor(blinking))
      return { ...view, clock }
    }

    it('blinks a cursor a frame says blinks', () => {
      const view = focused(true)
      expect(view.painted().cursor).toBeDefined()
      view.clock.advance(BLINK_MS)
      expect(view.painted().cursor, 'did not blink').toBeUndefined()
    })

    it('never hides a cursor a frame says is steady, and keeps no clock for it', () => {
      const view = focused(false)
      view.clock.advance(BLINK_MS * 3)
      expect(view.painted().cursor, 'hidden').toBeDefined()
      expect(view.clock.pending, 'a clock for a cursor that does not blink').toBe(0)
    })

    it('stops blinking when the next frame says the cursor is steady, and starts when one says it blinks', () => {
      const view = focused(true)
      view.source.send(withCursor(false))
      view.clock.advance(BLINK_MS * 3)
      expect(view.painted().cursor, 'still blinking a cursor the source steadied').toBeDefined()
      expect(view.clock.pending).toBe(0)

      view.source.send(withCursor(true))
      view.clock.advance(BLINK_MS)
      expect(view.painted().cursor, 'did not blink again').toBeUndefined()
    })
  })

  it('is not drawn while the view is scrolled away from it', () => {
    const { source, view, painter } = mounted()
    source.send(frame({ firstActive: 100, scrollback: 100, cursor: { column: 0, row: 0, shape: 'block', blinking: false, hidden: false } }))
    view.scrollTo(50)
    runFrames()

    expect(painter.scenes.at(-1)?.cursor).toBeUndefined()
  })
})

describe('cursor settings changed on a view that is up', () => {
  /** A view at the foot of a screen of history, with text selected: the state a remake would lose */
  function working(options: Parameters<typeof mountedView>[0], focused: boolean) {
    const clock = new TestClock()
    const view = mountedView({ clock, ...options })
    view.source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(100, 'alpha beta'), row(101, 'gamma')], cursor: { column: 1, row: 1, shape: 'bar', blinking: true, hidden: false } }))
    view.view.selection.select({ start: { line: 100, column: 0 }, end: { line: 100, column: 4 } })
    view.view.setFocused(focused)
    runFrames()
    return view
  }
  const kept = (view: ReturnType<typeof working>) => ({ top: view.view.viewport.top, span: view.view.selection.span, element: view.view.element })

  it('setCursorInactive: the next paint draws the new shape, and the view keeps its scroll and selection', () => {
    const view = working({ cursorInactive: 'underline' }, false)
    const before = kept(view)
    expect(view.painted().cursor?.shape).toBe('underline')

    view.view.setCursorInactive('bar')
    expect(view.painted().cursor?.shape, 'the paint kept the old shape').toBe('bar')
    expect(kept(view)).toEqual(before)

    view.view.scrollTo(50)
    const scrolled = kept(view)
    view.view.setCursorInactive('block')
    expect(kept(view), 'a scrolled-up view was moved').toEqual(scrolled)
    expect(scrolled.top).toBe(50)
  })
})

describe('text that blinks', () => {
  /** `ab`, of which the `a` blinks */
  const blinkingRow = () => ({ ...row(0, 'ab'), style: Uint16Array.from([1, 0]), styles: [PLAIN, { ...PLAIN, flags: StyleFlag.blink }] })
  const drawn = (view: ReturnType<typeof mountedView>) => view.painted().rows[0]?.glyphs.map((glyph) => glyph.text)

  it('is drawn and hidden in turn, by a clock that what is printed does not start over', () => {
    const clock = new TestClock()
    const view = mountedView({ clock })
    view.source.send(frame({ rows: [blinkingRow()] }))
    expect(drawn(view)).toEqual(['a', 'b'])
    clock.advance(BLINK_MS)
    expect(drawn(view), 'did not blink').toEqual(['b'])

    // Printed half way through the hidden half: a program that keeps printing must not keep it shown
    clock.advance(BLINK_MS / 2)
    view.source.send(frame({ sequence: 2, rows: [blinkingRow()] }))
    expect(drawn(view), 'output showed it again').toEqual(['b'])
    clock.advance(BLINK_MS / 2)
    expect(drawn(view)).toEqual(['a', 'b'])
  })

  it('costs no clock where none is in view, and is shown once the last of it has gone', () => {
    const clock = new TestClock()
    const view = mountedView({ clock })
    view.source.send(frame({ rows: [row(0, 'ab')] }))
    view.painted()
    expect(clock.pending, 'a clock for text that does not blink').toBe(0)

    view.source.send(frame({ sequence: 2, rows: [blinkingRow()] }))
    view.painted()
    expect(clock.pending).toBe(1)
    clock.advance(BLINK_MS)

    view.source.send(frame({ sequence: 3, rows: [row(0, 'ab')] }))
    expect(drawn(view)).toEqual(['a', 'b'])
    expect(clock.pending, 'the clock outlived the text').toBe(0)
  })

  it('stops with the frames when the view is taken off the page', () => {
    const clock = new TestClock()
    const view = mountedView({ clock })
    view.source.send(frame({ rows: [blinkingRow()] }))
    view.painted()
    view.view.unmount()
    expect(clock.pending, 'a view that is not on the page went on blinking').toBe(0)
  })
})

describe('view lifecycle, guards, and edge-case regressions', () => {
  it('guards screenRows === 0 in paint, marksIn, positionOf, and selectAll', () => {
    const view = mountedView({ gutter: 6 })
    view.source.send(frame({ screenRows: 0, columns: 20, rows: [] }))
    expect(() => runFrames()).not.toThrow()
    expect(() => view.view.selectAll()).not.toThrow()
    expect(() => view.point('pointermove', 0, 0)).not.toThrow()
    expect(view.view.selection.span).toBeNull()
  })

  it('reschedules fetch when an in-flight page was invalidated by staleFrom and absorbed nothing', async () => {
    const resolvers: Array<(fetched: Fetched) => void> = []
    const source = new (class extends FakeSource {
      override fetch(first: number, count: number): Promise<Fetched> {
        this.fetched.push([first, count])
        return new Promise((resolve) => { resolvers.push(resolve) })
      }
    })()
    const view = mountedView({}, source)
    source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(100, 'live')] }))
    view.view.scrollTo(90)
    runFrames()
    expect(source.fetched).toHaveLength(1)

    // staleFrom arrives while the first fetch is in flight
    source.send(frame({ sequence: 2, firstActive: 100, scrollback: 100, staleFrom: 0 }))
    runFrames()
    // First fetch resolves with stale rows -> absorb returns [] (nothing kept), must reschedule!
    resolvers[0]!({ epoch: 1, first: 0, count: 100, rows: [row(90, 'old')] })
    await settled()
    expect(framesWaiting(), 'view left invalidated lines permanently blank').toBe(1)
    runFrames()
    expect(source.fetched).toHaveLength(2)
  })

  it('ignores out-of-order commands() and images() responses within the same epoch', async () => {
    const cmdResolvers: Array<(c: readonly import('../src/source.ts').Command[]) => void> = []
    const imgResolvers: Array<(p: import('../src/source.ts').Placements) => void> = []
    const source = new (class extends FakeSource {
      override commands() { return new Promise<readonly import('../src/source.ts').Command[]>((r) => { cmdResolvers.push(r) }) }
      override images() { return new Promise<import('../src/source.ts').Placements>((r) => { imgResolvers.push(r) }) }
    })()
    const view = mountedView({}, source)
    source.send(frame({ sequence: 1, commands: 1, images: 1 }))
    source.send(frame({ sequence: 2, commands: 2, images: 2 }))
    const newerCmd = { id: 2, command: 'new', cwd: null, startedMs: null, finishedMs: null, outcome: { kind: 'running' as const }, prompt: 0, output: null, end: null }
    const olderCmd = { id: 1, command: 'old', cwd: null, startedMs: null, finishedMs: null, outcome: { kind: 'running' as const }, prompt: 0, output: null, end: null }
    cmdResolvers[1]!([newerCmd])
    await settled()
    cmdResolvers[0]!([olderCmd])
    await settled()
    expect(view.view.commands.map((c) => c.id)).toEqual([2])

    imgResolvers[1]!({ cellWidth: 10, cellHeight: 20, placements: [{ image: 2, generation: 1, z: 0, line: 0, column: 0, columns: 1, rows: 1, offsetX: 0, offsetY: 0, width: 10, height: 20, sourceX: 0, sourceY: 0, sourceWidth: 10, sourceHeight: 20 }] })
    await settled()
    imgResolvers[0]!({ cellWidth: 10, cellHeight: 20, placements: [] })
    await settled()
    expect(source.picturesAsked).toContain(2)
  })

  it('does not emit onFit for zero-sized containers and deduplicates identical fit emissions', () => {
    const fits: Array<{ columns: number; rows: number; cellWidth: number; cellHeight: number }> = []
    const source = new FakeSource()
    const painter = new RecordingPainter()
    const view = new TerminalView(source, { font: { family: 'monospace', size: 14 }, theme, painter: () => painter, onFit: (fit) => fits.push(fit) })
    const host = document.createElement('div')
    document.body.appendChild(host)
    // Zero-sized (hidden display:none) container emits nothing
    view.mount(host)
    expect(fits).toEqual([])

    let w = 200
    let h = 80
    Object.defineProperty(host, 'clientWidth', { get: () => w, configurable: true })
    Object.defineProperty(host, 'clientHeight', { get: () => h, configurable: true })
    view.fit()
    view.fit()
    expect(fits).toEqual([{ columns: 20, rows: 4, cellWidth: 10, cellHeight: 20 }])
    w = 210
    view.fit()
    expect(fits).toHaveLength(2)
  })

  it('cleans up thumb drag, mouse press, refresh timer on unmount, blocks mount after dispose, and guards find/paste after dispose', async () => {
    const clock = new TestClock()
    const source = new FakeSource()
    let resolveSearch!: (f: import('../src/source.ts').Found) => void
    source.search = () => new Promise((r) => { resolveSearch = r })
    let resolvePaste!: (o: import('../src/source.ts').PasteOutcome) => void
    source.paste = () => new Promise((r) => { resolvePaste = r })
    const view = mountedView({ clock }, source)
    source.send(frame({ firstActive: 100, scrollback: 100, modes: { alternateScreen: false, mouse: true }, rows: [row(100, 'hi')] }))
    runFrames()

    // Active mouse reporting press is released on unmount
    view.point('pointerdown', 1, 0)
    view.view.unmount()
    expect(source.pointers.map((p) => p.action)).toEqual(['press', 'release'])
    expect(clock.pending).toBe(0)

    // Remount, start search & paste, then dispose before they resolve
    view.view.mount(view.host)
    const findPromise = view.view.find('hi')
    const pastePromise = view.view.paste('abc')
    view.view.scrollTo(50)
    view.view.dispose()
    expect(clock.pending).toBe(0)
    // mount() after dispose is a no-op
    const attachedBefore = source.attached
    view.view.mount(view.host)
    expect(source.attached).toBe(attachedBefore)

    resolveSearch({ spans: [{ start: { line: 100, column: 0 }, end: { line: 100, column: 1 } }], more: false })
    resolvePaste('pasted')
    await Promise.all([findPromise, pastePromise])
    expect(view.view.found.total).toBe(0)
    expect(view.view.viewport.following).toBe(false)
  })

  it('notifies onFound on epoch reset when there was an active search and cancels refresh timer', async () => {
    const clock = new TestClock()
    const foundEvents: Array<{ current: number; total: number; more: boolean }> = []
    const source = new FakeSource()
    source.search = async () => ({ spans: [{ start: { line: 0, column: 0 }, end: { line: 0, column: 1 } }], more: false })
    const view = mountedView({ clock, onFound: (f) => foundEvents.push(f) }, source)
    source.send(frame({ sequence: 1, epoch: 1, rows: [row(0, 'ab')] }))
    await view.view.find('ab')
    // Schedule a refresh timer
    source.send(frame({ sequence: 2, epoch: 1, rows: [row(0, 'ac')] }))
    expect(clock.pending).toBe(1)
    // Epoch reset cancels timer and notifies onFound
    source.send(frame({ sequence: 3, epoch: 2, rows: [row(0, 'new')] }))
    expect(clock.pending).toBe(0)
    expect(foundEvents).toEqual([{ current: 0, total: 0, more: false }])
  })

  it('clamps pointer reporting coordinates, ignores secondary press while active, sanitizes link tooltips, and hides invisible/blinked cursor text & covers wide cursor', () => {
    const view = mountedView({ linkPreview: 'osc8' })
    const invisibleStyle = { ...PLAIN, flags: StyleFlag.invisible }
    const wideRow = {
      ...row(0, 'a日b', { links: ['https://example.com/\u202eevil\x07'] }),
      column: Uint16Array.from([0, 1, 3]),
      width: Uint8Array.from([1, 2, 1]),
      style: Uint16Array.from([1, 0, 0]),
      styles: [PLAIN, invisibleStyle],
      link: Uint16Array.from([0, 0, 1]),
    }
    view.source.send(frame({
      columns: 20, screenRows: 4,
      modes: { alternateScreen: false, mouse: true },
      cursor: { column: 0, row: 0, shape: 'block', blinking: false, hidden: false },
      rows: [wideRow],
    }))
    // Invisible cell under block cursor reveals no text
    expect(view.painted().cursor?.text).toBeUndefined()

    // Cursor on second cell of wide character snaps to column 1 with width 2
    view.source.send(frame({
      sequence: 2, columns: 20, screenRows: 4,
      modes: { alternateScreen: false, mouse: true },
      cursor: { column: 2, row: 0, shape: 'block', blinking: false, hidden: false },
      rows: [wideRow],
    }))
    expect(view.painted().cursor).toMatchObject({ column: 1, cells: 2, text: { text: '日' } })

    // Pointer reporting clamps coordinates to [0, columns*cellWidth] x [0, rows*cellHeight] and ignores secondary button press
    view.point('pointerdown', 100, 100, { button: 0 })
    view.point('pointerdown', 2, 1, { button: 2 })
    view.point('pointerup', 2, 1, { button: 2 })
    view.point('pointerup', 100, 100, { button: 0 })
    expect(view.source.pointers.map((p) => [p.action, p.button, p.x, p.y])).toEqual([
      ['press', 0, 200, 80],
      ['release', 0, 200, 80],
    ])

    // Hovering link strips bidi override and control chars from tooltip title
    view.source.send(frame({ sequence: 3, columns: 20, screenRows: 4, modes: { alternateScreen: false, mouse: false }, rows: [wideRow] }))
    runFrames()
    view.point('pointermove', 3, 0)
    expect(view.view.element.title).toBe('https://example.com/evil')
  })

  it('falls back to min/max-resolution and window resize when fractional DPR exact query does not match', () => {
    const queries: string[] = []
    const originalMatchMedia = window.matchMedia
    Object.defineProperty(window, 'devicePixelRatio', { value: 1.25, configurable: true })
    window.matchMedia = ((query: string) => {
      queries.push(query)
      return { matches: !query.startsWith('(resolution:'), addEventListener() {}, removeEventListener() {} }
    }) as unknown as typeof window.matchMedia
    try {
      const { canvas } = recordingContext(8)
      const fits: Array<{ cellWidth: number }> = []
      const view = new TerminalView(new FakeSource(), { font: { family: 'monospace', size: 14 }, theme, painter: (f) => new CanvasPainter(f, canvas), onFit: (fit) => fits.push(fit) })
      const host = document.createElement('div')
      Object.defineProperty(host, 'clientWidth', { value: 200, configurable: true })
      Object.defineProperty(host, 'clientHeight', { value: 80, configurable: true })
      document.body.appendChild(host)
      view.mount(host)
      expect(queries.some((q) => q.includes('min-resolution') && q.includes('max-resolution'))).toBe(true)

      Object.defineProperty(window, 'devicePixelRatio', { value: 1.5, configurable: true })
      window.dispatchEvent(new Event('resize'))
      expect(queries.at(-1)).toContain('min-resolution')
      view.dispose()
    } finally {
      Object.defineProperty(window, 'devicePixelRatio', { value: 1, configurable: true })
      window.matchMedia = originalMatchMedia
    }
  })

  it('resubscribes matchMedia when ResizeObserver detects a DPR change before window resize', () => {
    const originalMatchMedia = window.matchMedia
    const originalResizeObserver = globalThis.ResizeObserver
    let observerCallback: (() => void) | null = null
    const subscriptions: Array<{ query: string; added: number; removed: number; fire: () => void }> = []
    Object.defineProperty(window, 'devicePixelRatio', { value: 1, configurable: true })
    window.matchMedia = ((query: string) => {
      let listener: (() => void) | null = null
      const entry = {
        query,
        added: 0,
        removed: 0,
        fire: () => { listener?.() },
      }
      subscriptions.push(entry)
      return {
        matches: true,
        addEventListener(_type: string, fn: () => void) { entry.added += 1; listener = fn },
        removeEventListener(_type: string, fn: () => void) { if (listener === fn) { entry.removed += 1; listener = null } },
      }
    }) as unknown as typeof window.matchMedia
    globalThis.ResizeObserver = class {
      constructor(cb: () => void) { observerCallback = cb }
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof ResizeObserver
    try {
      const { canvas } = recordingContext(8)
      const view = new TerminalView(new FakeSource(), { font: { family: 'monospace', size: 14 }, theme, painter: (f) => new CanvasPainter(f, canvas) })
      const host = document.createElement('div')
      document.body.appendChild(host)
      view.mount(host)
      expect(subscriptions.map((s) => s.query)).toEqual(['(resolution: 1dppx)'])

      // DPR changes and ResizeObserver fires BEFORE window resize
      Object.defineProperty(window, 'devicePixelRatio', { value: 2, configurable: true })
      observerCallback!()
      expect(subscriptions[0]!.removed, 'previous 1dppx matchMedia listener was not cleaned up').toBe(1)
      expect(subscriptions.map((s) => s.query)).toEqual(['(resolution: 1dppx)', '(resolution: 2dppx)'])

      // Subsequent window resize sees pixelRatio already updated on this view (its 2dppx listener stays active),
      // and firing the new 2dppx listener handles future DPR changes cleanly
      window.dispatchEvent(new Event('resize'))
      expect(subscriptions[1]!.removed, 'window resize should not tear down the already-updated 2dppx listener').toBe(0)

      Object.defineProperty(window, 'devicePixelRatio', { value: 3, configurable: true })
      subscriptions[1]!.fire()
      expect(subscriptions[1]!.removed).toBe(1)
      expect(subscriptions.at(-1)?.query).toBe('(resolution: 3dppx)')

      view.dispose()
      expect(subscriptions.at(-1)?.removed).toBe(1)
    } finally {
      Object.defineProperty(window, 'devicePixelRatio', { value: 1, configurable: true })
      window.matchMedia = originalMatchMedia
      globalThis.ResizeObserver = originalResizeObserver
    }
  })

  it('does not re-arm search refresh timer when an in-flight refresh finishes while unmounted or after unmount reset', async () => {
    const clock = new TestClock()
    const resolvers: Array<(f: import('../src/source.ts').Found) => void> = []
    const foundEvents: Array<{ current: number; total: number; more: boolean }> = []
    const source = new FakeSource()
    source.search = () => new Promise((resolve) => { resolvers.push(resolve) })
    const view = mountedView({ clock, onFound: (f) => foundEvents.push(f) }, source)
    source.send(frame({ sequence: 1, rows: [row(0, 'hi')] }))

    const initial = view.view.find('hi')
    resolvers[0]!({ spans: [{ start: { line: 0, column: 0 }, end: { line: 0, column: 1 } }], more: false })
    await initial

    // Trigger refresh timer, advance to kick off in-flight runRefresh(), then send another frame to set refreshAgain
    source.send(frame({ sequence: 2, rows: [row(0, 'hi there')] }))
    expect(clock.pending).toBe(1)
    clock.advance(200)
    expect(resolvers).toHaveLength(2)
    source.send(frame({ sequence: 3, rows: [row(0, 'hi again')] }))

    // Unmount while refresh is in flight -> resets refreshAgain and sets listening = false
    view.view.unmount()
    resolvers[1]!({ spans: [{ start: { line: 0, column: 0 }, end: { line: 0, column: 1 } }, { start: { line: 0, column: 3 }, end: { line: 0, column: 4 } }], more: false })
    await Promise.resolve()
    await Promise.resolve()
    expect(clock.pending, 'in-flight refresh must not re-arm timer while unmounted').toBe(0)
    expect(foundEvents, 'in-flight refresh must not notify onFound while unmounted').toEqual([])

    // Remount, start another in-flight refresh with refreshAgain, unmount + remount before it resolves -> refreshAgain was cleared
    view.view.mount(view.host)
    source.send(frame({ sequence: 4, rows: [row(0, 'hi remounted')] }))
    clock.advance(200)
    expect(resolvers).toHaveLength(3)
    source.send(frame({ sequence: 5, rows: [row(0, 'hi more')] }))
    view.view.unmount()
    view.view.mount(view.host)
    resolvers[2]!({ spans: [{ start: { line: 0, column: 0 }, end: { line: 0, column: 1 } }], more: false })
    await Promise.resolve()
    await Promise.resolve()
    expect(clock.pending, 'unmount should have cleared stale refreshAgain flag').toBe(0)
  })

  it('clamps currentMatch to MATCH_HIGHLIGHTS when search refresh pushes the selected match past the highlight cap', async () => {
    const clock = new TestClock()
    const spanAt = (line: number) => ({ start: { line, column: 0 }, end: { line, column: 1 } })
    // Initial search returns 1000 matches (lines 999 down to 0); navigate to the oldest match (index 999, line 0).
    let spans = Array.from({ length: 1000 }, (_, i) => spanAt(999 - i))
    const foundEvents: Array<{ current: number; total: number; more: boolean }> = []
    const source = new FakeSource()
    source.search = async () => ({ spans, more: false })
    const { view } = mountedView({ clock, onFound: (f) => foundEvents.push(f) }, source)
    source.send(frame({ sequence: 1, rows: [row(0, 'hi')] }))

    await view.find('hi')
    // Wrapping backwards from index 0 lands on index 999 (line 0, current = 1000).
    view.findNext(-1)
    expect(view.found).toEqual({ current: 1000, total: 1000, more: false })

    // New output prepends 200 newer matches (lines 1199 down to 0, 1200 total), pushing line 0 to index 1199 (> 1000).
    spans = Array.from({ length: 1200 }, (_, i) => spanAt(1199 - i))
    source.send(frame({ sequence: 2, rows: [row(0, 'hi')] }))
    await clock.advanceAsync(200)

    expect(view.found.total).toBe(1000)
    expect(view.found.more).toBe(true)
    expect(view.found.current, 'current match must stay within held MATCH_HIGHLIGHTS bounds').toBeLessThanOrEqual(view.found.total)
    expect(view.found.current).toBe(1000)
    expect(foundEvents.at(-1)).toEqual({ current: 1000, total: 1000, more: true })
  })

  it('fires onFollowingChange(true) when setFolds snaps a scrolled-back view back to following', () => {
    const following: boolean[] = []
    const { source, view } = mounted({ onFollowingChange: (value) => following.push(value) })
    // Lines 5..19 (oldest = 5, firstActive = 15, screenRows = 5)
    source.send(frame({
      sequence: 1,
      screenRows: 5,
      scrollback: 10,
      firstActive: 15,
      rows: Array.from({ length: 5 }, (_, i) => row(15 + i, `line ${15 + i}`)),
    }))
    runFrames()

    // Scroll back into history -> following becomes false
    view.scrollTo(5)
    expect(following).toEqual([false])

    // Fold starting before oldest (0..15) hides all remaining history (5..15), advancing visibleOldest past firstActive
    view.setFolds([{ first: 0, last: 15, label: 'folded' }])
    expect(following, 'setFolds snapping back to following must notify onFollowingChange(true)').toEqual([false, true])
  })

  it('drives cursor blink, text blink, search refresh, and key-text dedupe through one shared clock with zero leaks on teardown', async () => {
    const clock = new TestClock()
    const blinkingRow = { ...row(0, 'ab'), style: Uint16Array.from([1, 0]), styles: [PLAIN, { ...PLAIN, flags: StyleFlag.blink }] }
    const one = mountedView({ clock })
    const two = mountedView({ clock })

    one.view.setFocused(true)
    one.source.send(frame({ sequence: 1, rows: [blinkingRow], cursor: { column: 0, row: 0, shape: 'block', blinking: true, hidden: false } }))
    two.view.setFocused(true)
    two.source.send(frame({ sequence: 1, rows: [row(0, 'hi')], cursor: { column: 0, row: 0, shape: 'block', blinking: true, hidden: false } }))
    runFrames()
    // one has cursor blink + text blink; two has cursor blink
    expect(clock.pending).toBe(3)

    // Two rapid keypresses on `two` cancel-before-arm `keyTextTimer` (only 1 pending keyText timer, not 2)
    two.key('keydown', { code: 'KeyA', key: 'a' })
    two.key('keydown', { code: 'KeyB', key: 'b' })
    expect(clock.pending).toBe(4)

    // Unmounting `two` clears both its cursor blink and its deferred keyText timer
    two.view.unmount()
    expect(clock.pending).toBe(2)

    // Advancing blinks `one`, and disposing `one` clears every remaining timer on the shared clock
    clock.advance(BLINK_MS)
    expect(one.painted().cursor).toBeUndefined()
    expect(one.painted().rows[0]?.glyphs.map((g) => g.text)).toEqual(['b'])
    one.view.dispose()
    two.view.dispose()
    expect(clock.pending).toBe(0)
  })

  it('handles synchronous after(0) clocks and ignores stale late-firing callbacks cleanly', () => {
    let cancels = 0
    const late: Array<() => void> = []
    const trickyClock: Clock = {
      after(ms, run) {
        if (ms === 0) {
          // Fires synchronously before returning the cancel function
          run()
          return () => { cancels += 1 }
        }
        late.push(run)
        return () => { cancels += 1 }
      },
    }
    const view = mountedView({ clock: trickyClock })
    view.view.setFocused(true)
    view.source.send(frame({ sequence: 1, cursor: { column: 0, row: 0, shape: 'block', blinking: true, hidden: false } }))
    runFrames()

    // Keypress arms after(0), which fires synchronously; subsequent dispose must NOT invoke a stale cancel handle for it
    view.key('keydown', { code: 'KeyA', key: 'a' })
    const blinkCancelCountBefore = cancels
    view.view.unmount()
    expect(cancels, 'only the active blink timer should be cancelled, not an already-fired synchronous timer').toBe(blinkCancelCountBefore + 1)

    // A buggy clock firing a cancelled blink callback late must be ignored and must not resurrect blinking
    const beforeLate = late.length
    for (const stale of [...late]) stale()
    expect(late.length, 'stale blink callback must not re-arm timer after unmount').toBe(beforeLate)
  })

  it('uses the default setTimeout/clearTimeout clock when options.clock is omitted', () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
      const view = mountedView()
      view.view.setFocused(true)
      view.source.send(frame({ sequence: 1, cursor: { column: 0, row: 0, shape: 'block', blinking: true, hidden: false } }))
      expect(view.painted().cursor).toBeDefined()
      vi.advanceTimersByTime(BLINK_MS)
      expect(view.painted().cursor).toBeUndefined()
      view.view.dispose()
      expect(vi.getTimerCount()).toBe(0)
    } finally {
      vi.useRealTimers()
    }
  })
})
