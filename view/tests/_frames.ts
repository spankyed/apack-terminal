/**
 * Animation frames a case runs by hand, and a size observer that never fires.
 *
 * jsdom has neither, and a view's whole rule is *when* it draws: so a case says when a frame happens and
 * counts how many were asked for.
 */
let queued: Array<FrameRequestCallback | null> = []

export function installFrames(): void {
  queued = []
  globalThis.requestAnimationFrame = ((callback: FrameRequestCallback) => queued.push(callback)) as typeof requestAnimationFrame
  globalThis.cancelAnimationFrame = ((handle: number) => { queued[handle - 1] = null }) as typeof cancelAnimationFrame
  globalThis.ResizeObserver = class { observe() {} unobserve() {} disconnect() {} } as unknown as typeof ResizeObserver
}

/** How many frames are waiting to run */
export const framesWaiting = (): number => queued.filter(Boolean).length

/** Runs every frame asked for so far, at `time` on the page's clock, answering how many ran */
export function runFrames(time = 0): number {
  const running = queued
  queued = []
  let ran = 0
  for (const callback of running) {
    if (callback) { callback(time); ran += 1 }
  }
  return ran
}
