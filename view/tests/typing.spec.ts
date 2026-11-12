// Text the host types into a program, as a keyboard would: sent as text, not as a paste, and followed to the screen.
import { beforeEach, describe, expect, it } from 'vitest'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView } from './_view.ts'

beforeEach(() => installFrames())

describe('text a host types into a program', () => {
  it('is sent to the program as text, not as a paste', () => {
    const source = new FakeSource()
    const view = mountedView({}, source)
    view.source.send(frame({ screenRows: 2, columns: 20, rows: [row(0, '$ ')] }))
    runFrames()
    view.view.typeText('ls -la')
    expect(source.texts).toEqual(['ls -la'])
    expect(source.pastes).toEqual([])
  })

  it('follows a view scrolled back to the screen, as a key does', () => {
    const view = mountedView({}, new FakeSource())
    view.source.send(frame({ screenRows: 2, columns: 20, firstActive: 100, scrollback: 100, rows: [row(100, 'a'), row(101, 'b')] }))
    runFrames()
    view.view.viewport.scrollBy?.(-5)
    expect(view.view.viewport.following).toBe(false)
    view.view.typeText('x')
    expect(view.view.viewport.following).toBe(true)
  })
})

describe('the commands a host is told of', () => {
  it('are reported each time the shell reports them, and one command\'s output is copied by copyCommand', async () => {
    class Printed extends FakeSource {
      override async textOf(): Promise<string> { return 'error here' }
    }
    const source = new Printed()
    const copied: string[] = []
    const reported: number[][] = []
    const view = mountedView({ onCopy: (text) => { copied.push(text) }, onCommands: (commands) => { reported.push(commands.map((command) => command.id)) } }, source)
    const command = { id: 1, command: 'make', cwd: null, startedMs: null, finishedMs: null, outcome: { kind: 'exited' as const, code: 2 }, prompt: 0, output: 1, end: 2 }
    source.commandList = [command]
    view.source.send(frame({ sequence: 1, screenRows: 3, columns: 20, commands: 1, rows: [row(0, '$ make'), row(1, 'error here'), row(2, '$ ')] }))
    runFrames()
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(reported.at(-1)).toEqual([1])
    expect(await view.view.copyCommand(command)).toBe(true)
    expect(copied).toEqual(['error here'])
  })
})
