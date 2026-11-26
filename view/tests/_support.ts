import { xtermPalette } from '../src/palette.ts'
import {
  RowFlag,
  RunFlag,
  packRow,
  type FrameUpdate,
  type ImagePiece,
  type PackedRow,
  type StyleSpec,
} from '../src/source.ts'
import { cellsOfCodePoint } from '../src/width.ts'

/** A palette whose sixteen themed colours are each recognisable: entry `n` is `0xA000n0`. */
export const PALETTE = xtermPalette(Array.from({ length: 16 }, (_, index) => 0xa00000 | (index << 4)))

/** Encodes a 24-bit RGB colour (`0xRRGGBB`) into a v2 `PackedStyle` colour word. */
export function packedRgb(rgb: number): number {
  return (0x01_00_00_00 | (rgb & 0x00ff_ffff)) >>> 0
}

/** Encodes a 0..=255 palette index into a v2 `PackedStyle` colour word. */
export function packedPalette(index: number): number {
  return (index + 1) >>> 0
}

/**
 * Builds a v2 `PackedRow` from plain text (handles ASCII runs and wide/non-ASCII clusters cleanly).
 */
export function row(
  line: number,
  text: string,
  options: {
    readonly style?: number
    readonly wrapped?: boolean
    readonly prompt?: boolean
    readonly hasBlink?: boolean
    readonly styles?: readonly StyleSpec[]
    readonly links?: readonly string[]
    readonly linkIdx?: number
    readonly pieces?: readonly ImagePiece[]
  } = {},
): PackedRow {
  const styles: StyleSpec[] = [{}, ...(options.styles ?? [])]
  const styleIdx = options.style ?? 0
  const linkIdx = options.linkIdx ?? 0
  const flags =
    (options.wrapped ? RowFlag.WRAPPED : 0)
    | (options.prompt ? RowFlag.PROMPT : 0)
    | (options.hasBlink ? RowFlag.HAS_BLINK : 0)
    | (options.pieces && options.pieces.length > 0 ? RowFlag.HAS_PLACEHOLDER : 0)

  if (text.length === 0) {
    return packRow({ line, flags, runs: [], styles, links: options.links ?? [], pieces: options.pieces })
  }

  let isAscii = true
  for (let i = 0; i < text.length; i += 1) {
    const c = text.charCodeAt(i)
    if (c < 0x20 || c > 0x7e) {
      isAscii = false
      break
    }
  }

  if (isAscii) {
    return packRow({
      line,
      flags,
      runs: [{ colStart: 0, colLen: text.length, text, styleIdx, linkIdx, flags: 0 }],
      styles,
      links: options.links ?? [],
      pieces: options.pieces,
    })
  }

  const chars = Array.from(text)
  let col = 0
  const runs = chars.map((ch) => {
    const w = Math.max(1, cellsOfCodePoint(ch.codePointAt(0) ?? 0))
    const perCell = w > 1 || ch.length > 1 || ch.charCodeAt(0) >= 0x80 ? RunFlag.PER_CELL : 0
    const run = { colStart: col, colLen: w, text: ch, styleIdx, linkIdx, flags: perCell }
    col += w
    return run
  })
  return packRow({
    line,
    flags,
    runs,
    styles,
    links: options.links ?? [],
    pieces: options.pieces,
  })
}

/** Builds a v2 `FrameUpdate` over a 4-row, 20-column screen unless overridden. */
export function frame(parts: Partial<FrameUpdate> = {}): FrameUpdate {
  return {
    seq: 1,
    epoch: 1,
    size: { cols: 20, rows: 4 },
    firstActiveLine: 0,
    scrollbackLines: 0,
    scrollDelta: 0,
    cursor: { col: 0, row: 0, shape: 'block', blinking: false, hidden: true },
    modes: { altScreen: false, mouse: false, reverseVideo: false, bracketedPaste: false },
    palette: null,
    rows: [],
    imagesGen: 0,
    fullSync: false,
    ...parts,
  }
}
