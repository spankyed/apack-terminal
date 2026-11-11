import { xtermPalette } from '../src/palette.ts'
import { PLAIN, type Frame, type Row, type Style } from '../src/source.ts'

/** A palette whose sixteen themed colours are each recognisable: entry `n` is `0xA000n0` */
export const PALETTE = xtermPalette(Array.from({ length: 16 }, (_, index) => 0xa00000 | (index << 4)))

/**
 * A row of plain ASCII, one character a cell, all in one style: the `style`th of `styles`, which has the
 * plain one put in front of it as every row's does.
 */
export function row(line: number, text: string, options: { style?: number; wrapped?: boolean; prompt?: boolean; styles?: Style[]; links?: string[] } = {}): Row {
  const characters = [...text]
  return {
    line,
    wrapped: options.wrapped ?? false,
    prompt: options.prompt ?? false,
    text: characters,
    column: Uint16Array.from(characters, (_, index) => index),
    width: Uint8Array.from(characters, () => 1),
    style: Uint16Array.from(characters, () => options.style ?? 0),
    link: new Uint16Array(characters.length),
    styles: [PLAIN, ...(options.styles ?? [])],
    links: ['', ...(options.links ?? [])],
  }
}

/** A frame over a 4-row, 20-column screen unless told otherwise, at epoch 1 */
export function frame(parts: Partial<Frame> & { rows?: Row[] }): Frame {
  return {
    sequence: 1,
    epoch: 1,
    firstActive: 0,
    columns: 20,
    screenRows: 4,
    scrollback: 0,
    commands: 0,
    images: 0,
    cursor: null,
    modes: { alternateScreen: false, mouse: false },
    reverseVideo: false,
    rows: [],
    ...parts,
  }
}
