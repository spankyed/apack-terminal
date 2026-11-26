/**
 * Bounded content-addressed LRU image cache for v2 Kitty/Sixel placements.
 *
 * - Fetches compressed blobs (`png` or `zstdRgba`) over `TerminalSource.getImageBlob` (pipelined
 *   in 64 KiB chunks by `BrokerSessionSource`).
 * - Decodes validated single-frame PNGs off the JS main thread via `createImageBitmap(new Blob(...))`
 *   with zero main-thread RGBA copy loops.
 * - Enforces a hard client-side byte cap (`default 64 MiB`) and closes evicted `ImageBitmap`s.
 */
import { blobHashToHex, type ImageBlobData, type TerminalSource } from './source.ts'

export interface DecodedImage {
  readonly key: string
  readonly source: CanvasImageSource
  readonly width: number
  readonly height: number
  readonly byteSize: number
}

export interface ImageCacheOptions {
  /** Maximum decoded image bytes retained in memory (default 64 MiB). */
  readonly maxBytes?: number
  /** Optional custom PNG/Zstd-RGBA decoder (useful for tests or custom zstd WASM decoders). */
  readonly decode?: (blob: ImageBlobData) => Promise<{ readonly source: CanvasImageSource; readonly width: number; readonly height: number }>
}

const DEFAULT_MAX_BYTES = 64 * 1024 * 1024

async function defaultDecodeBlob(
  blob: ImageBlobData,
): Promise<{ readonly source: CanvasImageSource; readonly width: number; readonly height: number }> {
  if (blob.encoding !== 'png') {
    throw new Error(`Image encoding '${blob.encoding}' requires a custom decode hook`)
  }
  if (typeof createImageBitmap !== 'function') {
    throw new Error('createImageBitmap is unavailable in this environment')
  }
  const pngBytes = new Uint8Array(blob.bytes.length)
  pngBytes.set(blob.bytes)
  const bitmap = await createImageBitmap(new Blob([pngBytes], { type: 'image/png' }))
  return { source: bitmap, width: bitmap.width, height: bitmap.height }
}

export class ImageCache {
  private readonly entries = new Map<string, DecodedImage>()
  private readonly inflight = new Map<string, Promise<DecodedImage | null>>()
  private readonly maxBytes: number
  private readonly decode: NonNullable<ImageCacheOptions['decode']>
  private usedBytes = 0
  private disposed = false

  constructor(options: ImageCacheOptions = {}) {
    this.maxBytes = Math.max(1024, options.maxBytes ?? DEFAULT_MAX_BYTES)
    this.decode = options.decode ?? defaultDecodeBlob
  }

  get bytesUsed(): number {
    return this.usedBytes
  }

  get size(): number {
    return this.entries.size
  }

  /** Synchronously returns a cached `DecodedImage` (bumping its LRU recency), or `undefined`. */
  get(blob: Uint8Array): DecodedImage | undefined {
    const key = blobHashToHex(blob)
    const hit = this.entries.get(key)
    if (!hit) return undefined
    this.entries.delete(key)
    this.entries.set(key, hit)
    return hit
  }

  /** Requests & decodes `blob` once (deduplicating concurrent callers), calling `onReady` when loaded. */
  ensure(source: TerminalSource, blob: Uint8Array, onReady?: () => void): Promise<DecodedImage | null> {
    if (this.disposed) return Promise.resolve(null)
    const key = blobHashToHex(blob)
    const cached = this.entries.get(key)
    if (cached) return Promise.resolve(cached)

    const existing = this.inflight.get(key)
    if (existing) {
      if (onReady) void existing.then((img) => { if (img && !this.disposed) onReady() })
      return existing
    }

    const promise = (async (): Promise<DecodedImage | null> => {
      try {
        const data = await source.getImageBlob(blob)
        if (this.disposed) return null
        const decoded = await this.decode(data)
        if (this.disposed) {
          closeIfBitmap(decoded.source)
          return null
        }
        const byteSize = Math.max(data.bytes.length, decoded.width * decoded.height * 4)
        const entry: DecodedImage = {
          key,
          source: decoded.source,
          width: decoded.width,
          height: decoded.height,
          byteSize,
        }
        this.insert(entry)
        onReady?.()
        return entry
      } catch {
        return null
      } finally {
        this.inflight.delete(key)
      }
    })()

    this.inflight.set(key, promise)
    return promise
  }

  private insert(entry: DecodedImage): void {
    const prev = this.entries.get(entry.key)
    if (prev) {
      this.usedBytes -= prev.byteSize
      closeIfBitmap(prev.source)
      this.entries.delete(entry.key)
    }
    this.entries.set(entry.key, entry)
    this.usedBytes += entry.byteSize
    while (this.usedBytes > this.maxBytes && this.entries.size > 1) {
      const oldestKey = this.entries.keys().next().value
      if (oldestKey === undefined) break
      const oldest = this.entries.get(oldestKey)!
      this.entries.delete(oldestKey)
      this.usedBytes -= oldest.byteSize
      closeIfBitmap(oldest.source)
    }
  }

  dispose(): void {
    this.disposed = true
    for (const entry of this.entries.values()) {
      closeIfBitmap(entry.source)
    }
    this.entries.clear()
    this.inflight.clear()
    this.usedBytes = 0
  }
}

function closeIfBitmap(source: CanvasImageSource): void {
  if (typeof ImageBitmap !== 'undefined' && source instanceof ImageBitmap) {
    source.close()
  }
}
