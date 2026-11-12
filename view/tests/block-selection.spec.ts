// A rectangle selected by dragging with Alt (Option on a Mac): each line is the columns the rectangle has on it, the text is
// each line's own columns joined by newlines, and a drag that is not made with Alt is still the text between its ends.
import { beforeEach, describe, expect, it } from 'vitest'
import type { Span } from '../src/source.ts'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'

beforeEach(() => installFrames())

/** Each span answered by what it names, so a case reads which lines and columns were asked for */
class Named extends FakeSource {
  readonly asked: Span[] = []
  override async textOf(span: Span): Promise<string> {
    this.asked.push(span)
    return `L${span.start.line}:${span.start.column}-${span.end.column}`
  }
}

function showing(setup: ReturnType<typeof mountedView>) {
  setup.source.send(frame({ columns: 20, screenRows: 4, rows: [row(0, 'alpha beta gamma'), row(1, 'delta epsilon'), row(2, 'zeta eta theta'), row(3, 'iota kappa')] }))
  runFrames()
}

describe('a block selected with Alt', () => {
  it('covers the same columns on every line from its first to its last, and nothing beyond them', () => {
    const view = mountedView({}, new Named())
    showing(view)
    view.point('pointerdown', 1, 0, { altKey: true })
    view.point('pointermove', 3, 2, { altKey: true })
    view.point('pointerup', 3, 2, { altKey: true })
    const selection = view.view.selection
    expect(selection.on(0)).toEqual({ from: 1, to: 4 })
    expect(selection.on(1)).toEqual({ from: 1, to: 4 })
    expect(selection.on(2)).toEqual({ from: 1, to: 4 })
    expect(selection.on(3), 'a line below the rectangle is not selected').toBeUndefined()
  })

  it('is the same rectangle dragged up and to the left as down and to the right', () => {
    const view = mountedView({}, new Named())
    showing(view)
    view.point('pointerdown', 3, 2, { altKey: true })
    view.point('pointermove', 1, 0, { altKey: true })
    view.point('pointerup', 1, 0, { altKey: true })
    expect(view.view.selection.on(0)).toEqual({ from: 1, to: 4 })
    expect(view.view.selection.on(2)).toEqual({ from: 1, to: 4 })
  })

  it('is copied as each line\'s own columns, joined by newlines', async () => {
    const source = new Named()
    const copied: string[] = []
    const view = mountedView({ onCopy: (text) => { copied.push(text) } }, source)
    showing(view)
    view.point('pointerdown', 1, 0, { altKey: true })
    view.point('pointermove', 3, 2, { altKey: true })
    view.point('pointerup', 3, 2, { altKey: true })
    await settled()
    expect(await view.view.copy()).toBe(true)
    expect(copied).toEqual(['L0:1-3\nL1:1-3\nL2:1-3'])
    expect(source.asked.map((span) => span.start.line)).toEqual([0, 1, 2])
  })

  it('is not made by a drag without Alt, which selects the text between its ends', () => {
    const view = mountedView({}, new Named())
    showing(view)
    view.point('pointerdown', 1, 0)
    view.point('pointermove', 3, 2)
    view.point('pointerup', 3, 2)
    expect(view.view.selection.block).toBeNull()
    expect(view.view.selection.on(1), 'a middle line runs to its edge').toEqual({ from: 0, to: 20 })
  })
})
