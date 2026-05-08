import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { patchOpenAI, unpatchOpenAI } from '../../src/patcher/openai.js'
import { Client, getActive, resetActive, setActive } from '../../src/client.js'
import { resolveConfig } from '../../src/config.js'
import { JamjetPolicyBlocked } from '../../src/errors.js'
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

describe('patchOpenAI Plan 2 enforcement', () => {
  beforeEach(async () => {
    await resetActive()
    setActive(new Client(resolveConfig({ apiKey: 'jj_t', project: 'p', maxCostUsd: 100 })))
  })
  afterEach(async () => {
    unpatchOpenAI()
    await resetActive()
  })

  test('strips blocked tools and re-checks post-decision', async () => {
    getActive()!._policy.add('block', 'wire_*')
    let received: any
    const fakeOpenAI = {
      resources: {
        chat: {
          completions: {
            Completions: class {
              async create(args: any) {
                received = args
                return { usage: { prompt_tokens: 1, completion_tokens: 1 }, model: 'gpt-4o', choices: [{ message: {} }] }
              }
            },
          },
        },
      },
    }
    patchOpenAI(fakeOpenAI)
    const inst = new (fakeOpenAI as any).resources.chat.completions.Completions()
    await inst.create({
      model: 'gpt-4o', messages: [],
      tools: [{ type: 'function', function: { name: 'search' } }, { type: 'function', function: { name: 'wire_money' } }],
    })
    expect(received.tools).toHaveLength(1)
  })

  test('throws JamjetPolicyBlocked when model invents blocked tool', async () => {
    getActive()!._policy.add('block', 'wire_*')
    const fakeOpenAI = {
      resources: {
        chat: {
          completions: {
            Completions: class {
              async create() {
                return {
                  usage: { prompt_tokens: 1, completion_tokens: 1 },
                  model: 'gpt-4o',
                  choices: [{
                    message: {
                      tool_calls: [{ id: 'tc_1', type: 'function', function: { name: 'wire_money', arguments: '{}' } }],
                    },
                  }],
                }
              }
            },
          },
        },
      },
    }
    patchOpenAI(fakeOpenAI)
    const inst = new (fakeOpenAI as any).resources.chat.completions.Completions()
    await expect(inst.create({ model: 'gpt-4o', messages: [] })).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })
})
