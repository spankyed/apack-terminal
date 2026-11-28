// Text the host types into a program, as a keyboard would: sent as text, not as a paste, and followed to the screen.
import { beforeEach, describe, expect, it } from 'vitest'
import type { CommandRecord } from '../src/source.ts'
import { installFrames, runFrames } from './_frames.ts'
import { FakeSource } from './_source.ts'
import { frame, row } from './_support.ts'
import { mountedView } from './_view.ts'

beforeEach(() => installFrames())

describe('text a host types into a program', () => {
  it('is sent to the program as text, not as a paste', () => {
    const source = new FakeSource()
    const view = mountedView({}, source)
    view.source.send(frame({ size: { cols: 20, rows: 2 }, rows: [row(0, '$ ')] }))
    runFrames()
    view.view.typeText('ls -la')
    expect(source.texts).toEqual(['ls -la'])
    expect(source.pastes).toEqual([])
  })

  it('follows a view scrolled back to the screen, as a key does', () => {
    const view = mountedView({}, new FakeSource())
    view.source.send(frame({
      size: { cols: 20, rows: 2 },
      firstActiveLine: 100,
      scrollbackLines: 100,
      rows: [row(100, 'a'), row(101, 'b')],
    }))
    runFrames()
    view.view.viewport.scrollBy(-5)
    expect(view.view.viewport.following).toBe(false)
    view.view.typeText('x')
    expect(view.view.viewport.following).toBe(true)
  })
})

describe('the commands a host is told of', () => {
  it('are reported on mount and live events, and one command\'s output is copied synchronously by copyCommand', async () => {
    const source = new FakeSource()
    const copied: string[] = []
    const reported: number[][] = []
    const command: CommandRecord = {
      id: 1,
      command: 'make',
      cwd: null,
      startedMs: 1000,
      finishedMs: 1200,
      outcome: { kind: 'exited', code: 2 },
      promptLine: 0,
      outputLine: 1,
      endLine: 2,
    }
    source.commandList = [command]
    const view = mountedView(
      {
        onCopy: (text) => { copied.push(text) },
        onCommands: (commands) => { reported.push(commands.map((c) => c.id)) },
      },
      source,
    )
    view.source.send(frame({ seq: 1, size: { cols: 20, rows: 3 }, rows: [row(0, '$ make'), row(1, 'error here'), row(2, '$ ')] }))
    runFrames()
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(reported.at(-1)).toEqual([1])
    // Copies directly from cached RowStore in 0 ms without even calling remote textOf!
    expect(await view.view.copyCommand(command)).toBe(true)
    expect(copied).toEqual(['error here'])
    expect(source.remoteTextCalls).toEqual([])
  })
})
