// Folded output: a fold is one header row where its lines start, and its other lines are neither drawn nor pointed at. What
// is copied from it is still all of its text, and the cursor and the rows under the pointer follow what is drawn.
import { beforeEach, describe, expect, it } from 'vitest'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'

beforeEach(() => installFrames())

/** The text of each row the view draws, read where a screen reader reads it */
const drawnText = (view: ReturnType<typeof mountedView>) =>
  [...view.view.element.querySelector('[data-terminal-text]')!.children].map((child) => child.textContent)

/**
 * Seven lines: the screen is lines 3 to 6 and lines 0 to 2 are history, shown by scrolling back to the top. The cursor, when
 * given, is at a row of the screen.
 */
function sevenLines(setup: ReturnType<typeof mountedView>, cursor: { row: number; column: number } | null = null) {
  setup.source.send(frame({
    screenRows: 4, columns: 20, firstActive: 3, scrollback: 3, cursor: cursor ? { ...cursor, shape: 'block' } as never : null,
    rows: [row(0, 'L0'), row(1, 'L1'), row(2, 'L2'), row(3, 'L3'), row(4, 'L4'), row(5, 'L5'), row(6, 'L6')],
  }))
  runFrames()
  setup.view.viewport.scrollTo(0)
  // Scrolling does not paint: setting the folds, which are none yet, does
  setup.view.setFolds([])
  runFrames()
}

describe('a folded run of lines', () => {
  it('is one header row where it starts, and its other lines are not drawn', () => {
    const view = mountedView({}, new FakeSource())
    sevenLines(view)
    expect(drawnText(view)).toEqual(['L0', 'L1', 'L2', 'L3'])
    view.view.setFolds([{ first: 1, last: 3, label: 'output' }])
    runFrames()
    expect(drawnText(view)).toEqual(['L0', '▸ output', 'L4', 'L5'])
  })

  it('is a header with its count when it has no label of its own', () => {
    const view = mountedView({}, new FakeSource())
    sevenLines(view)
    view.view.setFolds([{ first: 1, last: 3 }])
    runFrames()
    expect(drawnText(view)[1]).toBe('▸ 2 lines folded')
  })

  it('moves the rows under the pointer: the row a header is on is its first line, and the rows below it are the lines after it', async () => {
    const source = new FakeSource()
    const view = mountedView({}, source)
    sevenLines(view)
    view.view.setFolds([{ first: 1, last: 3 }])
    runFrames()
    view.point('pointerdown', 0, 2)
    view.point('pointermove', 3, 2)
    view.point('pointerup', 3, 2)
    expect(view.view.selection.span?.start.line, 'the row under the pointer is the fold\'s line after the header').toBe(4)
  })

  it('keeps the cursor on the line it is on, at the row it is drawn in', () => {
    const view = mountedView({}, new FakeSource())
    // The cursor is on the screen's third row, which is line 5
    sevenLines(view, { row: 2, column: 0 })
    view.view.setFolds([{ first: 1, last: 3 }])
    runFrames()
    const cursor = view.painted().cursor
    expect(cursor?.row, 'line 5 is the fourth row once lines 2 and 3 are folded away').toBe(3)
  })

  it('is copied as all of its text, though it is drawn as its header', async () => {
    class Printed extends FakeSource {
      readonly asked: number[][] = []
      override async textOf(span: { start: { line: number }; end: { line: number } }): Promise<string> {
        this.asked.push([span.start.line, span.end.line])
        return 'the whole output'
      }
    }
    const source = new Printed()
    const copied: string[] = []
    const view = mountedView({ onCopy: (text) => { copied.push(text) } }, source)
    sevenLines(view)
    view.view.setFolds([{ first: 1, last: 3 }])
    runFrames()
    expect(view.view.selectOutput({ id: 1, command: 'make', cwd: null, startedMs: null, finishedMs: null, outcome: { kind: 'exited', code: 0 }, prompt: 0, output: 1, end: 4 } as never)).toBe(true)
    await settled()
    expect(await view.view.copy()).toBe(true)
    expect(source.asked).toEqual([[1, 3]])
    expect(copied).toEqual(['the whole output'])
  })

  it('survives scrolling: a view scrolled to a line folded away starts at the fold\'s header, and no hidden line is drawn', () => {
    const view = mountedView({}, new FakeSource())
    sevenLines(view)
    view.view.setFolds([{ first: 1, last: 3, label: 'output' }])
    runFrames()
    view.view.viewport.scrollTo(2)
    view.view.setFolds([{ first: 1, last: 3, label: 'output' }])
    runFrames()
    expect(drawnText(view)).toEqual(['▸ output', 'L4', 'L5', 'L6'])
  })

  it('survives a resize of the screen: the fold still hides its lines in the rows there are', () => {
    const view = mountedView({}, new FakeSource())
    sevenLines(view)
    view.view.setFolds([{ first: 1, last: 3, label: 'output' }])
    runFrames()
    view.source.send(frame({ sequence: 2, screenRows: 3, columns: 20, firstActive: 3, scrollback: 3, rows: [row(0, 'L0'), row(1, 'L1'), row(2, 'L2'), row(3, 'L3'), row(4, 'L4'), row(5, 'L5'), row(6, 'L6')] }))
    view.view.viewport.scrollTo(0)
    view.view.setFolds([{ first: 1, last: 3, label: 'output' }])
    runFrames()
    expect(drawnText(view)).toEqual(['L0', '▸ output', 'L4'])
  })

  it('is headed by a label whose wide characters take two cells, so what follows lines up with them', () => {
    const view = mountedView({}, new FakeSource())
    sevenLines(view)
    view.view.setFolds([{ first: 1, last: 3, label: '中x' }])
    runFrames()
    // The header is "▸ 中x": the wide character is on cells 2 and 3, so the x is on cell 4
    const header = view.painted().rows[1]!
    expect(header.glyphs.map((glyph) => [glyph.text, glyph.column])).toEqual([['▸', 0], ['中', 2], ['x', 4]])
  })

  it('places the input at the fold header when the cursor sits on a line the fold hides', () => {
    const view = mountedView({}, new FakeSource())
    // The cursor is on line 5, which a fold over lines 4 to 6 hides. The view is shown from line 0, so the header, line 4,
    // is the fifth row; rows are twenty pixels high in the test painter. Placed on line 5 it would be a row lower still.
    sevenLines(view, { row: 2, column: 0 })
    view.view.setFolds([{ first: 4, last: 6 }])
    runFrames()
    expect(view.field.style.top).toBe('80px')
  })

  describe('scrolling across a fold', () => {
    /** Thirty lines of history over a four-row screen, with lines 10 to 24 folded to the header on line 9 */
    function thirtyLines() {
      const view = mountedView({}, new FakeSource())
      view.source.send(frame({ screenRows: 4, columns: 20, firstActive: 30, scrollback: 30, rows: Array.from({ length: 34 }, (_, line) => row(line, `L${line}`)) }))
      runFrames()
      view.view.setFolds([{ first: 9, last: 24, label: 'output' }])
      runFrames()
      return view
    }

    it('moves one drawn row a notch: a fold is one row, so no notch is spent turning over its hidden lines', () => {
      const view = thirtyLines()
      const tops: number[] = []
      for (let notch = 0; notch < 15; notch += 1) {
        view.view.scrollBy(-1)
        runFrames()
        tops.push(view.view.viewport.top)
        if (notch === 5) expect(drawnText(view)[0], 'the notch that reaches the fold shows its header').toBe('▸ output')
      }
      expect(tops).toEqual([29, 28, 27, 26, 25, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0])
      view.view.scrollBy(-1)
      expect(view.view.viewport.top, 'the oldest line is where it stops').toBe(0)
    })

    it('drags the scrollbar through every drawn row, the fold\'s header among them and none of its hidden lines', () => {
      const view = thirtyLines()
      Object.defineProperty(view.view.element, 'clientHeight', { get: () => 80, configurable: true })
      view.view.fit()
      view.view.scrollTo(0)
      view.painted()
      const thumb = view.view.element.lastElementChild as HTMLElement
      thumb.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true, cancelable: true, clientY: 40 }))
      const visited: Array<number | 'following'> = []
      // Every pixel of the track, top to bottom: each row is reached and none is skipped
      for (let clientY = 40; clientY <= 100; clientY += 1) {
        window.dispatchEvent(new MouseEvent('pointermove', { clientY }))
        const top = view.view.viewport.top
        // The track is 60 pixels for 15 drawn rows, so each four pixels is one row of the scrollbar's own measure
        if ((clientY - 40) % 4 === 0) expect(view.view.viewport.extent.position, `${(clientY - 40) / 4} rows down the track`).toBe((clientY - 40) / 4)
        const shown = view.view.viewport.following ? 'following' : Math.floor(top)
        if (visited.at(-1) !== shown) visited.push(shown)
      }
      expect(visited).toEqual([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 25, 26, 27, 28, 29, 'following'])
    })
  })

  it('takes folds that do not overlap, and refuses ones that do', () => {
    const view = mountedView({}, new FakeSource())
    expect(() => view.view.setFolds([{ first: 1, last: 3 }, { first: 3, last: 5 }])).toThrow(RangeError)
    expect(() => view.view.setFolds([{ first: 1, last: 3 }, { first: 4, last: 6 }])).not.toThrow()
  })
})
