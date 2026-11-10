// The keyboard, as a view hears it: keys sent as what the keyboard said, text that is not a key sent as
// text, and the keys that belong to the app or the input method left alone.
import { beforeEach, describe, expect, it } from 'vitest'
import { installFrames } from './_frames.ts'
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
      { action: 'press', code: 'KeyA', modifiers: 0, text: 'a', unshifted: 97 },
      { action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshifted: 97 },
      { action: 'press', code: 'ArrowUp', modifiers: 0, text: '', unshifted: 0 },
      // With control held the key types nothing: what it sends is the source's to work out
      { action: 'press', code: 'KeyC', modifiers: 2, text: '', unshifted: 99 },
      { action: 'press', code: 'KeyB', modifiers: 4, text: '∫', unshifted: 98 },
    ])
  })

  it('held down is a repeat, and let go is a release that types nothing', () => {
    const { source, key } = mountedView()
    key('keydown', { code: 'KeyJ', key: 'j', repeat: true })
    key('keyup', { code: 'KeyJ', key: 'j' })
    expect(source.keys.map((sent) => [sent.action, sent.text])).toEqual([['repeat', 'j'], ['release', '']])
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
