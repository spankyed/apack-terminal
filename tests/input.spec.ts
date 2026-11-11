// The keyboard, as a view hears it: keys sent as what the keyboard said, text that is not a key sent as
// text, and the keys that belong to the app or the input method left alone.
import { beforeEach, describe, expect, it } from 'vitest'
import { TooLarge, type PasteOutcome } from '../src/source.ts'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'

beforeEach(() => installFrames())

describe('a key', () => {
  it('is sent as the physical key, the modifiers and what it typed', () => {
    const { source, key } = mountedView()
    key('keydown', { code: 'KeyA', key: 'a' })
    key('keydown', { code: 'KeyA', key: 'A', shiftKey: true })
    key('keydown', { code: 'ArrowUp', key: 'ArrowUp' })
    key('keydown', { code: 'KeyC', key: 'c', ctrlKey: true })
    key('keydown', { code: 'KeyB', key: '∫', altKey: true })
    expect(source.keys).toEqual([
      { action: 'press', code: 'KeyA', modifiers: 0, text: 'a', unshifted: 97, composed: false },
      { action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshifted: 97, composed: false },
      { action: 'press', code: 'ArrowUp', modifiers: 0, text: '', unshifted: 0, composed: false },
      // With control held the key types nothing: what it sends is the source's to work out
      { action: 'press', code: 'KeyC', modifiers: 2, text: '', unshifted: 99, composed: false },
      // A Mac's option key typed that character: alt is still said to be down, and so is what it was for
      { action: 'press', code: 'KeyB', modifiers: 4, text: '∫', unshifted: 98, composed: true },
    ])
  })

  it('held down is a repeat, and let go is a release that types nothing', () => {
    const { source, key } = mountedView()
    key('keydown', { code: 'KeyJ', key: 'j', repeat: true })
    key('keyup', { code: 'KeyJ', key: 'j' })
    expect(source.keys.map((sent) => [sent.action, sent.text])).toEqual([['repeat', 'j'], ['release', '']])
  })

  it('typed with the key for a third level is its character, though Windows reports control and alt with it', () => {
    const { source, key } = mountedView({ mac: false })
    // A German keyboard's `@`: AltGr and Q
    key('keydown', { code: 'KeyQ', key: '@', ctrlKey: true, altKey: true, modifierAltGraph: true })
    key('keydown', { code: 'Digit7', key: '{', ctrlKey: true, altKey: true, shiftKey: true, modifierAltGraph: true })
    // The same two keys held by hand type nothing: they are a shortcut
    key('keydown', { code: 'KeyQ', key: 'q', ctrlKey: true, altKey: true })
    expect(source.keys).toEqual([
      { action: 'press', code: 'KeyQ', modifiers: 0, text: '@', unshifted: 113, composed: false },
      { action: 'press', code: 'Digit7', modifiers: 1, text: '{', unshifted: 55, composed: false },
      { action: 'press', code: 'KeyQ', modifiers: 6, text: '', unshifted: 113, composed: false },
    ])
  })

  it('typed with a Mac\'s option key says so, unless the character is the one the key types anyway', () => {
    const { source, key } = mountedView({ mac: true })
    key('keydown', { code: 'KeyA', key: 'å', altKey: true })
    key('keydown', { code: 'KeyA', key: 'Å', altKey: true, shiftKey: true })
    // Option changed nothing about what these type, so it was held with them, not used to type them
    key('keydown', { code: 'KeyA', key: 'a', altKey: true })
    key('keydown', { code: 'KeyA', key: 'A', altKey: true, shiftKey: true })
    key('keydown', { code: 'ArrowLeft', key: 'ArrowLeft', altKey: true })
    key('keyup', { code: 'KeyA', key: 'å', altKey: true })
    expect(source.keys.map((sent) => [sent.action, sent.text, sent.modifiers, sent.composed])).toEqual([
      ['press', 'å', 4, true], ['press', 'Å', 5, true], ['press', 'a', 4, false], ['press', 'A', 5, false],
      ['press', '', 4, false], ['release', '', 4, false],
    ])
  })

  it('typed with alt anywhere else is a key held with alt, whatever it typed', () => {
    const { source, key } = mountedView({ mac: false })
    key('keydown', { code: 'KeyA', key: 'å', altKey: true })
    expect(source.keys).toEqual([{ action: 'press', code: 'KeyA', modifiers: 4, text: 'å', unshifted: 97, composed: false }])
  })

  it('held with an option key that is set to be alt is sent as what the key types without it', () => {
    const { source, key } = mountedView({ mac: true, optionIsAlt: true })
    key('keydown', { code: 'KeyA', key: 'å', altKey: true })
    key('keydown', { code: 'KeyA', key: 'Å', altKey: true, shiftKey: true })
    // What shift makes of a key that is not a letter is the layout's to say, so the source works it out from the key
    key('keydown', { code: 'Digit1', key: '⁄', altKey: true, shiftKey: true })
    key('keydown', { code: 'ArrowLeft', key: 'ArrowLeft', altKey: true })
    expect(source.keys.map((sent) => [sent.code, sent.text, sent.modifiers, sent.composed])).toEqual([
      ['KeyA', 'a', 4, false], ['KeyA', 'A', 5, false], ['Digit1', '', 5, false], ['ArrowLeft', '', 4, false],
    ])

    // Option and E would start an accent: with option as alt it starts nothing, and is alt and `e`
    const accent = key('keydown', { code: 'KeyE', key: 'Dead', altKey: true })
    expect(source.keys.at(-1)).toEqual({ action: 'press', code: 'KeyE', modifiers: 4, text: 'e', unshifted: 101, composed: false })
    expect(accent.defaultPrevented, 'the accent was left to begin').toBe(true)
  })

  it('never has a control character for its text: what such a key sends is the source\'s to work out', () => {
    const { source, key } = mountedView({ mac: false })
    key('keydown', { code: 'Backspace', key: '\u007f' })
    key('keydown', { code: 'KeyH', key: '\b', ctrlKey: true })
    key('keydown', { code: 'Enter', key: '\r' })
    expect(source.keys.map((sent) => [sent.code, sent.text])).toEqual([['Backspace', ''], ['KeyH', ''], ['Enter', '']])
  })

  it('let go is not told to a program that was never told it went down', () => {
    const { source, key } = mountedView({ mac: true, onKey: (event) => event.code === 'KeyK' })
    // One the host kept for itself
    key('keydown', { code: 'KeyK', key: 'k' })
    key('keyup', { code: 'KeyK', key: 'k' })
    // And one that was the app's when it went down, let go after the command key was
    key('keydown', { code: 'KeyT', key: 't', metaKey: true })
    key('keyup', { code: 'KeyT', key: 't' })
    expect(source.keys).toEqual([])

    key('keydown', { code: 'KeyJ', key: 'j' })
    key('keyup', { code: 'KeyJ', key: 'j' })
    key('keyup', { code: 'KeyJ', key: 'j' })
    expect(source.keys.map((sent) => [sent.action, sent.code])).toEqual([['press', 'KeyJ'], ['release', 'KeyJ']])
  })

  it('does not also arrive as text typed into the field', () => {
    const { key } = mountedView()
    expect(key('keydown', { code: 'KeyA', key: 'a' }).defaultPrevented).toBe(true)
  })

  it('with the command key held is the app\'s on a Mac, and the program never hears it', () => {
    const { source, key } = mountedView({ mac: true })
    const pressed = key('keydown', { code: 'KeyT', key: 't', metaKey: true })
    expect(source.keys).toEqual([])
    expect(pressed.defaultPrevented, 'the app\'s own shortcut was swallowed').toBe(false)
  })

  it('the host wants for itself is kept from the program', () => {
    const { source, key } = mountedView({ onKey: (event) => event.code === 'KeyF' && event.ctrlKey })
    expect(key('keydown', { code: 'KeyF', key: 'f', ctrlKey: true }).defaultPrevented).toBe(true)
    key('keydown', { code: 'KeyG', key: 'g', ctrlKey: true })
    expect(source.keys.map((sent) => sent.code)).toEqual(['KeyG'])
  })

  it('that only starts an accent is the input method\'s', () => {
    const { source, key } = mountedView()
    const dead = key('keydown', { code: 'Quote', key: 'Dead' })
    expect(source.keys).toEqual([])
    expect(dead.defaultPrevented).toBe(false)
  })
})

describe('text that is not a key', () => {
  it('composed by an input method is sent once, when it is finished, and no key during it is', () => {
    const { source, field, key } = mountedView()
    field.dispatchEvent(new CompositionEvent('compositionstart'))
    key('keydown', { code: 'KeyN', key: 'n' })
    field.value = 'に'
    field.dispatchEvent(new Event('input'))
    field.dispatchEvent(new CompositionEvent('compositionend', { data: '日本' }))
    expect(source.keys).toEqual([])
    expect(source.texts).toEqual(['日本'])
    expect(field.value).toBe('')
  })

  it('put in the field by something else — a picker, dictation — is sent as it is', () => {
    const { source, field } = mountedView()
    field.value = '👍🏽'
    field.dispatchEvent(new Event('input'))
    expect(source.texts).toEqual(['👍🏽'])
    expect(field.value).toBe('')
  })
})

describe('the field an input method composes in', () => {
  it('is where the cursor is drawn, clear of the gutter, in the terminal\'s font and colours', () => {
    const { source, field } = mountedView({ gutter: 6 })
    source.send(frame({ rows: [row(1, 'abc')], cursor: { column: 2, row: 1, shape: 'block', blinking: false } }))
    runFrames()

    // Cells are ten by twenty: two columns in, past six pixels of gutter, one row down
    expect([field.style.left, field.style.top, field.style.height, field.style.lineHeight]).toEqual(['26px', '20px', '20px', '20px'])
    expect(field.style.fontFamily).toBe('monospace')
    expect(field.style.fontSize).toBe('14px')
    expect(field.style.color).toBe('rgb(212, 212, 212)')
    expect(field.style.background).toBe('rgb(30, 30, 30)')
  })
})

describe('typing while scrolled back', () => {
  /** A view over a hundred lines of history, scrolled fifty lines up */
  function scrolledBack(options: Parameters<typeof mountedView>[0] = {}) {
    const view = mountedView(options)
    view.source.send(frame({ firstActive: 100, scrollback: 100, rows: [row(100, '$ ')] }))
    view.view.scrollTo(50)
    runFrames()
    return view
  }

  it('brings the view back to the screen, where what was typed appears', () => {
    const typed = scrolledBack()
    typed.key('keydown', { code: 'KeyA', key: 'a' })
    expect(typed.view.viewport.following, 'a key').toBe(true)

    const composed = scrolledBack()
    composed.field.dispatchEvent(new CompositionEvent('compositionstart'))
    composed.field.dispatchEvent(new CompositionEvent('compositionend', { data: '日本' }))
    expect(composed.view.viewport.following, 'composed text').toBe(true)
  })

  it('does so for a paste once it has gone, and not for one that was held back', async () => {
    const held = scrolledBack()
    held.source.pasteOutcome = 'unsafe'
    await held.view.paste('rm -rf x\n')
    expect(held.view.viewport.following, 'a paste nobody agreed to moved the view').toBe(false)

    const pasted = scrolledBack()
    await pasted.view.paste('ls')
    expect(pasted.view.viewport.following).toBe(true)
  })

  it('leaves the view where it is for a key that only changes what another means, or was not the program\'s', () => {
    const view = scrolledBack({ onKey: (event) => event.code === 'F5' })
    view.key('keydown', { code: 'ShiftLeft', key: 'Shift', shiftKey: true })
    view.key('keydown', { code: 'F5', key: 'F5' })
    view.key('keyup', { code: 'ShiftLeft', key: 'Shift' })
    expect(view.view.viewport.following).toBe(false)
    expect(view.source.keys.map((sent) => sent.code), 'the modifier itself is still the program\'s to hear').toEqual(['ShiftLeft', 'ShiftLeft'])
  })

  it('leaves the view where it is when the host says so', () => {
    const view = scrolledBack({ followOnInput: false })
    view.key('keydown', { code: 'KeyA', key: 'a' })
    expect(view.view.viewport.following).toBe(false)
    expect(view.source.keys).toHaveLength(1)
  })
})

function pasteOf(text: string): ClipboardEvent {
  const event = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent
  Object.defineProperty(event, 'clipboardData', { value: { getData: () => text } })
  return event
}

describe('a paste', () => {
  it('goes to the source as text, which decides whether it is safe', async () => {
    const { source, field } = mountedView()
    const event = pasteOf('ls -la')
    field.dispatchEvent(event)
    await settled()
    expect(event.defaultPrevented).toBe(true)
    expect(source.pastes).toEqual([{ text: 'ls -la', allowUnsafe: false }])
  })

  it('that could run a command is dropped when nobody can be asked', async () => {
    const { source, field } = mountedView()
    source.pasteOutcome = 'unsafe'
    field.dispatchEvent(pasteOf('rm -rf x\n'))
    await settled()
    expect(source.pastes).toEqual([{ text: 'rm -rf x\n', allowUnsafe: false }])
  })

  it('that could run a command is sent again only once the person agrees to it', async () => {
    const asked: string[] = []
    let agree = false
    const { source, view } = mountedView({ confirmPaste: async (text) => { asked.push(text); return agree } })
    source.pasteOutcome = 'unsafe'

    await view.paste('rm -rf x\n')
    expect(asked).toEqual(['rm -rf x\n'])
    expect(source.pastes).toHaveLength(1)

    agree = true
    await view.paste('rm -rf x\n')
    expect(source.pastes.at(-1)).toEqual({ text: 'rm -rf x\n', allowUnsafe: true })
  })

  it('too large for the source to carry is refused whole, and the host is told', async () => {
    class Small extends FakeSource {
      override async paste(text: string): Promise<PasteOutcome> {
        if (text.length > 10) throw new TooLarge('a message was longer than a message may be')
        return 'pasted'
      }
    }
    const refused: Error[] = []
    const { view, field } = mountedView({ onPasteRefused: (error) => { refused.push(error) } }, new Small())

    await expect(view.paste('far more than ten characters')).rejects.toBeInstanceOf(TooLarge)
    await expect(view.paste('short')).resolves.toBeUndefined()

    // The person's own paste has nobody waiting on it, so the host hears instead
    field.dispatchEvent(pasteOf('far more than ten characters'))
    await settled()
    expect(refused).toHaveLength(1)
    expect(refused[0]).toBeInstanceOf(TooLarge)
  })

  it('with the control and shift keys is left to the page where there is no command key', () => {
    const { source, key } = mountedView({ mac: false })
    const pressed = key('keydown', { code: 'KeyV', key: 'V', ctrlKey: true, shiftKey: true })
    expect(source.keys).toEqual([])
    expect(pressed.defaultPrevented, 'the paste that follows was stopped').toBe(false)
  })
})

describe('focus', () => {
  it('is told to the source when the field gains and loses it, once each', () => {
    const { source, field } = mountedView()
    field.dispatchEvent(new FocusEvent('focus'))
    field.dispatchEvent(new FocusEvent('focus'))
    field.dispatchEvent(new FocusEvent('blur'))
    expect(source.focus_).toEqual([true, false])
  })
})
