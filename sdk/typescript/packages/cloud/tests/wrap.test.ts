import { afterEach, describe, expect, test, vi } from 'vitest'
import { wrap } from '../src/wrap.js'
import { Client, resetActive, setActive } from '../src/client.js'
import type { ResolvedConfig } from '../src/config.js'
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
