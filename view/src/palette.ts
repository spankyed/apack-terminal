/**
 * The 256 colours a terminal's numbered colours name, given the sixteen a theme chooses.
 *
 * The first sixteen are the theme's. The rest are the same in every terminal: a 6×6×6 cube of colours and
 * a ramp of twenty-four greys, which no theme redefines.
 */
export function xtermPalette(ansi: readonly number[]): number[] {
  const palette = ansi.slice(0, 16)
  while (palette.length < 16) palette.push(0)
  const level = (step: number) => (step === 0 ? 0 : 55 + step * 40)
  for (let red = 0; red < 6; red += 1) {
    for (let green = 0; green < 6; green += 1) {
      for (let blue = 0; blue < 6; blue += 1) palette.push((level(red) << 16) | (level(green) << 8) | level(blue))
    }
  }
  for (let step = 0; step < 24; step += 1) {
    const grey = 8 + step * 10
    palette.push((grey << 16) | (grey << 8) | grey)
  }
  return palette
}
