// Which lines a view shows: the screen while following, and a line that stays put while scrolled back.
import { describe, expect, it } from 'vitest'
import { RowStore } from '../src/row-store.ts'
import { Viewport } from '../src/viewport.ts'
import { frame } from './_support.ts'

/** A store whose screen starts at `firstActive` with that much history, and a viewport over it */
function scrolledTo(firstActive: number, scrollback = firstActive) {
  const store = new RowStore()
  store.apply(frame({ firstActive, scrollback }))
  return { store, viewport: new Viewport(store) }
}

describe('a view that is following', () => {
  it('shows the screen, and moves with it', () => {
    const { store, viewport } = scrolledTo(100)
    expect(viewport.following).toBe(true)
    expect(viewport.visible).toEqual([100, 4])

    store.apply(frame({ sequence: 2, firstActive: 130, scrollback: 130 }))
    expect(viewport.top, 'new output did not move a following view').toBe(130)
  })
})

describe('a view scrolled back', () => {
  it('goes on showing the same lines while output arrives', () => {
    const { store, viewport } = scrolledTo(100)
    viewport.scrollBy(-12)
    expect(viewport.following).toBe(false)
    expect(viewport.top).toBe(88)

    // A thousand more lines are written
    store.apply(frame({ sequence: 2, firstActive: 1100, scrollback: 1100 }))

    expect(viewport.top, 'output moved a view that was scrolled back').toBe(88)
    expect(viewport.following).toBe(false)
  })

  it('shows the oldest line there is once the one it held is gone', () => {
    const { store, viewport } = scrolledTo(100)
    viewport.scrollTo(10)

    // The owner keeps only fifty lines of history now
    store.apply(frame({ sequence: 2, firstActive: 200, scrollback: 50 }))

    expect(viewport.top).toBe(150)
  })

  it('follows again on reaching the screen, however it got there', () => {
    const { viewport } = scrolledTo(100)
    viewport.scrollBy(-5)
    viewport.scrollBy(5)
    expect(viewport.following, 'scrolling down to the screen did not resume following').toBe(true)

    viewport.scrollBy(-5)
    viewport.follow()
    expect(viewport.top).toBe(100)
  })

  it('cannot go above the oldest line', () => {
    const { viewport } = scrolledTo(100, 20)
    viewport.scrollBy(-500)
    expect(viewport.top).toBe(80)
  })

  it('part-way between two lines needs one row more, and says how far it is shifted', () => {
    const { viewport } = scrolledTo(100)
    viewport.scrollTo(90.25)

    expect(viewport.visible).toEqual([90, 5])
    expect(viewport.offset).toBeCloseTo(0.25)
  })
})

describe('a screen with no history', () => {
  it('is always followed: there is nowhere to scroll', () => {
    const { store, viewport } = scrolledTo(100)
    viewport.scrollBy(-10)

    // A full-screen program takes over, which has no scrollback
    store.apply(frame({ sequence: 2, firstActive: 100, scrollback: 0, modes: { alternateScreen: true, mouse: false } }))
    viewport.settle()

    expect(viewport.following).toBe(true)
    expect(viewport.top).toBe(100)
  })

  it('goes on being followed when history starts again, after the history a view was reading was erased', () => {
    const { store, viewport } = scrolledTo(100)
    viewport.scrollBy(-10)

    // The history is cleared while the view is scrolled back in it
    store.apply(frame({ sequence: 2, firstActive: 100, scrollback: 0 }))
    viewport.settle()
    expect(viewport.following).toBe(true)

    // And then one line scrolls off the screen
    store.apply(frame({ sequence: 3, firstActive: 101, scrollback: 1 }))
    viewport.settle()

    expect(viewport.following, 'a view with nothing to hold stopped following at the first new line').toBe(true)
    expect(viewport.top).toBe(101)
  })
})

describe('what a scrollbar is drawn from', () => {
  it('is how much there is, where the view is, and how much it shows', () => {
    const { viewport } = scrolledTo(100, 60)
    viewport.scrollTo(70)

    expect(viewport.extent).toEqual({ total: 64, position: 30, shown: 4 })
  })
})

describe('line numbers changing under a view', () => {
  it('leaves it the same distance from the screen', () => {
    const { store, viewport } = scrolledTo(100)
    viewport.scrollTo(80)

    const after = viewport.acrossReset()
    // A resize reflowed the history: everything is renumbered
    store.apply(frame({ sequence: 2, epoch: 2, firstActive: 240, scrollback: 240 }))
    after()

    expect(viewport.top, 'the view kept a number that now names another line').toBe(220)
  })

  it('leaves a following view following', () => {
    const { store, viewport } = scrolledTo(100)
    const after = viewport.acrossReset()
    store.apply(frame({ sequence: 2, epoch: 2, firstActive: 240, scrollback: 240 }))
    after()

    expect(viewport.following).toBe(true)
  })
})
