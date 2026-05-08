import { afterEach, beforeEach, describe, expect, it, test, vi } from 'vitest'
import { wrap } from '../src/wrap.js'
import { Client, getActive, resetActive, setActive } from '../src/client.js'
import type { ResolvedConfig } from '../src/config.js'
import { resolveConfig } from '../src/config.js'
import { JamjetBudgetExceeded } from '../src/errors.js'
import { agent } from '../src/governance.js'
import type { SpanEventDict } from '../src/span.js'
import type { Transport } from '../src/transport.js'

const cfg: ResolvedConfig = {
  apiKey: 'k',
  apiUrl: 'https://api.jamjet.dev',
  project: 'p',
  agent: 'default',
  sampling: { rate: 1, alwaysKeepErrors: true, alwaysKeepApprovals: true },
  redaction: { mode: 'off', custom: [] },
  debug: true,
}

class CapturingTransport {
  events: SpanEventDict[] = []
  async send(events: SpanEventDict[]) { this.events.push(...events) }
}

const fakeOpenAIClient = () => ({
  chat: {
    completions: {
      create: vi.fn(async (args: any) => ({
        id: 'cmpl-1',
        model: args.model,
        usage: { prompt_tokens: 10, completion_tokens: 20, total_tokens: 30 },
        choices: [{ message: { role: 'assistant', content: 'hi' }, finish_reason: 'stop' }],
      })),
    },
  },
})

describe('wrap(openai)', () => {
  let transport: CapturingTransport

  afterEach(async () => {
    await resetActive()
  })

  test('emits span on chat.completions.create', async () => {
    transport = new CapturingTransport()
    setActive(new Client(cfg, transport as unknown as Transport))
    const client = wrap(fakeOpenAIClient())
    const res = await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
    expect(res.id).toBe('cmpl-1')

    const active = (await import('../src/client.js')).getActive()
    await active?.shutdown()
    expect(transport.events).toHaveLength(1)
    const span = transport.events[0]!
    expect(span.kind).toBe('llm_call')
    expect(span.name).toBe('openai.gpt-4o')
    expect(span.model).toBe('gpt-4o')
    expect(span.input_tokens).toBe(10)
    expect(span.output_tokens).toBe(20)
    expect(span.cost_usd).toBeGreaterThan(0)
    expect(span.status).toBe('ok')
    expect(span.duration_ms).toBeGreaterThanOrEqual(0)
  })

  test('records error span when underlying call throws', async () => {
    transport = new CapturingTransport()
    setActive(new Client(cfg, transport as unknown as Transport))
    const failing = {
      chat: {
        completions: {
          create: vi.fn(async () => { throw new Error('boom') }),
        },
      },
    }
    const client = wrap(failing as any)
    await expect(
      client.chat.completions.create({ model: 'gpt-4o', messages: [] }),
    ).rejects.toThrow('boom')

    const active = (await import('../src/client.js')).getActive()
    await active?.shutdown()
    expect(transport.events).toHaveLength(1)
    expect(transport.events[0]?.status).toBe('error')
  })

  test('passes through when no active client (fail-soft)', async () => {
    const client = wrap(fakeOpenAIClient())
    const res = await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
    expect(res.id).toBe('cmpl-1')
  })
})

describe('wrap(anthropic)', () => {
  test('emits span on messages.create', async () => {
    const transport = new CapturingTransport()
    setActive(new Client(cfg, transport as unknown as Transport))

    const client = wrap({
      messages: {
        create: vi.fn(async (args: any) => ({
          id: 'msg-1',
          model: args.model,
          usage: { input_tokens: 50, output_tokens: 100 },
          content: [{ type: 'text', text: 'hello' }],
        })),
      },
    } as any)

    await (client as any).messages.create({
      model: 'claude-sonnet-4-6',
      messages: [{ role: 'user', content: 'hi' }],
    })

    const active = (await import('../src/client.js')).getActive()
    await active?.shutdown()
    expect(transport.events).toHaveLength(1)
    const span = transport.events[0]!
    expect(span.name).toBe('anthropic.claude-sonnet-4-6')
    expect(span.input_tokens).toBe(50)
    expect(span.output_tokens).toBe(100)
    expect(span.cost_usd).toBeCloseTo(50 * 3e-6 + 100 * 15e-6, 8)
    await resetActive()
  })
})

describe('wrap Plan 2 enforcement', () => {
  beforeEach(async () => {
    await resetActive()
    setActive(new Client(resolveConfig({ apiKey: 'jj_t', project: 'p', maxCostUsd: 100 })))
  })
  afterEach(async () => {
    await resetActive()
  })

  it('strips blocked tools before calling original (openai shape)', async () => {
    getActive()!._policy.add('block', 'wire_*')
    let received: any
    const fakeOpenai = {
      chat: { completions: { create: async (args: any) => {
        received = args
        return { usage: { prompt_tokens: 1, completion_tokens: 1 }, model: 'gpt-4o', choices: [{ message: {} }] }
      } } },
    }
    wrap(fakeOpenai)
    await fakeOpenai.chat.completions.create({
      model: 'gpt-4o', messages: [],
      tools: [{ type: 'function', function: { name: 'search' } }, { type: 'function', function: { name: 'wire_money' } }],
    })
    expect(received.tools).toHaveLength(1)
  })

  it('opts.agent overrides context agent', async () => {
    let recorded: any
    const c = getActive()!
    const orig = c.recordSpan.bind(c)
    c.recordSpan = (e) => { recorded = e; return orig(e) }
    const fakeOpenai = {
      chat: { completions: { create: async (..._args: any[]) => ({ usage: { prompt_tokens: 1, completion_tokens: 1 }, model: 'gpt-4o', choices: [{ message: {} }] }) } },
    }
    wrap(fakeOpenai, { agent: agent('explicit_agent') })
    await fakeOpenai.chat.completions.create({ model: 'gpt-4o', messages: [] })
    expect(recorded.agent_name).toBe('explicit_agent')
  })

  it('throws JamjetBudgetExceeded on pre-call', async () => {
    getActive()!._budget.setLimit(0.0001)
    getActive()!._budget.record(0.0001)
    const fakeOpenai = {
      chat: { completions: { create: async (..._args: any[]) => ({ usage: { prompt_tokens: 1, completion_tokens: 1 }, model: 'gpt-4o', choices: [{ message: {} }] }) } },
    }
    wrap(fakeOpenai)
    await expect(fakeOpenai.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x'.repeat(1_000) }] }))
      .rejects.toBeInstanceOf(JamjetBudgetExceeded)
  })
})
