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

  it('takes folds that do not overlap, and refuses ones that do', () => {
    const view = mountedView({}, new FakeSource())
    expect(() => view.view.setFolds([{ first: 1, last: 3 }, { first: 3, last: 5 }])).toThrow(RangeError)
    expect(() => view.view.setFolds([{ first: 1, last: 3 }, { first: 4, last: 6 }])).not.toThrow()
  })
})
