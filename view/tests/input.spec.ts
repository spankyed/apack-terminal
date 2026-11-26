// Keyboard, IME composition, AltGr, macOS Option-as-Alt, paste confirmation, and dirty-checked field styles.
import { beforeEach, describe, expect, it } from 'vitest'
import { Input } from '../src/input.ts'
import { TestClock } from './_clock.ts'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView, settled } from './_view.ts'

beforeEach(() => installFrames())

describe('Input & keyboard handling', () => {
  it('sends physical key, modifiers, text, and unshiftedCodepoint', () => {
    const { source, key } = mountedView()
    key('keydown', { code: 'KeyA', key: 'a' })
    key('keydown', { code: 'KeyA', key: 'A', shiftKey: true })
    key('keydown', { code: 'ArrowUp', key: 'ArrowUp' })
    key('keydown', { code: 'KeyC', key: 'c', ctrlKey: true })
    key('keydown', { code: 'KeyB', key: '∫', altKey: true })
    expect(source.keys).toEqual([
      { action: 'press', code: 'KeyA', modifiers: 0, text: 'a', unshiftedCodepoint: 97, composed: false },
      { action: 'press', code: 'KeyA', modifiers: 1, text: 'A', unshiftedCodepoint: 97, composed: false },
      { action: 'press', code: 'ArrowUp', modifiers: 0, text: '', unshiftedCodepoint: 0, composed: false },
      { action: 'press', code: 'KeyC', modifiers: 2, text: '', unshiftedCodepoint: 99, composed: false },
      { action: 'press', code: 'KeyB', modifiers: 4, text: '∫', unshiftedCodepoint: 98, composed: true },
    ])
  })

  it('handles repeat/release, AltGr on Windows/Linux, and macOS optionIsAlt', () => {
    const { source, key } = mountedView({ mac: false })
    key('keydown', { code: 'KeyJ', key: 'j', repeat: true })
    key('keyup', { code: 'KeyJ', key: 'j' })
    expect(source.keys.map((s) => [s.action, s.text])).toEqual([['repeat', 'j'], ['release', '']])

    source.keys.length = 0
    key('keydown', { code: 'KeyQ', key: '@', ctrlKey: true, altKey: true, modifierAltGraph: true })
    key('keydown', { code: 'KeyQ', key: 'q', ctrlKey: true, altKey: true })
    expect(source.keys).toEqual([
      { action: 'press', code: 'KeyQ', modifiers: 0, text: '@', unshiftedCodepoint: 113, composed: false },
      { action: 'press', code: 'KeyQ', modifiers: 6, text: '', unshiftedCodepoint: 113, composed: false },
    ])

    const macAlt = mountedView({ mac: true, optionIsAlt: true })
    const accent = macAlt.key('keydown', { code: 'KeyE', key: 'Dead', altKey: true })
    expect(accent.defaultPrevented).toBe(true)
    expect(macAlt.source.keys.at(-1)).toEqual({
      action: 'press',
      code: 'KeyE',
      modifiers: 4,
      text: 'e',
      unshiftedCodepoint: 101,
      composed: false,
    })
  })

  it('supports IME composition and deduplicates key-generated input events via Clock', () => {
    const clock = new TestClock()
    const { source, key, field } = mountedView({ clock })
    key('keydown', { code: 'KeyA', key: 'a' })
    field.value = 'a'
    field.dispatchEvent(new Event('input', { bubbles: true }))
    // Deduplicated because keydown already sent 'a'
    expect(source.texts).toEqual([])
    expect(source.keys).toHaveLength(1)

    // After clock fires, picker/dictation input goes through
    clock.advance(1)
    field.value = '🎉'
    field.dispatchEvent(new Event('input', { bubbles: true }))
    expect(source.texts).toEqual(['🎉'])

    // IME composition commits once on compositionend
    field.dispatchEvent(new Event('compositionstart'))
    field.value = '日本語'
    field.dispatchEvent(new Event('input'))
    expect(source.texts).toEqual(['🎉'])
    field.dispatchEvent(new Event('compositionend'))
    expect(source.texts).toEqual(['🎉', '日本語'])

    // Blur mid-composition resets composing state so subsequent keys are never stuck
    field.dispatchEvent(new Event('compositionstart'))
    field.value = 'stuck'
    field.dispatchEvent(new Event('blur'))
    expect(field.value).toBe('')
    expect(field.style.opacity).toBe('0')
    key('keydown', { code: 'KeyB', key: 'b' })
    expect(source.keys.at(-1)?.code).toBe('KeyB')
  })

  it('confirms unsafe pastes before resending with allowUnsafe=true and reports paste errors', async () => {
    const source = new FakeSource()
    source.pasteOutcome = 'unsafe'
    let confirmed = false
    const errors: unknown[] = []
    const { view } = mountedView(
      {
        confirmPaste: async () => {
          confirmed = true
          source.pasteOutcome = 'pasted'
          return true
        },
        onPasteError: (err) => errors.push(err),
      },
      source,
    )
    source.send(frame({ rows: [row(0, '$ ')] }))
    runFrames()

    await view.paste('echo hi\n')
    expect(confirmed).toBe(true)
    expect(source.pastes).toEqual([
      { text: 'echo hi\n', allowUnsafe: false },
      { text: 'echo hi\n', allowUnsafe: true },
    ])

    // Error callback when paste throws
    source.paste = async () => {
      throw new Error('too large')
    }
    const ev = new Event('paste', { bubbles: true, cancelable: true }) as ClipboardEvent
    Object.defineProperty(ev, 'clipboardData', { value: { getData: () => 'big' } })
    view.element.querySelector('textarea')!.dispatchEvent(ev)
    await settled()
    expect(errors).toHaveLength(1)
  })

  it('dirty-checks Input.moveTo and Input.appearance to avoid redundant DOM style writes', () => {
    const source = new FakeSource()
    const input = new Input(source, {
      mac: true,
      paste: () => {},
      copy: () => {},
      focused: () => {},
    })
    let leftWrites = 0
    const origDesc = Object.getOwnPropertyDescriptor(CSSStyleDeclaration.prototype, 'left')
    Object.defineProperty(input.field.style, 'left', {
      configurable: true,
      get() { return origDesc?.get?.call(this) ?? '' },
      set(v: string) {
        leftWrites += 1
        origDesc?.set?.call(this, v)
      },
    })
    input.moveTo(20, 40, 18)
    input.moveTo(20, 40, 18)
    input.moveTo(20, 40, 18)
    expect(leftWrites).toBe(1)
    input.moveTo(30, 40, 18)
    expect(leftWrites).toBe(2)
    input.dispose()
  })
})
