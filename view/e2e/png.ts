import { inflateSync } from 'node:zlib'

export interface DecodedPng {
  readonly width: number
  readonly height: number
  /** Tightly packed RGBA bytes (`width * height * 4`). */
  readonly rgba: Uint8Array
}

export interface PixelRect {
  readonly x: number
  readonly y: number
  readonly w: number
  readonly h: number
}

const PNG_SIG = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10])

function paethPredictor(a: number, b: number, c: number): number {
  const p = a + b - c
  const pa = Math.abs(p - a)
  const pb = Math.abs(p - b)
  const pc = Math.abs(p - c)
  if (pa <= pb && pa <= pc) return a
  if (pb <= pc) return b
  return c
}

/**
 * Decodes an 8-bit RGB or RGBA PNG buffer (such as Playwright's `locator.screenshot()`) into
 * tightly packed RGBA bytes using Node's built-in `zlib.inflateSync`.
 */
export function decodePng(buf: Buffer): DecodedPng {
  if (buf.length < 33 || !buf.subarray(0, 8).equals(PNG_SIG)) {
    throw new Error('Invalid PNG signature')
  }
  let width = 0
  let height = 0
  let bitDepth = 0
  let colorType = 0
  const idatParts: Buffer[] = []

  let offset = 8
  while (offset + 12 <= buf.length) {
    const length = buf.readUInt32BE(offset)
    const type = buf.subarray(offset + 4, offset + 8).toString('ascii')
    const dataStart = offset + 8
    const dataEnd = dataStart + length
    if (dataEnd + 4 > buf.length) {
      throw new Error(`Truncated PNG chunk ${type}`)
    }
    const chunk = buf.subarray(dataStart, dataEnd)
    if (type === 'IHDR') {
      width = chunk.readUInt32BE(0)
      height = chunk.readUInt32BE(4)
      bitDepth = chunk[8] ?? 0
      colorType = chunk[9] ?? 0
      const interlace = chunk[12] ?? 0
      if (bitDepth !== 8 || (colorType !== 2 && colorType !== 6) || interlace !== 0) {
        throw new Error(`Unsupported PNG format: bitDepth=${bitDepth} colorType=${colorType} interlace=${interlace}`)
      }
    } else if (type === 'IDAT') {
      idatParts.push(chunk)
    } else if (type === 'IEND') {
      break
    }
    offset = dataEnd + 4
  }

  if (width <= 0 || height <= 0 || idatParts.length === 0) {
    throw new Error('PNG missing IHDR or IDAT')
  }

  const bpp = colorType === 6 ? 4 : 3
  const stride = width * bpp
  const raw = inflateSync(Buffer.concat(idatParts))
  const expected = height * (1 + stride)
  if (raw.length < expected) {
    throw new Error(`Inflated PNG too short: got ${raw.length}, expected ${expected}`)
  }

  const unfilt = new Uint8Array(height * stride)
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (1 + stride)] ?? 0
    const rowIn = y * (1 + stride) + 1
    const rowOut = y * stride
    const prevOut = (y - 1) * stride
    for (let x = 0; x < stride; x++) {
      const byte = raw[rowIn + x] ?? 0
      const left = x >= bpp ? (unfilt[rowOut + x - bpp] ?? 0) : 0
      const up = y > 0 ? (unfilt[prevOut + x] ?? 0) : 0
      const upLeft = y > 0 && x >= bpp ? (unfilt[prevOut + x - bpp] ?? 0) : 0
      let val = byte
      switch (filter) {
        case 0:
          val = byte
          break
        case 1:
          val = (byte + left) & 0xff
          break
        case 2:
          val = (byte + up) & 0xff
          break
        case 3:
          val = (byte + ((left + up) >> 1)) & 0xff
          break
        case 4:
          val = (byte + paethPredictor(left, up, upLeft)) & 0xff
          break
        default:
          throw new Error(`Unsupported PNG filter ${filter}`)
      }
      unfilt[rowOut + x] = val
    }
  }

  if (bpp === 4) {
    return { width, height, rgba: unfilt }
  }

  const rgba = new Uint8Array(width * height * 4)
  for (let i = 0, j = 0; i < unfilt.length; i += 3, j += 4) {
    rgba[j] = unfilt[i] ?? 0
    rgba[j + 1] = unfilt[i + 1] ?? 0
    rgba[j + 2] = unfilt[i + 2] ?? 0
    rgba[j + 3] = 255
  }
  return { width, height, rgba }
}

/** Counts pixels inside `rect` (clamped to image bounds) satisfying `predicate(r, g, b, a)`. */
export function countMatchingPixels(
  png: DecodedPng,
  rect: PixelRect,
  predicate: (r: number, g: number, b: number, a: number) => boolean,
): number {
  const x0 = Math.max(0, Math.floor(rect.x))
  const y0 = Math.max(0, Math.floor(rect.y))
  const x1 = Math.min(png.width, Math.ceil(rect.x + rect.w))
  const y1 = Math.min(png.height, Math.ceil(rect.y + rect.h))
  let count = 0
  for (let y = y0; y < y1; y++) {
    for (let x = x0; x < x1; x++) {
      const idx = (y * png.width + x) * 4
      const r = png.rgba[idx] ?? 0
      const g = png.rgba[idx + 1] ?? 0
      const b = png.rgba[idx + 2] ?? 0
      const a = png.rgba[idx + 3] ?? 0
      if (predicate(r, g, b, a)) count++
    }
  }
  return count
}

/** Returns the count of pixels differing from `(bgR, bgG, bgB)` by more than `tolerance` on any channel. */
export function countNonBackgroundPixels(
  png: DecodedPng,
  rect: PixelRect,
  bg: readonly [number, number, number],
  tolerance = 14,
): number {
  return countMatchingPixels(
    png,
    rect,
    (r, g, b) =>
      Math.abs(r - bg[0]) > tolerance || Math.abs(g - bg[1]) > tolerance || Math.abs(b - bg[2]) > tolerance,
  )
}
