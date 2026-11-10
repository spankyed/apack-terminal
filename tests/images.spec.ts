// Images a program placed: asked for when a frame says they changed, their pixels fetched once each, and
// drawn where they are in the cells this view draws.
import { beforeEach, describe, expect, it } from 'vitest'
import { BrokerViews } from '../src/index.ts'
import { rgbaOf } from '../src/broker/source.ts'
import { CanvasPainter } from '../src/canvas-painter.ts'
import { drawRow } from '../src/draw.ts'
import { PLAIN, type Picture, type Placement } from '../src/source.ts'
import { FakeBroker } from './_broker.ts'
import { recordingContext } from './_canvas.ts'
import { installFrames } from './_frames.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled, theme } from './_view.ts'

beforeEach(installFrames)

const placement = (parts: Partial<Placement>): Placement => ({
  image: 1, generation: 1, z: 0, line: 0, column: 0, columns: 4, rows: 3, offsetX: 0, offsetY: 0, width: 40, height: 60,
  sourceX: 0, sourceY: 0, sourceWidth: 40, sourceHeight: 60, ...parts,
})
const pixels = (width: number, height: number): Picture => ({ generation: 1, width, height, pixels: new Uint8ClampedArray(width * height * 4) })

/** A view whose pictures are the pixels they were made from, so a case can say which one was drawn */
function viewing() {
  const made: Picture[] = []
  const setup = mountedView({ picture: async (read) => { made.push(read); return read as unknown as CanvasImageSource } })
  return { ...setup, made }
}

describe('the images of a session', () => {
  it('are asked for when a frame says they changed, and their pixels once for each picture', async () => {
    const view = viewing()
    view.source.placed = { cellWidth: 10, cellHeight: 20, placements: [placement({ image: 7, line: 1 }), placement({ image: 7, line: 9 })] }
    view.source.pictures.set(7, pixels(40, 60))
    view.source.send(frame({ images: 1 }))
    await settled()
    expect(view.source.imagesAsked).toBe(1)
    expect(view.source.picturesAsked, 'one picture placed twice is one picture').toEqual([7])

    view.source.send(frame({ sequence: 2, images: 1 }))
    await settled()
    expect(view.source.imagesAsked, 'asked again though nothing changed').toBe(1)

    // Placed somewhere else: where they are is asked again, the pixels this view holds are not
    view.source.placed = { cellWidth: 10, cellHeight: 20, placements: [placement({ image: 7, line: 2 })] }
    view.source.send(frame({ sequence: 3, images: 2 }))
    await settled()
    expect(view.source.imagesAsked).toBe(2)
    expect(view.source.picturesAsked).toEqual([7])

    // Replaced by another picture under the same number: its pixels are stale
    view.source.placed = { cellWidth: 10, cellHeight: 20, placements: [placement({ image: 7, line: 2, generation: 2 })] }
    view.source.send(frame({ sequence: 4, images: 3 }))
    await settled()
    expect(view.source.picturesAsked).toEqual([7, 7])
  })

  it('are drawn where they are, in this view\'s cells, the ones under the text first', async () => {
    const view = viewing()
    const under = pixels(40, 60)
    const over = pixels(8, 8)
    view.source.pictures.set(1, under)
    view.source.pictures.set(2, over)
    // The terminal was told a cell is five by ten; this view draws one at ten by twenty
    view.source.placed = {
      cellWidth: 5, cellHeight: 10,
      placements: [
        placement({ image: 2, z: 3, line: 2, column: 6, width: 8, height: 8, sourceWidth: 8, sourceHeight: 8, columns: 2, rows: 1 }),
        placement({ image: 1, z: -1, line: 1, column: 3, offsetX: 2, offsetY: 4, width: 20, height: 30, sourceX: 10, sourceY: 5, sourceWidth: 20, sourceHeight: 20 }),
      ],
    }
    view.source.send(frame({ images: 1, rows: [row(0, 'text')] }))
    await settled()
    await settled()
    expect(view.painted().images).toEqual([
      { picture: under, x: 34, y: 28, width: 40, height: 60, source: { x: 10, y: 5, width: 20, height: 20 }, above: false, z: -1 },
      { picture: over, x: 60, y: 40, width: 16, height: 16, source: { x: 0, y: 0, width: 8, height: 8 }, above: true, z: 3 },
    ])
  })

  it('are drawn only where some of them is in view, and from above the first row when they began there', async () => {
    const view = viewing()
    view.source.pictures.set(1, pixels(40, 60))
    view.source.placed = {
      cellWidth: 10, cellHeight: 20,
      placements: [placement({ line: 98, rows: 3 }), placement({ line: 90, rows: 3 }), placement({ line: 104, rows: 3 })],
    }
    view.source.send(frame({ firstActive: 100, scrollback: 100, images: 1 }))
    await settled()
    await settled()
    // The screen is lines 100 to 103: one image ends on line 100, one ended before it, one starts after
    expect(view.painted().images?.map((image) => image.y)).toEqual([-40])
  })

  it('lose their places when the screen is reflowed, and keep their pixels', async () => {
    const view = viewing()
    view.source.pictures.set(1, pixels(40, 60))
    view.source.placed = { cellWidth: 10, cellHeight: 20, placements: [placement({ line: 1 })] }
    view.source.send(frame({ images: 1 }))
    await settled()
    await settled()
    expect(view.painted().images).toHaveLength(1)

    view.source.placed = { cellWidth: 10, cellHeight: 20, placements: [placement({ line: 2 })] }
    view.source.send(frame({ sequence: 2, epoch: 2, images: 1 }))
    expect(view.painted().images, 'drawn on a line of the epoch before').toEqual([])
    await settled()
    expect(view.source.imagesAsked).toBe(2)
    expect(view.painted().images?.map((image) => image.y)).toEqual([40])
    expect(view.source.picturesAsked).toEqual([1])
  })
})

describe('an image on the canvas', () => {
  it('is drawn over the backgrounds and under the characters, or over both', () => {
    const { canvas, drawn } = recordingContext()
    const painter = new CanvasPainter({ family: 'monospace', size: 14 }, canvas)
    painter.resize(20, 4, 1, 6)
    const picture = {} as CanvasImageSource
    const source = { x: 1, y: 2, width: 3, height: 4 }
    painter.paint({
      theme,
      rows: [drawRow(row(0, 'ab'), [PLAIN], theme)],
      offset: 0.5,
      images: [
        { picture, x: 10, y: 20, width: 30, height: 40, source, above: false },
        { picture, x: 50, y: 0, width: 10, height: 10, source, above: true },
      ],
    })
    const kinds = drawn.map((one) => one.kind)
    const first = kinds.indexOf('image')
    const last = kinds.lastIndexOf('image')
    const text = kinds.indexOf('text')
    expect(first).toBeLessThan(text)
    expect(last).toBeGreaterThan(text)
    // Moved clear of the gutter with the rows, and up by the half row the view is scrolled
    expect(drawn[first]).toMatchObject({ x: 16, y: 10, width: 30, height: 40, source: [1, 2, 3, 4] })
    expect(drawn[last]).toMatchObject({ x: 56, y: -10, width: 10, height: 10 })
  })
})

describe('an image from a broker', () => {
  function attached() {
    const broker = new FakeBroker()
    const source = new BrokerViews(broker).open('s-1')
    source.attach({ frame: () => {}, ended: () => {} })
    broker.say({ kind: 'frame', channel: 1, frame: frame({ epoch: 6 }) })
    broker.taken()
    return { broker, source }
  }
  const part = { kind: 'image', channel: 1, image: 7, generation: 3, width: 2, height: 1, channels: 4, total: 8 } as const

  it('is where the broker says, unless the answer is from before a reflow', async () => {
    const { broker, source } = attached()
    const placed = [placement({ image: 7, line: 40 })]
    const asked = source.images()
    expect(broker.taken()).toEqual([{ kind: 'images', channel: 1, request: 1 }])
    broker.say({ kind: 'images', channel: 1, request: 1, epoch: 6, cellWidth: 9, cellHeight: 18, placements: placed })
    expect(await asked).toEqual({ cellWidth: 9, cellHeight: 18, placements: placed })

    const stale = source.images()
    broker.say(
      { kind: 'frame', channel: 1, frame: frame({ epoch: 7 }) },
      { kind: 'images', channel: 1, request: 2, epoch: 6, cellWidth: 9, cellHeight: 18, placements: placed },
    )
    expect((await stale).placements).toEqual([])
  })

  it('arrives in parts, each asked for from where the last ended', async () => {
    const { broker, source } = attached()
    const asked = source.image(7)
    expect(broker.taken()).toEqual([{ kind: 'image', channel: 1, request: 1, image: 7, offset: 0 }])
    broker.say({ ...part, request: 1, offset: 0, bytes: Uint8Array.of(1, 2, 3, 4, 5) })
    await settled()
    expect(broker.taken()).toEqual([{ kind: 'image', channel: 1, request: 2, image: 7, offset: 5 }])
    broker.say({ ...part, request: 2, offset: 5, bytes: Uint8Array.of(6, 7, 8) })
    expect(await asked).toEqual({ generation: 3, width: 2, height: 1, pixels: Uint8ClampedArray.of(1, 2, 3, 4, 5, 6, 7, 8) })
  })

  it('is read again from its start when the program replaced it between two parts', async () => {
    const { broker, source } = attached()
    const asked = source.image(7)
    broker.say({ ...part, request: 1, offset: 0, bytes: Uint8Array.of(1, 2, 3, 4, 5) })
    await settled()
    broker.say({ ...part, generation: 4, request: 2, offset: 5, bytes: Uint8Array.of(9, 9, 9) })
    await settled()
    expect(broker.taken().at(-1)).toEqual({ kind: 'image', channel: 1, request: 3, image: 7, offset: 0 })
    broker.say({ ...part, generation: 4, request: 3, offset: 0, bytes: Uint8Array.of(8, 7, 6, 5, 4, 3, 2, 1) })
    expect(await asked).toEqual({ generation: 4, width: 2, height: 1, pixels: Uint8ClampedArray.of(8, 7, 6, 5, 4, 3, 2, 1) })
  })

  it('is nothing when the broker no longer has it', async () => {
    const { broker, source } = attached()
    const asked = source.image(7)
    broker.say({ kind: 'failed', channel: 1, request: 1, message: 'no image 7' })
    expect(await asked).toBeNull()
  })
})

describe('pixels of fewer than four bytes', () => {
  it('become red, green, blue and alpha', () => {
    expect([...rgbaOf(1, Uint8Array.of(10, 200), 2)]).toEqual([10, 10, 10, 255, 200, 200, 200, 255])
    expect([...rgbaOf(2, Uint8Array.of(10, 128, 200, 0), 2)]).toEqual([10, 10, 10, 128, 200, 200, 200, 0])
    expect([...rgbaOf(3, Uint8Array.of(1, 2, 3, 4, 5, 6), 2)]).toEqual([1, 2, 3, 255, 4, 5, 6, 255])
    expect([...rgbaOf(4, Uint8Array.of(1, 2, 3, 4), 1)]).toEqual([1, 2, 3, 4])
  })
})
