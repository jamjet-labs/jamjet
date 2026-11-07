// Tool policy across both vendors' tool shapes, and on streaming calls.
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { runEnforcedCall } from '../src/enforcement.js'
import { JamjetBudgetExceeded, JamjetPolicyBlocked } from '../src/errors.js'
import type { SpanEventDict } from '../src/span.js'

function makeClient(opts: { maxCostUsd?: number } = {}): { client: Client; spans: SpanEventDict[] } {
  const cfg = resolveConfig({
    apiKey: 'jj_t',
    project: 'p',
    ...(opts.maxCostUsd !== undefined ? { maxCostUsd: opts.maxCostUsd } : {}),
  })
  const client = new Client(cfg)
  const spans: SpanEventDict[] = []
  client.recordSpan = (e) => {
    spans.push(e)
  }
  setActive(client)
  return { client, spans }
}

function streamOf(chunks: unknown[]): () => Promise<AsyncIterable<unknown>> {
  return async () => ({
    [Symbol.asyncIterator]: async function* () {
      for (const c of chunks) yield c
    },
  })
}

async function drain(stream: unknown): Promise<unknown[]> {
  const seen: unknown[] = []
  for await (const chunk of stream as AsyncIterable<unknown>) seen.push(chunk)
  return seen
}

const ANTHROPIC_TOOLS = [
  { name: 'search', description: 'search the web', input_schema: { type: 'object' } },
  { name: 'wire_money', description: 'send money', input_schema: { type: 'object' } },
]

describe('pre-call tool filtering', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('strips blocked Anthropic tools before the model sees them', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'wire_*')
    let sent: any
    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: async (args: any) => {
        sent = args
        return { model: 'claude-sonnet-4-6', usage: { input_tokens: 1, output_tokens: 1 }, content: [] }
      },
      args: [{ model: 'claude-sonnet-4-6', max_tokens: 64, messages: [], tools: ANTHROPIC_TOOLS }],
    })
    expect(sent.tools.map((t: any) => t.name)).toEqual(['search'])
  })

  it('drops tools and tool_choice when every tool is blocked', async () => {
    const { client } = makeClient()
    client._policy.add('block', '*')
    let sent: any
    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: async (args: any) => {
        sent = args
        return { model: 'claude-sonnet-4-6', usage: { input_tokens: 1, output_tokens: 1 }, content: [] }
      },
      args: [{ model: 'claude-sonnet-4-6', messages: [], tools: ANTHROPIC_TOOLS, tool_choice: { type: 'any' } }],
    })
    expect(sent).not.toHaveProperty('tools')
    expect(sent).not.toHaveProperty('tool_choice')
  })

  it('refuses a call whose tool_choice forces a blocked tool', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'wire_*')
    let called = false
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: async () => {
          called = true
          return {}
        },
        args: [
          {
            model: 'gpt-4o',
            messages: [],
            tools: [
              { type: 'function', function: { name: 'search' } },
              { type: 'function', function: { name: 'wire_money' } },
            ],
            tool_choice: { type: 'function', function: { name: 'wire_money' } },
          },
        ],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(called).toBe(false)
  })

  it('checks tool calls in every choice, not just the first', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'wire_*')
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: async () => ({
          model: 'gpt-4o',
          usage: { prompt_tokens: 1, completion_tokens: 1 },
          choices: [
            { index: 0, message: { content: 'no tools here' } },
            { index: 1, message: { tool_calls: [{ id: 'c2', type: 'function', function: { name: 'wire_money', arguments: '{}' } }] } },
          ],
        }),
        args: [{ model: 'gpt-4o', messages: [], n: 2 }],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('a refused response still counts against the budget', async () => {
    const { client } = makeClient({ maxCostUsd: 100 })
    client._policy.add('block', 'wire_*')
    await expect(
      runEnforcedCall({
        client,
        vendor: 'anthropic',
        original: async () => ({
          model: 'claude-sonnet-4-6',
          usage: { input_tokens: 1000, output_tokens: 1000 },
          content: [{ type: 'tool_use', id: 'tu_1', name: 'wire_money', input: {} }],
        }),
        args: [{ model: 'claude-sonnet-4-6', messages: [] }],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(client._budget.spent).toBeGreaterThan(0)
  })
})

describe('streaming: OpenAI chunks', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  const textThenBlockedCall = [
    { model: 'gpt-4o', choices: [{ index: 0, delta: { role: 'assistant', content: 'Sending it now.' } }] },
    {
      model: 'gpt-4o',
      choices: [
        {
          index: 0,
          delta: {
            tool_calls: [{ index: 0, id: 'call_1', type: 'function', function: { name: 'wire_money', arguments: '' } }],
          },
        },
      ],
    },
    { model: 'gpt-4o', choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '{"to":"x"}' } }] } }] },
    { model: 'gpt-4o', choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
  ]

  it('refuses a blocked tool call and never hands its chunks to the caller', async () => {
    const { client, spans } = makeClient()
    client._policy.add('block', 'wire_*')
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: streamOf(textThenBlockedCall),
      args: [{ model: 'gpt-4o', stream: true, messages: [] }],
    })
    const seen: any[] = []
    await expect(
      (async () => {
        for await (const chunk of stream as AsyncIterable<any>) seen.push(chunk)
      })(),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(seen.map((c) => c.choices[0].delta.content)).toEqual(['Sending it now.'])
    expect(seen.some((c) => c.choices[0].delta.tool_calls)).toBe(false)
    expect(spans).toHaveLength(1)
    expect(spans[0]!.status).toBe('error')
    expect(spans[0]!.policy_blocked_tool_calls).toEqual([{ id: 'call_1', name: 'wire_money' }])
  })

  it('releases allowed tool calls in order after the stream ends', async () => {
    const { client, spans } = makeClient()
    client._policy.add('block', 'shell.*')
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: streamOf(textThenBlockedCall),
      args: [{ model: 'gpt-4o', stream: true, messages: [] }],
    })
    expect(await drain(stream)).toEqual(textThenBlockedCall)
    expect(spans).toHaveLength(1)
    expect(spans[0]!.status).toBe('ok')
  })

  it('records reported usage and cost against the budget', async () => {
    const { client, spans } = makeClient({ maxCostUsd: 100 })
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: streamOf([
        { model: 'gpt-4o', choices: [{ index: 0, delta: { content: 'hi' } }] },
        { model: 'gpt-4o', choices: [], usage: { prompt_tokens: 1000, completion_tokens: 500 } },
      ]),
      args: [{ model: 'gpt-4o', stream: true, messages: [], stream_options: { include_usage: true } }],
    })
    await drain(stream)
    expect(spans[0]!.input_tokens).toBe(1000)
    expect(spans[0]!.output_tokens).toBe(500)
    expect(client._budget.spent).toBeCloseTo(1000 * 2.5e-6 + 500 * 10e-6, 12)
    expect(spans[0]!.payload?.['usage_estimated']).toBeUndefined()
  })

  it('estimates cost when the stream reports no usage', async () => {
    const { client, spans } = makeClient({ maxCostUsd: 100 })
    const stream = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: streamOf([{ model: 'gpt-4o', choices: [{ index: 0, delta: { content: 'x'.repeat(400) } }] }]),
      args: [{ model: 'gpt-4o', stream: true, messages: [{ role: 'user', content: 'hello' }] }],
    })
    await drain(stream)
    expect(spans[0]!.output_tokens).toBe(100)
    expect(client._budget.spent).toBeGreaterThan(0)
    expect(spans[0]!.payload?.['usage_estimated']).toBe(true)
  })
})

describe('streaming: Anthropic events', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  const events = (toolName: string) => [
    { type: 'message_start', message: { model: 'claude-sonnet-4-6', usage: { input_tokens: 2000, output_tokens: 1 } } },
    { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
    { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'On it.' } },
    { type: 'content_block_stop', index: 0 },
    { type: 'content_block_start', index: 1, content_block: { type: 'tool_use', id: 'tu_1', name: toolName, input: {} } },
    { type: 'content_block_delta', index: 1, delta: { type: 'input_json_delta', partial_json: '{"amount":1}' } },
    { type: 'content_block_stop', index: 1 },
    { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 1000 } },
    { type: 'message_stop' },
  ]

  it('stops at the tool_use block of a blocked tool', async () => {
    const { client, spans } = makeClient({ maxCostUsd: 100 })
    client._policy.add('block', 'wire_*')
    let pulledAfterBlock = 0
    const all = events('wire_money')
    const original = async () => ({
      [Symbol.asyncIterator]: async function* () {
        for (const [i, e] of all.entries()) {
          if (i > 4) pulledAfterBlock += 1
          yield e
        }
      },
    })
    const stream = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    const seen: any[] = []
    await expect(
      (async () => {
        for await (const e of stream as AsyncIterable<any>) seen.push(e)
      })(),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(seen.map((e) => e.type)).toEqual(['message_start', 'content_block_start', 'content_block_delta', 'content_block_stop'])
    expect(pulledAfterBlock).toBe(0)
    expect(spans[0]!.policy_blocked_tool_calls).toEqual([{ id: 'tu_1', name: 'wire_money' }])
    expect(client._budget.spent).toBeGreaterThan(0)
  })

  it('records usage, and the budget cap then refuses the next call', async () => {
    const { client, spans } = makeClient({ maxCostUsd: 0.02 })
    const stream = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: streamOf(events('search')),
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    await drain(stream)
    // 2000 in * $3/MTok + 1000 out * $15/MTok = 0.006 + 0.015
    expect(client._budget.spent).toBeCloseTo(0.021, 12)
    expect(spans[0]!.input_tokens).toBe(2000)
    expect(spans[0]!.output_tokens).toBe(1000)
    await expect(
      runEnforcedCall({
        client,
        vendor: 'anthropic',
        original: streamOf(events('search')),
        args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
      }),
    ).rejects.toBeInstanceOf(JamjetBudgetExceeded)
  })

  it('records a stream the caller stops reading early', async () => {
    const { client, spans } = makeClient({ maxCostUsd: 100 })
    const stream = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: streamOf(events('search')),
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    for await (const _ of stream as AsyncIterable<unknown>) break
    expect(spans).toHaveLength(1)
    expect(spans[0]!.payload?.['stream_incomplete']).toBe(true)
    expect(client._budget.spent).toBeGreaterThan(0)
  })
})
