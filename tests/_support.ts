import { PLAIN, type Frame, type Row, type Style } from '../src/source.ts'

/** A row of plain ASCII, one character a cell, all in one style */
export function row(line: number, text: string, options: { style?: number; wrapped?: boolean; prompt?: boolean } = {}): Row {
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
  }
}

/** A frame over a 4-row, 20-column screen unless told otherwise, at epoch 1 */
export function frame(parts: Partial<Frame> & { rows?: Row[]; styles?: Style[] }): Frame {
  return {
    sequence: 1,
    epoch: 1,
    firstActive: 0,
    columns: 20,
    screenRows: 4,
    scrollback: 0,
    cursor: null,
    modes: { alternateScreen: false, mouse: false },
    styles: [PLAIN],
    links: [''],
    rows: [],
    ...parts,
  }
}
