import { describe, expect, test, vi, afterEach } from 'vitest'
import { patchOpenAI, unpatchOpenAI } from '../../src/patcher/openai.js'
import { Client, getActive, resetActive, setActive } from '../../src/client.js'
import type { ResolvedConfig } from '../../src/config.js'
import type { SpanEventDict } from '../../src/span.js'
import type { Transport } from '../../src/transport.js'

class FakeCompletions {
  async create(args: any): Promise<any> {
    return {
      id: 'cmpl-1',
      model: args.model,
      usage: { prompt_tokens: 10, completion_tokens: 20 },
    }
  }
}

class CapturingTransport {
  events: SpanEventDict[] = []
  async send(e: SpanEventDict[]) { this.events.push(...e) }
}

const cfg: ResolvedConfig = {
  apiKey: 'k', apiUrl: 'https://api.jamjet.dev', project: 'p', agent: 'default',
  sampling: { rate: 1, alwaysKeepErrors: true, alwaysKeepApprovals: true },
  redaction: { mode: 'off', custom: [] }, debug: true,
}

describe('patchOpenAI', () => {
  afterEach(async () => {
    unpatchOpenAI()
    await resetActive()
  })

  test('patches Completions.create at prototype level so new instances are covered', async () => {
    const fakeModule = {
      resources: {
        chat: { completions: { Completions: FakeCompletions } },
      },
    }
    patchOpenAI(fakeModule as any)

    const transport = new CapturingTransport()
    setActive(new Client(cfg, transport as unknown as Transport))

    const inst = new FakeCompletions()
    await inst.create({ model: 'gpt-4o', messages: [] })

    await getActive()?.shutdown()
    expect(transport.events).toHaveLength(1)
    expect(transport.events[0]?.name).toBe('openai.gpt-4o')
  })

  test('idempotent — second patch is a no-op', () => {
    const fakeModule = {
      resources: {
        chat: { completions: { Completions: FakeCompletions } },
      },
    }
    const original = FakeCompletions.prototype.create
    patchOpenAI(fakeModule as any)
    const afterFirst = FakeCompletions.prototype.create
    patchOpenAI(fakeModule as any)
    const afterSecond = FakeCompletions.prototype.create
    expect(afterFirst).toBe(afterSecond)
    expect(afterFirst).not.toBe(original)
  })

  test('unpatchOpenAI restores original', () => {
    const fakeModule = {
      resources: {
        chat: { completions: { Completions: FakeCompletions } },
      },
    }
    const original = FakeCompletions.prototype.create
    patchOpenAI(fakeModule as any)
    expect(FakeCompletions.prototype.create).not.toBe(original)
    unpatchOpenAI()
    expect(FakeCompletions.prototype.create).toBe(original)
  })

  test('records error span on failure and rethrows', async () => {
    class Failing {
      async create() { throw new Error('boom') }
    }
    const fakeModule = {
      resources: {
        chat: { completions: { Completions: Failing } },
      },
    }
    patchOpenAI(fakeModule as any)
    const transport = new CapturingTransport()
    setActive(new Client(cfg, transport as unknown as Transport))

    await expect(new Failing().create()).rejects.toThrow('boom')
    await getActive()?.shutdown()
    expect(transport.events).toHaveLength(1)
    expect(transport.events[0]?.status).toBe('error')
  })
})
