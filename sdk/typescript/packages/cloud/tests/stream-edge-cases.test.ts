// Stream enforcement edge cases raised in review of the streaming change.
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { runEnforcedCall } from '../src/enforcement.js'
import { JamjetPolicyBlocked } from '../src/errors.js'
import type { SpanEventDict } from '../src/span.js'

function makeClient() {
  const client = new Client(resolveConfig({ apiKey: 'jj_t', project: 'p', maxCostUsd: 100 }))
  const spans: SpanEventDict[] = []
  client.recordSpan = (e) => {
    spans.push(e)
  }
  setActive(client)
  return { client, spans }
}

const chunk = (delta: Record<string, unknown>, finish: string | null = null) => ({
  model: 'gpt-4o',
  choices: [{ index: 0, delta, finish_reason: finish }],
})

async function readAll(stream: unknown): Promise<{ seen: any[]; error: unknown }> {
  const seen: any[] = []
  try {
    for await (const c of stream as AsyncIterable<any>) seen.push(c)
    return { seen, error: null }
  } catch (error) {
    return { seen, error }
  }
}

describe('stream edge cases', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('checks a tool name split across deltas as the joined name too', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'wire_money')
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () =>
        (async function* () {
          yield chunk({ tool_calls: [{ index: 0, id: 'c1', type: 'function', function: { name: 'wire_', arguments: '' } }] })
          yield chunk({ tool_calls: [{ index: 0, function: { name: 'money', arguments: '{}' } }] })
          yield chunk({}, 'tool_calls')
        })(),
      args: [{ model: 'gpt-4o', stream: true, messages: [] }],
    })
    const { seen, error } = await readAll(stream)
    expect(error).toBeInstanceOf(JamjetPolicyBlocked)
    expect(seen).toHaveLength(0)
  })

  it('enforces direct next() calls when the stream is itself an iterator', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'wire_*')
    const stream = (await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () =>
        (async function* () {
          yield chunk({ tool_calls: [{ index: 0, id: 'c1', type: 'function', function: { name: 'wire_money', arguments: '{}' } }] })
          yield chunk({}, 'tool_calls')
        })(),
      args: [{ model: 'gpt-4o', stream: true, messages: [] }],
    })) as AsyncIterator<any>
    await expect(stream.next()).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('holds tool chunks for a rule added after the stream started', async () => {
    const { client } = makeClient()
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () =>
        (async function* () {
          yield chunk({ content: 'hi' })
          client._policy.add('block', 'wire_*')
          yield chunk({ tool_calls: [{ index: 0, id: 'c1', type: 'function', function: { name: 'wire_money', arguments: '{}' } }] })
          yield chunk({}, 'tool_calls')
        })(),
      args: [{ model: 'gpt-4o', stream: true, messages: [] }],
    })
    const { seen, error } = await readAll(stream)
    expect(error).toBeInstanceOf(JamjetPolicyBlocked)
    expect(seen.some((c) => c.choices[0].delta.tool_calls)).toBe(false)
  })

  it('records usage when the upstream stream fails part way', async () => {
    const { client, spans } = makeClient()
    const stream = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: async () =>
        (async function* () {
          yield { type: 'message_start', message: { model: 'claude-sonnet-4-6', usage: { input_tokens: 4000, output_tokens: 1 } } }
          throw new Error('connection reset')
        })(),
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    const { error } = await readAll(stream)
    expect((error as Error).message).toBe('connection reset')
    expect(client._budget.spent).toBeGreaterThan(0)
    expect(spans).toHaveLength(1)
    expect(spans[0]!.status).toBe('error')
    expect(spans[0]!.input_tokens).toBe(4000)
  })

  it('marks a stream that ends without a completion event as incomplete', async () => {
    const { spans, client } = makeClient()
    const stream = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: async () =>
        (async function* () {
          yield { type: 'message_start', message: { model: 'claude-sonnet-4-6', usage: { input_tokens: 10, output_tokens: 1 } } }
          yield { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'partial' } }
        })(),
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    await readAll(stream)
    expect(spans[0]!.payload?.['stream_incomplete']).toBe(true)
    expect(spans[0]!.payload?.['usage_estimated']).toBe(true)
  })

  it('counts tool definitions and legacy prompts in the input estimate', async () => {
    const { client, spans } = makeClient()
    const bigTool = { type: 'function', function: { name: 'search', description: 'x'.repeat(4000), parameters: {} } }
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () => (async function* () { yield chunk({ content: 'ok' }, 'stop') })(),
      args: [{ model: 'gpt-4o', stream: true, messages: [], tools: [bigTool] }],
    })
    await readAll(stream)
    expect(spans[0]!.input_tokens).toBeGreaterThan(1000)
    const legacy = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () =>
        (async function* () { yield { model: 'gpt-3.5-turbo-instruct', choices: [{ index: 0, text: 'y'.repeat(400), finish_reason: 'stop' }] } })(),
      args: [{ model: 'gpt-3.5-turbo-instruct', stream: true, prompt: 'p'.repeat(4000) }],
    })
    await readAll(legacy)
    expect(spans[1]!.input_tokens).toBeGreaterThan(1000)
    expect(spans[1]!.output_tokens).toBe(100)
  })
})
