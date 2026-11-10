// A view: when it draws, when it says a frame is drawn, and what it asks for.
import { beforeEach, describe, expect, it } from 'vitest'
import type { Theme } from '../src/draw.ts'
import type { Painter, Scene } from '../src/painter.ts'
import { TerminalView } from '../src/view.ts'
import { framesWaiting, installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'

const theme: Theme = {
  background: 0x1e1e1e, foreground: 0xd4d4d4, cursor: 0xffffff, cursorText: 0x000000,
  selectionBackground: 0x264f78, selectionForeground: null,
}

/** A painter that keeps the scenes it was asked to paint, with cells 10 by 20 */
class RecordingPainter implements Painter {
  readonly canvas = document.createElement('canvas')
  readonly metrics = { cellWidth: 10, cellHeight: 20, baseline: 16 }
  readonly scenes: Scene[] = []
  readonly sizes: Array<[number, number]> = []
  resize(columns: number, rows: number): void { this.sizes.push([columns, rows]) }
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

  it('does not trust what was on the screen when it stopped listening', () => {
    const { source, view, host } = mounted()
    source.send(frame({ firstActive: 10, scrollback: 10, rows: [row(5, 'history'), row(10, 'was live')] }))
    runFrames()

    view.unmount()
    view.mount(host)

    expect(view.store.row(5), 'history, which cannot have changed, was thrown away').toBeDefined()
    expect(view.store.row(10), 'a line that may have changed unseen was kept').toBeUndefined()
  })
})

describe('the grid a view draws', () => {
  it('is the one its frames describe, whatever fits where it is mounted', () => {
    const fits: unknown[] = []
    const { source, painter } = mounted({ onFit: (fit) => fits.push(fit) })
    source.send(frame({ columns: 132, screenRows: 43 }))

    expect(painter.sizes.at(-1)).toEqual([132, 43])
    expect(fits.length, 'mounting did not report what fits').toBeGreaterThan(0)
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
    expect(source.fetched).toEqual([[90, 4]])
    expect(textOf(painter.scenes.at(-1), 0), 'a line not yet held is drawn blank').toBeNull()

    view.redraw()
    runFrames()
    expect(source.fetched, 'asked again for lines already on their way').toHaveLength(1)

    await Promise.resolve()
    await Promise.resolve()
    runFrames()
    expect(textOf(painter.scenes.at(-1), 0)).toBe("line-90")
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
    frame({ rows: [row(0, 'ab')], cursor: { column: 1, row: 0, shape: 'bar', blinking } })

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

  it('is not drawn while the view is scrolled away from it', () => {
    const { source, view, painter } = mounted()
    source.send(frame({ firstActive: 100, scrollback: 100, cursor: { column: 0, row: 0, shape: 'block', blinking: false } }))
    view.scrollTo(50)
    runFrames()

    expect(painter.scenes.at(-1)?.cursor).toBeUndefined()
  })
})
