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
const MAX_FAILED_PER_SOURCE = 1024

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
  const bitmap = await createImageBitmap(new Blob([pngBytes], { type: 'image/png' }), {
    premultiplyAlpha: 'premultiply',
  })
  return { source: bitmap, width: bitmap.width, height: bitmap.height }
}

export class ImageCache {
  private readonly entries = new Map<string, DecodedImage>()
  private inflight = new WeakMap<TerminalSource, Map<string, Promise<DecodedImage | null>>>()
  private inflightCallbacks = new WeakMap<TerminalSource, Map<string, Set<() => void>>>()
  private failed = new WeakMap<TerminalSource, Set<string>>()
  private readonly evictListeners = new Set<(key: string) => void>()
  private readonly maxBytes: number
  private readonly decode: NonNullable<ImageCacheOptions['decode']>
  private usedBytes = 0
  private disposed = false

  constructor(options: ImageCacheOptions = {}) {
    this.maxBytes = Math.max(1024, options.maxBytes ?? DEFAULT_MAX_BYTES)
    this.decode = options.decode ?? defaultDecodeBlob
  }

  /** Subscribes to key evictions, replacements, and cache disposal so painters can free GPU textures. */
  onEvict(listener: (key: string) => void): () => void {
    this.evictListeners.add(listener)
    return () => {
      this.evictListeners.delete(listener)
    }
  }

  private notifyEvict(key: string): void {
    for (const listener of [...this.evictListeners]) {
      try {
        listener(key)
      } catch {
        // Isolate listener failures so sibling views still release GPU textures and ImageBitmaps always close.
      }
    }
  }

  private inflightFor(source: TerminalSource): Map<string, Promise<DecodedImage | null>> {
    let map = this.inflight.get(source)
    if (!map) {
      map = new Map()
      this.inflight.set(source, map)
    }
    return map
  }

  private callbacksFor(source: TerminalSource): Map<string, Set<() => void>> {
    let map = this.inflightCallbacks.get(source)
    if (!map) {
      map = new Map()
      this.inflightCallbacks.set(source, map)
    }
    return map
  }

  private recordFailed(source: TerminalSource, key: string): void {
    if (this.disposed) return
    let set = this.failed.get(source)
    if (!set) {
      set = new Set()
      this.failed.set(source, set)
    }
    set.delete(key)
    set.add(key)
    while (set.size > MAX_FAILED_PER_SOURCE) {
      const oldest = set.values().next().value
      if (oldest === undefined) break
      set.delete(oldest)
    }
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

  /** Requests & decodes `blob` once per source (deduplicating concurrent callers), calling `onReady` when loaded. */
  ensure(source: TerminalSource, blob: Uint8Array, onReady?: () => void): Promise<DecodedImage | null> {
    if (this.disposed) return Promise.resolve(null)
    const key = blobHashToHex(blob)
    const cached = this.entries.get(key)
    if (cached) return Promise.resolve(cached)
    if (this.failed.get(source)?.has(key)) return Promise.resolve(null)

    const sourceInflight = this.inflightFor(source)
    const sourceCallbacks = this.callbacksFor(source)
    const existing = sourceInflight.get(key)
    if (existing) {
      if (onReady) sourceCallbacks.get(key)?.add(onReady)
      return existing
    }

    const callbacks = new Set<() => void>()
    if (onReady) callbacks.add(onReady)
    sourceCallbacks.set(key, callbacks)

    let settled = false
    const promise = (async (): Promise<DecodedImage | null> => {
      let entry: DecodedImage | null = null
      let waiting: Set<() => void> | undefined
      try {
        const data = await source.getImageBlob(blob)
        if (this.disposed) return null
        const decoded = await this.decode(data)
        if (this.disposed) {
          closeIfBitmap(decoded.source)
          return null
        }
        const validDims =
          Number.isFinite(decoded.width)
          && Number.isFinite(decoded.height)
          && decoded.width > 0
          && decoded.height > 0
        const pixelBytes = validDims ? decoded.width * decoded.height * 4 : Number.NaN
        if (!validDims || !Number.isFinite(pixelBytes) || pixelBytes > Number.MAX_SAFE_INTEGER) {
          closeIfBitmap(decoded.source)
          this.recordFailed(source, key)
          return null
        }
        const existingEntry = this.entries.get(key)
        if (existingEntry) {
          closeIfBitmap(decoded.source)
          entry = existingEntry
        } else {
          const byteSize = Math.max(data.bytes.length, pixelBytes)
          entry = {
            key,
            source: decoded.source,
            width: decoded.width,
            height: decoded.height,
            byteSize,
          }
          this.insert(entry)
        }
        waiting = sourceCallbacks.get(key)
      } catch {
        this.recordFailed(source, key)
        return null
      } finally {
        settled = true
        sourceInflight.delete(key)
        sourceCallbacks.delete(key)
      }

      if (waiting) {
        for (const cb of [...waiting]) {
          try {
            cb()
          } catch {
            // Isolate callback failures so a decoded entry stays cached and sibling callers still run.
          }
        }
      }
      return entry
    })()

    if (!settled) sourceInflight.set(key, promise)
    return promise
  }

  private insert(entry: DecodedImage): void {
    const prev = this.entries.get(entry.key)
    if (prev) {
      this.usedBytes -= prev.byteSize
      this.entries.delete(entry.key)
      this.notifyEvict(entry.key)
      closeIfBitmap(prev.source)
    }
    this.entries.set(entry.key, entry)
    this.usedBytes += entry.byteSize
    while (this.usedBytes > this.maxBytes && this.entries.size > 1) {
      const oldestKey = this.entries.keys().next().value
      if (oldestKey === undefined) break
      const oldest = this.entries.get(oldestKey)!
      this.entries.delete(oldestKey)
      this.usedBytes -= oldest.byteSize
      this.notifyEvict(oldestKey)
      closeIfBitmap(oldest.source)
    }
  }

  dispose(): void {
    this.disposed = true
    const keys = [...this.entries.keys()]
    const sources = [...this.entries.values()].map((e) => e.source)
    this.entries.clear()
    this.inflight = new WeakMap()
    this.inflightCallbacks = new WeakMap()
    this.failed = new WeakMap()
    this.usedBytes = 0
    for (const key of keys) this.notifyEvict(key)
    for (const source of sources) closeIfBitmap(source)
    this.evictListeners.clear()
  }
}

function closeIfBitmap(source: CanvasImageSource): void {
  if (typeof source === 'object' && source !== null && 'close' in source && typeof source.close === 'function') {
    source.close()
  }
}
