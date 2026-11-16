// The least contrast a character is drawn at against what it sits on. A program chooses its colours, and a dark grey
// on black or a blue on a blue background is unreadable for a person with low vision, or in a glare. The minimum is the
// WCAG contrast ratio: 1 leaves every colour as the program chose, 4.5 is the level WCAG asks for normal text.

/** A colour as the channels a view holds it in: 0xRRGGBB */
const channel = (color: number, shift: number): number => {
  const value = ((color >> shift) & 0xff) / 255
  return value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4
}

/** The relative luminance of a colour, from 0 for black to 1 for white (WCAG 2.x) */
export function luminance(color: number): number {
  return 0.2126 * channel(color, 16) + 0.7152 * channel(color, 8) + 0.0722 * channel(color, 0)
}

/** The contrast ratio of two colours, from 1 for none to 21 for black on white */
export function contrastRatio(a: number, b: number): number {
  const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x) as [number, number]
  return (light + 0.05) / (dark + 0.05)
}

/** Blends `color` toward `target` by `amount` from 0 (unchanged) to 1 (the target) */
function blend(color: number, target: number, amount: number): number {
  const mix = (shift: number) => Math.round(((color >> shift) & 0xff) * (1 - amount) + ((target >> shift) & 0xff) * amount)
  return (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/** The results for one minimum, keyed by the colour pair; a session holds one minimum at a time, so a few are plenty */
const caches = new Map<number, Map<number, number>>()
/** A bound on each cache: a program that paints many colours cannot grow it without limit */
const CACHE_LIMIT = 4096
const MINIMUMS_KEPT = 8

function cacheFor(minimum: number): Map<number, number> {
  let cache = caches.get(minimum)
  if (!cache) {
    if (caches.size >= MINIMUMS_KEPT) caches.clear()
    cache = new Map()
    caches.set(minimum, cache)
  }
  return cache
}

/**
 * The colour nearest `foreground` that is at least `minimum` away from `background`, moving it toward black or white,
 * whichever gives more contrast. A foreground that is already far enough is returned as it is. When even black or white
 * is not far enough, the one nearer to the minimum is.
 *
 * **Pass and fail are both cached.** A character that is already far enough is the common case, and deciding it costs
 * a luminance pair on every paint of every glyph, so the answer is kept as well as the adjusted colour.
 */
export function ensureContrast(foreground: number, background: number, minimum: number): number {
  if (minimum <= 1) return foreground
  const cache = cacheFor(minimum)
  // Both colours are 24 bits, so the pair is an exact number
  const key = foreground * 0x1000000 + background
  const known = cache.get(key)
  if (known !== undefined) return known

  let result = foreground
  if (contrastRatio(foreground, background) < minimum) {
    const target = contrastRatio(0xffffff, background) >= contrastRatio(0x000000, background) ? 0xffffff : 0x000000
    result = target
    if (contrastRatio(target, background) >= minimum) {
      // The smallest step toward the target that is far enough: contrast grows with the step, so a search finds it
      let near = 0
      let far = 1
      for (let step = 0; step < 16; step += 1) {
        const middle = (near + far) / 2
        if (contrastRatio(blend(foreground, target, middle), background) >= minimum) far = middle
        else near = middle
      }
      result = blend(foreground, target, far)
    }
  }

  if (cache.size >= CACHE_LIMIT) cache.clear()
  cache.set(key, result)
  return result
}
