// The patchers against the real `openai` and `@anthropic-ai/sdk` packages,
// with a stub fetch in place of the network. Fake resource classes cannot
// catch a patcher that looks for the class in the wrong place, or a patched
// create() that drops the methods the SDKs' own helpers call on its result.
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import * as anthropicModule from '@anthropic-ai/sdk'
import * as openaiModule from 'openai'
import { Client, getActive, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { JamjetPolicyBlocked } from '../src/errors.js'
import { patchAnthropic, unpatchAnthropic } from '../src/patcher/anthropic.js'
import { patchOpenAI, unpatchOpenAI } from '../src/patcher/openai.js'

const Anthropic = anthropicModule.default
const OpenAI = openaiModule.default

type Captured = { body: any }

function sse(events: Array<{ event?: string; data: unknown }>): string {
  return events
    .map((e) => `${e.event ? `event: ${e.event}\n` : ''}data: ${typeof e.data === 'string' ? e.data : JSON.stringify(e.data)}\n\n`)
    .join('')
}

function stubFetch(respond: (body: any) => { json?: unknown; sse?: string }, captured: Captured[]) {
  return async (_url: unknown, init?: { body?: unknown }) => {
    const body = JSON.parse(String(init?.body ?? '{}'))
    captured.push({ body })
    const r = respond(body)
    if (r.sse !== undefined) {
      return new Response(r.sse, { status: 200, headers: { 'content-type': 'text/event-stream' } })
    }
    return new Response(JSON.stringify(r.json), { status: 200, headers: { 'content-type': 'application/json' } })
  }
}

function anthropicEvents(toolName: string) {
  return sse([
    { event: 'message_start', data: { type: 'message_start', message: { id: 'msg_1', type: 'message', role: 'assistant', model: 'claude-sonnet-4-6', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 2000, output_tokens: 1 } } } },
    { event: 'content_block_start', data: { type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: 'tu_1', name: toolName, input: {} } } },
    { event: 'content_block_delta', data: { type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: '{"q":"x"}' } } },
    { event: 'content_block_stop', data: { type: 'content_block_stop', index: 0 } },
    { event: 'message_delta', data: { type: 'message_delta', delta: { stop_reason: 'tool_use', stop_sequence: null }, usage: { output_tokens: 1000 } } },
    { event: 'message_stop', data: { type: 'message_stop' } },
  ])
}

function openaiChunks(toolName: string) {
  const base = { id: 'c1', object: 'chat.completion.chunk', created: 1, model: 'gpt-4o' }
  return sse([
    { data: { ...base, choices: [{ index: 0, delta: { role: 'assistant', tool_calls: [{ index: 0, id: 'call_1', type: 'function', function: { name: toolName, arguments: '' } }] }, finish_reason: null }] } },
    { data: { ...base, choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: '{}' } }] }, finish_reason: null }] } },
    { data: { ...base, choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] } },
    { data: '[DONE]' },
  ])
}

function strictTool(name: string) {
  const parameters = { type: 'object', properties: {}, required: [], additionalProperties: false }
  return { type: 'function' as const, function: { name, parameters, strict: true } }
}

describe('patchers against the real vendor SDKs', () => {
  beforeEach(async () => {
    await resetActive()
    // Spans stay in the process: no events go to a JamJet API.
    const client = new Client(resolveConfig({ apiKey: 'jj_t', project: 'p', apiUrl: 'https://api.jamjet.test', maxCostUsd: 100 }))
    client.recordSpan = () => {}
    setActive(client)
    patchOpenAI(openaiModule)
    patchAnthropic(anthropicModule)
  })
  afterEach(async () => {
    unpatchOpenAI()
    unpatchAnthropic()
    await resetActive()
  })

  it('anthropic: strips a blocked tool from messages.create()', async () => {
    getActive()!._policy.add('block', 'wire_*')
    const captured: Captured[] = []
    const client = new Anthropic({
      apiKey: 'x',
      fetch: stubFetch(() => ({
        json: { id: 'msg_1', type: 'message', role: 'assistant', model: 'claude-sonnet-4-6', content: [], stop_reason: 'end_turn', stop_sequence: null, usage: { input_tokens: 1, output_tokens: 1 } },
      }), captured) as any,
    })
    await client.messages.create({
      model: 'claude-sonnet-4-6',
      max_tokens: 64,
      messages: [{ role: 'user', content: 'hi' }],
      tools: [
        { name: 'search', input_schema: { type: 'object' } },
        { name: 'wire_money', input_schema: { type: 'object' } },
      ],
    })
    expect(captured[0]!.body.tools.map((t: any) => t.name)).toEqual(['search'])
  })

  it('anthropic: messages.stream() works and refuses a blocked tool call', async () => {
    getActive()!._policy.add('block', 'wire_*')
    const client = new Anthropic({ apiKey: 'x', fetch: stubFetch(() => ({ sse: anthropicEvents('wire_money') }), []) as any })
    const stream = client.messages.stream({ model: 'claude-sonnet-4-6', max_tokens: 64, messages: [{ role: 'user', content: 'pay' }] })
    // MessageStream re-wraps errors it did not raise as AnthropicError, keeping ours as the cause.
    const err = await stream.finalMessage().then(
      () => null,
      (e: unknown) => e as Error,
    )
    expect(err?.cause ?? err).toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('anthropic: messages.stream() records the streamed cost', async () => {
    const client = new Anthropic({ apiKey: 'x', fetch: stubFetch(() => ({ sse: anthropicEvents('search') }), []) as any })
    const final = await client.messages
      .stream({ model: 'claude-sonnet-4-6', max_tokens: 64, messages: [{ role: 'user', content: 'look' }] })
      .finalMessage()
    expect(final.content[0]).toMatchObject({ type: 'tool_use', name: 'search' })
    expect(getActive()!._budget.spent).toBeCloseTo(2000 * 3e-6 + 1000 * 15e-6, 12)
  })

  it('openai: refuses a blocked tool call in a stream', async () => {
    getActive()!._policy.add('block', 'wire_*')
    const client = new OpenAI({ apiKey: 'x', fetch: stubFetch(() => ({ sse: openaiChunks('wire_money') }), []) as any })
    const stream = await client.chat.completions.create({ model: 'gpt-4o', stream: true, messages: [{ role: 'user', content: 'pay' }] })
    await expect(
      (async () => {
        for await (const _ of stream) { /* read */ }
      })(),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('openai: strips a blocked tool, and chat.completions.parse() still works', async () => {
    getActive()!._policy.add('block', 'wire_*')
    const captured: Captured[] = []
    const client = new OpenAI({
      apiKey: 'x',
      fetch: stubFetch(() => ({
        json: { id: 'c1', object: 'chat.completion', created: 1, model: 'gpt-4o', choices: [{ index: 0, message: { role: 'assistant', content: 'ok', refusal: null }, finish_reason: 'stop', logprobs: null }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } },
      }), captured) as any,
    })
    const parsed = await client.chat.completions.parse({
      model: 'gpt-4o',
      messages: [{ role: 'user', content: 'hi' }],
      tools: [strictTool('search'), strictTool('wire_money')],
    })
    expect(parsed.choices[0]!.message.content).toBe('ok')
    expect(captured[0]!.body.tools.map((t: any) => t.function.name)).toEqual(['search'])
  })
})
