import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { ImageCache, blobHashToHex } from '../src/index.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'
import { framesWaiting, installFrames } from './_frames.ts'

function fakeBitmap(width: number, height: number) {
  let closed = false
  return {
    width,
    height,
    get closed() {
      return closed
    },
    close() {
      closed = true
    },
  }
}

describe('ImageCache and TerminalView image placements', () => {
  beforeEach(() => {
    installFrames()
  })
  afterEach(() => {
    document.body.innerHTML = ''
  })

  it('deduplicates concurrent fetches and evicts least-recently-used entries under byte cap', async () => {
    const source = new FakeSource()
    const b1 = new Uint8Array(32).fill(1)
    const b2 = new Uint8Array(32).fill(2)
    const b3 = new Uint8Array(32).fill(3)
    for (const b of [b1, b2, b3]) {
      source.blobs.set(blobHashToHex(b), { blob: b, encoding: 'png', bytes: new Uint8Array([1, 2, 3]) })
    }

    const bitmaps: Array<ReturnType<typeof fakeBitmap>> = []
    // Each 16x16 RGBA image = 1024 bytes; cap at 2048 bytes => holds at most 2 images
    const cache = new ImageCache({
      maxBytes: 2048,
      decode: async () => {
        const bm = fakeBitmap(16, 16)
        bitmaps.push(bm)
        return { source: bm as unknown as CanvasImageSource, width: 16, height: 16 }
      },
    })

    // Deduplicate concurrent ensure() calls for b1
    const [r1a, r1b] = await Promise.all([cache.ensure(source, b1), cache.ensure(source, b1)])
    expect(r1a).toBe(r1b)
    expect(source.blobsAsked).toHaveLength(1)

    await cache.ensure(source, b2)
    // Touch b1 so b2 becomes least-recently-used
    expect(cache.get(b1)).toBeDefined()
    await cache.ensure(source, b3)

    expect(cache.size).toBe(2)
    expect(cache.bytesUsed).toBeLessThanOrEqual(2048)
    expect(cache.get(b1)).toBeDefined()
    expect(cache.get(b3)).toBeDefined()
    expect(cache.get(b2)).toBeUndefined()
    // Evicted bitmap must be closed!
    expect(bitmaps[1]!.closed).toBe(true)
    cache.dispose()
    expect(cache.size).toBe(0)
    expect(bitmaps[0]!.closed).toBe(true)
    expect(bitmaps[2]!.closed).toBe(true)
  })

  it('fetches placements on imagesGen bump and renders both placements and placeholder pieces in z-order', async () => {
    const source = new FakeSource()
    const bgBlob = new Uint8Array(32).fill(0x10)
    const fgBlob = new Uint8Array(32).fill(0x20)
    const pieceBlob = new Uint8Array(32).fill(0x30)
    for (const b of [bgBlob, fgBlob, pieceBlob]) {
      source.blobs.set(blobHashToHex(b), { blob: b, encoding: 'png', bytes: new Uint8Array([9]) })
    }
    source.imagesGen = 1
    source.imagePlacements = [
      // Large natural pixelSize (2000x1500) placed in 4x1 cells -> drawn at 4*10=40 by 1*20=20 CSS px!
      {
        blob: fgBlob,
        line: 1,
        col: 2,
        sourceRect: [0, 0, 2000, 1500],
        cellOffset: [0, 0],
        pixelSize: [2000, 1500],
        cellSpan: [4, 1],
        zIndex: 5,
      },
      {
        blob: bgBlob,
        line: 0,
        col: 0,
        sourceRect: [0, 0, 800, 600],
        cellOffset: [0, 0],
        pixelSize: [800, 600],
        cellSpan: [2, 1],
        zIndex: -2,
      },
    ]

    const pics = new Map<string, CanvasImageSource>()
    const cache = new ImageCache({
      decode: async (data) => {
        const pic = document.createElement('canvas')
        pic.width = 40
        pic.height = 20
        pics.set(blobHashToHex(data.blob), pic)
        return { source: pic, width: 40, height: 20 }
      },
    })

    const { view, painted } = mountedView({ imageCache: cache }, source)
    source.send(
      frame({
        seq: 1,
        imagesGen: 1,
        rows: [
          row(0, 'top', {
            pieces: [{ blob: pieceBlob, colStart: 1, colLen: 2, srcRect: [0, 0, 20, 20], dstPx: [20, 20] }],
          }),
          row(1, 'mid'),
        ],
      }),
    )
    painted()
    await settled()
    await settled()
    const scene = painted()

    expect(source.imagesAsked).toBe(1)
    expect(scene.images?.length).toBe(3)
    // Sorted by z: bg (-2) -> piece (-1) -> fg (+5)
    expect(scene.images![0]!.picture).toBe(pics.get(blobHashToHex(bgBlob)))
    expect(scene.images![0]!.above).toBe(false)
    expect(scene.images![1]!.picture).toBe(pics.get(blobHashToHex(pieceBlob)))
    expect(scene.images![1]!.above).toBe(false)
    expect(scene.images![2]!.picture).toBe(pics.get(blobHashToHex(fgBlob)))
    expect(scene.images![2]!.above).toBe(true)
    expect(scene.images![2]!.width).toBe(40)
    expect(scene.images![2]!.height).toBe(20)

    view.dispose()
    cache.dispose()
  })

  it('notifies painter.releaseImage on ImageCache eviction, honors anchored sourceRect/cellOffset/pixelSize, and reschedules doubted askPage fetches', async () => {
    const source = new FakeSource()
    const b1 = new Uint8Array(32).fill(0x51)
    const b2 = new Uint8Array(32).fill(0x52)
    const b3 = new Uint8Array(32).fill(0x53)
    for (const b of [b1, b2, b3]) {
      source.blobs.set(blobHashToHex(b), { blob: b, encoding: 'png', bytes: new Uint8Array([1, 2, 3]) })
    }
    source.imagesGen = 1
    source.imagePlacements = [
      {
        blob: b1,
        line: 0,
        col: 1,
        sourceRect: [4, 2, 12, 10],
        cellOffset: [3, 5],
        pixelSize: [12, 10],
        cellSpan: [3, 2],
        zIndex: 1,
      },
    ]

    // Cap at 2048 bytes (each 16x16 RGBA = 1024 bytes => inserting b3 evicts b1)
    const cache = new ImageCache({
      maxBytes: 2048,
      decode: async () => {
        const pic = document.createElement('canvas')
        pic.width = 16
        pic.height = 16
        return { source: pic, width: 16, height: 16 }
      },
    })
    const { view, painter, painted } = mountedView({ imageCache: cache }, source)
    source.send(frame({ seq: 1, imagesGen: 1, rows: [row(0, 'anchored'), row(1, 'below')] }))
    painted()
    await settled()
    await settled()
    const scene = painted()

    // H10 (view): anchored placement honors sourceRect, cellOffset, pixelSize (12x10, not stretched to 30x40), and key!
    expect(scene.images).toHaveLength(1)
    const img = scene.images![0]!
    expect(img.key).toBe(blobHashToHex(b1))
    expect(img.x).toBe(1 * 10 + 3)
    expect(img.y).toBe(0 * 20 + 5)
    expect(img.width).toBe(12)
    expect(img.height).toBe(10)
    expect(img.source).toEqual({ x: 4, y: 2, width: 12, height: 10 })

    // H10 edge cases: oversized cellOffset clamps inside the cell (max cellWidth-1 / cellHeight-1),
    // destination width/height never overflow spanCols*cellWidth - offX / spanRows*cellHeight - offY,
    // and NaN/zero-sized placements are safely skipped!
    source.imagesGen = 2
    source.imagePlacements = [
      {
        blob: b1,
        line: 0,
        col: 0,
        sourceRect: [0, 0, 16, 16],
        cellOffset: [999, -5],
        pixelSize: [500, 500],
        cellSpan: [2, 1],
        zIndex: 0,
      },
      {
        blob: b1,
        line: 0,
        col: 2,
        sourceRect: [Number.NaN, 0, 16, 16],
        cellOffset: [0, 0],
        pixelSize: [0, 0],
        cellSpan: [0, 0],
        zIndex: 0,
      },
    ]
    source.send(frame({ seq: 2, imagesGen: 2, rows: [row(0, 'anchored'), row(1, 'below')] }))
    painted()
    await settled()
    const clampedScene = painted()
    expect(clampedScene.images).toHaveLength(1)
    // cellWidth=10 -> offX clamped to 9, maxW = 2*10 - 9 = 11; cellHeight=20 -> offY clamped to 0, maxH = 1*20 - 0 = 20
    expect(clampedScene.images![0]!.x).toBe(9)
    expect(clampedScene.images![0]!.y).toBe(0)
    expect(clampedScene.images![0]!.width).toBe(11)
    expect(clampedScene.images![0]!.height).toBe(20)

    // H14: evicting b1 from ImageCache immediately calls painter.releaseImage(hex(b1))
    // even when another onEvict listener throws (error isolation prevents bitmap/texture leaks)!
    const unsubThrow = cache.onEvict(() => { throw new Error('boom') })
    await cache.ensure(source, b2)
    await cache.ensure(source, b3)
    unsubThrow()
    expect(painter.releasedImages).toEqual([blobHashToHex(b1)])

    // M13: when store.imagesChanged increments while fetchRows is in flight, askPage reschedules
    // so doubted placeholder rows are refetched automatically!
    let resolveFetch!: (rows: readonly ReturnType<typeof row>[]) => void
    let fetchCalls = 0
    const origFetch = source.fetchRows.bind(source)
    source.fetchRows = (epoch, from, count) => {
      fetchCalls += 1
      if (fetchCalls === 1) {
        return new Promise((res) => { resolveFetch = res })
      }
      return origFetch(epoch, from, count)
    }
    const pieceRow = row(0, 'stale-placeholder', {
      pieces: [{ blob: b2, colStart: 0, colLen: 2, srcRect: [0, 0, 16, 16], dstPx: [16, 16] }],
    })
    source.history.set(0, row(0, 'fresh-placeholder'))
    source.send(
      frame({
        seq: 3,
        epoch: 2,
        imagesGen: 2,
        firstActiveLine: 5,
        scrollbackLines: 5,
        size: { cols: 20, rows: 2 },
        rows: [row(5, 'bottom0'), row(6, 'bottom1')],
      }),
    )
    view.scrollTo(0)
    painted()
    expect(fetchCalls).toBe(1)
    // Bump imagesGen while fetchRows #1 is still in flight, then drain syncFrameImages + pending frames
    // BEFORE resolving fetch #1 so the ONLY trigger for refetch #2 is askPage's M13 reschedule!
    source.send(
      frame({
        seq: 4,
        epoch: 2,
        imagesGen: 3,
        firstActiveLine: 5,
        scrollbackLines: 5,
        size: { cols: 20, rows: 2 },
        rows: [row(5, 'bottom0'), row(6, 'bottom1')],
      }),
    )
    painted()
    await settled()
    painted()
    expect(fetchCalls).toBe(1)
    expect(framesWaiting()).toBe(0)

    // Complete stale fetch #1 returning a placeholder row (doubted/discarded by RowStore.absorb -> absorbed = [])
    resolveFetch([pieceRow])
    await settled()
    expect(framesWaiting()).toBeGreaterThan(0)
    painted()
    await settled()
    expect(fetchCalls).toBe(2)
    const refetchedScene = painted()
    expect(refetchedScene.rows[0]?.glyphs.map((g) => g.text).join('')).toBe('fresh-placeholder')

    view.dispose()
    cache.dispose()
  })

  it('isolates throwing onEvict listeners so dispose closes every ImageBitmap and notifies siblings', async () => {
    const bitmaps: Array<ReturnType<typeof fakeBitmap>> = []
    const cache = new ImageCache({
      maxBytes: 4096,
      decode: async () => {
        const bm = fakeBitmap(8, 8)
        bitmaps.push(bm)
        return { source: bm as unknown as CanvasImageSource, width: 8, height: 8 }
      },
    })
    const source = new FakeSource()
    const b1 = new Uint8Array(32).fill(0x51)
    const b2 = new Uint8Array(32).fill(0x52)
    source.blobs.set(blobHashToHex(b1), { blob: b1, encoding: 'png', bytes: new Uint8Array([1]) })
    source.blobs.set(blobHashToHex(b2), { blob: b2, encoding: 'png', bytes: new Uint8Array([2]) })
    await cache.ensure(source, b1)
    await cache.ensure(source, b2)

    const seen: string[] = []
    cache.onEvict(() => { throw new Error('listener-failure') })
    cache.onEvict((key) => seen.push(key))
    cache.dispose()

    expect(seen).toEqual([blobHashToHex(b1), blobHashToHex(b2)])
    expect(bitmaps.every((bm) => bm.closed)).toBe(true)
    expect(cache.size).toBe(0)
  })
})
