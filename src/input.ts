/**
 * The keyboard, as a view hears it.
 *
 * **A key is sent as what the keyboard said, not as bytes.** What an arrow, a function key or a letter
 * with control held sends a program depends on modes the program set, which only the terminal's owner
 * knows — so this reports the physical key, the modifiers and the text it typed, and the source encodes.
 *
 * **Text that did not come from a key goes another way.** An input method composes several keys into a
 * character, an emoji picker inserts one with no key at all; both arrive as text in a hidden field, which
 * is here to receive exactly that and to give the page something that can hold focus.
 */
import { Modifier, type KeyInput, type TerminalSource } from './source.ts'

/** What each key types with no modifier on a US keyboard, for a browser that cannot say what this one's types */
const US_LAYOUT: Record<string, string> = {
  Backquote: '`', Minus: '-', Equal: '=', BracketLeft: '[', BracketRight: ']', Backslash: '\\',
  Semicolon: ';', Quote: "'", Comma: ',', Period: '.', Slash: '/', Space: ' ', IntlBackslash: '\\',
}

/** The browser's map from a physical key to what it types unshifted, where the browser has one */
type LayoutMap = { get(code: string): string | undefined }

export interface InputOptions {
  /** A Mac's command key belongs to the app, and its option key composes characters */
  readonly mac: boolean
  /**
   * The option key is alt, as a person who set their terminal that way expects: it composes nothing, and a
   * key held with it is sent as the character the key types without it. Only a Mac has the choice.
   */
  readonly optionIsAlt?: boolean
  /**
   * A key the host wants for itself — a shortcut. Answering `true` keeps it from the program. Asked before
   * anything else, for every key.
   */
  readonly intercept?: (event: KeyboardEvent) => boolean
  /** Text arrived to be pasted */
  readonly paste: (text: string) => void
  /** The person asked to copy: a shortcut, or the page's own copy command */
  readonly copy: () => void
  readonly focused: (focused: boolean) => void
  /** The person typed something the program was sent: a key that is more than a modifier, or text */
  readonly typed?: () => void
}

/** The keys that only change what another key means */
const MODIFIER_KEYS = new Set(['Shift', 'Control', 'Alt', 'AltGraph', 'Meta', 'CapsLock', 'NumLock', 'ScrollLock', 'Fn', 'Hyper', 'Super'])

/** How the field that shows a composition is drawn, so what is being composed looks like what it will become */
export interface FieldAppearance {
  /** A CSS font shorthand */
  readonly font: string
  readonly color: string
  readonly background: string
}

/** Whether a string is a control character, which no key types */
function isControl(character: string): boolean {
  const point = character.codePointAt(0) ?? 0
  return point < 0x20 || (point >= 0x7f && point < 0xa0)
}

/**
 * What a key types with at most shift held, from what it types with nothing held: itself, or for a letter
 * its capital. Empty where that cannot be known — a key with nothing known of it, or the shifted character
 * of a key that is not a letter, which is the layout's to say — and the owner encodes from the key itself.
 */
function withShift(unshifted: number, shift: boolean): string {
  if (unshifted === 0) return ''
  const plain = String.fromCodePoint(unshifted)
  if (!shift) return plain
  const capital = plain.toUpperCase()
  return capital !== plain && [...capital].length === 1 ? capital : ''
}

function modifiersOf(event: KeyboardEvent | MouseEvent): number {
  return (event.shiftKey ? Modifier.shift : 0)
    | (event.ctrlKey ? Modifier.control : 0)
    | (event.altKey ? Modifier.alt : 0)
    | (event.metaKey ? Modifier.super : 0)
    | (event.getModifierState?.('CapsLock') ? Modifier.capsLock : 0)
    | (event.getModifierState?.('NumLock') ? Modifier.numLock : 0)
}

export { modifiersOf }

export class Input {
  /** Where text that is not a key arrives, and what holds the keyboard's focus */
  readonly field: HTMLTextAreaElement
  private layout: LayoutMap | null = null
  private composing = false
  /** The keys the source was told went down and has not been told came up */
  private readonly down = new Set<string>()

  constructor(private readonly source: TerminalSource, private readonly options: InputOptions) {
    const field = document.createElement('textarea')
    field.setAttribute('aria-label', 'Terminal input')
    field.setAttribute('autocapitalize', 'off')
    field.setAttribute('autocomplete', 'off')
    field.setAttribute('autocorrect', 'off')
    field.spellcheck = false
    field.tabIndex = 0
    // On the page and focusable, and not seen: an input method draws its own window beside it
    field.style.cssText = 'position:absolute;left:0;top:0;width:1px;height:1px;padding:0;border:0;margin:0;'
      + 'opacity:0;resize:none;overflow:hidden;white-space:pre;outline:none;caret-color:transparent;z-index:1'
    this.field = field

    field.addEventListener('keydown', this.keyDown)
    field.addEventListener('keyup', this.keyUp)
    field.addEventListener('compositionstart', this.compositionStart)
    field.addEventListener('compositionend', this.compositionEnd)
    field.addEventListener('input', this.inserted)
    field.addEventListener('paste', this.pasted)
    field.addEventListener('copy', this.copied)
    field.addEventListener('focus', this.focus)
    field.addEventListener('blur', this.blur)

    // What each key types unshifted on *this* keyboard, which a key event itself does not say
    const keyboard = (navigator as Navigator & { keyboard?: { getLayoutMap?: () => Promise<LayoutMap> } }).keyboard
    void keyboard?.getLayoutMap?.().then((layout) => { this.layout = layout }, () => {})
  }

  /**
   * Puts the field where the cursor is, so an input method's window opens beside what is being typed.
   * In pixels from the corner of the element the field is in, and one row high.
   */
  moveTo(x: number, y: number, height: number): void {
    this.field.style.left = `${x}px`
    this.field.style.top = `${y}px`
    this.field.style.height = `${height}px`
    this.field.style.lineHeight = `${height}px`
  }

  /** What a composition is drawn in: the terminal's own font and colours, over the cells it will replace */
  appearance(appearance: FieldAppearance): void {
    this.field.style.font = appearance.font
    this.field.style.color = appearance.color
    this.field.style.background = appearance.background
  }

  dispose(): void {
    this.field.remove()
  }

  private unshifted(code: string): number {
    const typed = this.layout?.get(code)
      ?? (/^Key[A-Z]$/.test(code) ? code.slice(3).toLowerCase() : /^Digit\d$/.test(code) ? code.slice(5) : US_LAYOUT[code])
    return typed?.codePointAt(0) ?? 0
  }

  /** The key as the source is told it, or `null` for one that is the input method's or the app's */
  private read(event: KeyboardEvent, action: KeyInput['action']): KeyInput | null {
    // The command key is the app's on a Mac: a program never hears it
    if (this.options.mac && event.metaKey) return null
    const unshifted = this.unshifted(event.code)
    // An option key that is alt composes nothing, the accents it would otherwise start included: option
    // and E is alt and `e`, not the beginning of an `é`
    const optionAsAlt = this.options.mac && this.options.optionIsAlt === true && event.altKey && !event.ctrlKey
    if (optionAsAlt && !this.composing && !event.isComposing) {
      const release = action === 'release'
      return { action, code: event.code, modifiers: modifiersOf(event), text: release ? '' : withShift(unshifted, event.shiftKey), unshifted, composed: false }
    }
    // A key the input method is in the middle of, or one that only starts an accent
    if (this.composing || event.isComposing || event.key === 'Dead' || event.key === 'Process' || event.keyCode === 229) return null
    // One character, and one that is typed: a control character is a key's doing, never its text
    const character = [...event.key].length === 1 && !isControl(event.key)
    let modifiers = modifiersOf(event)
    let text = ''
    let composed = false
    if (character && event.getModifierState?.('AltGraph') === true) {
      // The key that reaches a keyboard's third level — `@`, `{`, `\` on most European layouts — is reported
      // by Windows as control and alt together. Those two are how the character was typed, not held with it
      modifiers &= ~(Modifier.control | Modifier.alt)
      text = event.key
    } else if (this.options.mac && event.altKey && !event.ctrlKey) {
      // A Mac's option key is its third level, and the system has already typed the character by the time
      // a page hears the key. Alt stays among the modifiers: `composed` is what says it typed the character
      if (character) {
        text = event.key
        composed = event.key !== withShift(unshifted, false) && event.key !== withShift(unshifted, true)
      }
    } else if (character && !event.ctrlKey && !event.metaKey) {
      text = event.key
    }
    const release = action === 'release'
    return { action, code: event.code, modifiers, text: release ? '' : text, unshifted, composed: !release && composed }
  }

  private readonly keyDown = (event: KeyboardEvent) => {
    if (this.options.intercept?.(event)) {
      event.preventDefault()
      return
    }
    // Copy and paste where the command key is not there to ask with: control and shift together
    if (!this.options.mac && event.ctrlKey && event.shiftKey && !event.altKey) {
      if (event.code === 'KeyC') {
        event.preventDefault()
        return this.options.copy()
      }
      // The browser's own paste follows, and brings the text with it
      if (event.code === 'KeyV') return
    }
    if (this.options.mac && event.metaKey && event.code === 'KeyC') {
      event.preventDefault()
      return this.options.copy()
    }
    const key = this.read(event, event.repeat ? 'repeat' : 'press')
    if (!key) return
    // Sent as a key, so it must not also arrive as text typed into the field
    event.preventDefault()
    this.down.add(event.code)
    this.source.key(key)
    if (!MODIFIER_KEYS.has(event.key)) this.options.typed?.()
  }

  /** A release is sent for a key whose press was: one the host kept, or the app's, never went down for the program */
  private readonly keyUp = (event: KeyboardEvent) => {
    if (!this.down.delete(event.code)) return
    const key = this.read(event, 'release')
    if (key) this.source.key(key)
  }

  private readonly compositionStart = () => {
    this.composing = true
    // What is being composed is shown where it will go, in the field itself
    this.field.style.opacity = '1'
    this.field.style.width = '20em'
  }

  private readonly compositionEnd = (event: CompositionEvent) => {
    this.composing = false
    this.field.style.opacity = '0'
    this.field.style.width = '1px'
    this.field.value = ''
    this.source.text(event.data)
    if (event.data !== '') this.options.typed?.()
  }

  /** Text put in the field by something other than a key or a composition: a picker, dictation */
  private readonly inserted = (event: Event) => {
    if (this.composing || (event as InputEvent).isComposing) return
    const text = this.field.value
    this.field.value = ''
    this.source.text(text)
    if (text !== '') this.options.typed?.()
  }

  private readonly pasted = (event: ClipboardEvent) => {
    event.preventDefault()
    const text = event.clipboardData?.getData('text/plain') ?? ''
    if (text !== '') this.options.paste(text)
  }

  private readonly copied = (event: ClipboardEvent) => {
    event.preventDefault()
    this.options.copy()
  }

  private readonly focus = () => this.options.focused(true)
  private readonly blur = () => {
    // Keys let go while something else has the keyboard are never heard of again
    this.down.clear()
    this.options.focused(false)
  }
}
