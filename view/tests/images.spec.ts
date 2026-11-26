import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { ImageCache, blobHashToHex } from '../src/index.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'
import { installFrames } from './_frames.ts'

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
    cache.dispose()
    expect(cache.size).toBe(0)
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
      { blob: fgBlob, line: 1, col: 2, pixelSize: [40, 20], cellSpan: [4, 1], zIndex: 5 },
      { blob: bgBlob, line: 0, col: 0, pixelSize: [20, 20], cellSpan: [2, 1], zIndex: -2 },
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

    view.dispose()
    cache.dispose()
  })
})
