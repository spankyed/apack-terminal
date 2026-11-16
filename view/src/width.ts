// How many cells a character takes. The painter takes each glyph's cells from the broker's rows, so this only serves text
// the view makes itself: a fold's header label, which a host has no row for. It covers the East Asian Wide and Fullwidth
// ranges that CJK text and emoji occupy, and every other character is one cell.

const WIDE: ReadonlyArray<readonly [number, number]> = [
  [0x1100, 0x115f], [0x2e80, 0x303e], [0x3041, 0x33ff], [0x3400, 0x4dbf], [0x4e00, 0x9fff], [0xa000, 0xa4cf],
  [0xac00, 0xd7a3], [0xf900, 0xfaff], [0xfe30, 0xfe4f], [0xff00, 0xff60], [0xffe0, 0xffe6],
  [0x1f300, 0x1f64f], [0x1f900, 0x1f9ff], [0x20000, 0x3fffd],
]

/** The cells a code point takes: two for a wide one, one otherwise */
export function cellsOfCodePoint(codePoint: number): number {
  return WIDE.some(([from, to]) => codePoint >= from && codePoint <= to) ? 2 : 1
}
