// A selection: where it is, which the view knows; and what it means, which it asks.
import { describe, expect, it } from 'vitest'
import { Selection } from '../src/selection.ts'
import type { Position, Span } from '../src/source.ts'
import { FakeSource } from './_source.ts'

const at = (line: number, column: number): Position => ({ line, column })

function selection(source = new FakeSource()) {
  let changes = 0
  const made = new Selection(source, () => 80, () => { changes += 1 })
  return { made, source, changes: () => changes }
}

describe('a selection dragged across cells', () => {
  it('runs from where it started to where it is, whichever way it was dragged', () => {
    const { made } = selection()
    made.begin(at(5, 10), 'cell')
    made.extendTo(at(7, 3))
    expect(made.span).toEqual({ start: at(5, 10), end: at(7, 3) })

    made.extendTo(at(2, 40))
    expect(made.span, 'dragging back past the start did not turn it around').toEqual({ start: at(2, 40), end: at(5, 10) })
  })

  it('covers part of its first and last lines and the whole of those between', () => {
    const { made } = selection()
    made.begin(at(5, 10), 'cell')
    made.extendTo(at(7, 3))

    expect(made.on(4)).toBeUndefined()
    expect(made.on(5)).toEqual({ from: 10, to: 80 })
    expect(made.on(6)).toEqual({ from: 0, to: 80 })
    expect(made.on(7), 'the cell under the pointer is selected too').toEqual({ from: 0, to: 4 })
    expect(made.on(8)).toBeUndefined()
  })

  it('is drawn the moment it moves, without waiting on anything', () => {
    const { made, changes } = selection()
    made.begin(at(1, 1), 'cell')
    made.extendTo(at(1, 9))
    expect(changes()).toBe(2)
  })
})

describe('a selection by word or line', () => {
  /** A source for which every word is the five cells from a multiple of ten */
  class Words extends FakeSource {
    override async wordAt(position: Position): Promise<Span | null> {
      const start = Math.floor(position.column / 10) * 10
      return { start: at(position.line, start), end: at(position.line, start + 4) }
    }
  }

  it('grows each end to the word it is in, as the terminal says where words end', async () => {
    const { made } = selection(new Words())
    made.begin(at(3, 12), 'word')
    await Promise.resolve()
    expect(made.span).toEqual({ start: at(3, 10), end: at(3, 14) })

    made.extendTo(at(3, 33))
    await Promise.resolve()
    expect(made.span, 'dragging did not take whole words').toEqual({ start: at(3, 10), end: at(3, 34) })
  })

  it('ignores an answer that arrives after the selection has moved on', async () => {
    const { made } = selection(new Words())
    made.begin(at(3, 12), 'word')
    made.clear()
    await Promise.resolve()

    expect(made.span, 'a cleared selection came back').toBeNull()
  })
})

describe('the selected text', () => {
  it('is asked of the source, which knows what was written', async () => {
    class Text extends FakeSource {
      asked: Span | null = null
      override async textOf(span: Span): Promise<string> { this.asked = span; return 'the text' }
    }
    const source = new Text()
    const { made } = selection(source)
    made.begin(at(1, 0), 'cell')
    made.extendTo(at(2, 5))

    expect(await made.text()).toBe('the text')
    expect(source.asked).toEqual({ start: at(1, 0), end: at(2, 5) })
  })

  it('is empty with nothing selected', async () => {
    expect(await selection().made.text()).toBe('')
  })
})
