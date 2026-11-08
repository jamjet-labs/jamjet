// Regression tests from the independent review's probes
// (each asserted the buggy behaviour; these assert the fix).
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import v8 from 'node:v8'
import vm from 'node:vm'
import * as anthropicModule from '@anthropic-ai/sdk'
import * as openaiModule from 'openai'
import { Client, getActive, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { runEnforcedCall } from '../src/enforcement.js'
import { JamjetPolicyBlocked, JamjetUnenforceableCall } from '../src/errors.js'
import { patchAnthropic, unpatchAnthropic } from '../src/patcher/anthropic.js'
import { patchOpenAI, unpatchOpenAI } from '../src/patcher/openai.js'
import type { SpanEventDict } from '../src/span.js'

const Anthropic = anthropicModule.default
const OpenAI = openaiModule.default

function makeClient() {
  const client = new Client(resolveConfig({ apiKey: 'jj_t', project: 'p', apiUrl: 'https://api.jamjet.test', maxCostUsd: 100 }))
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

describe('legacy OpenAI functions / function_call', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('strips a blocked function and refuses a returned function_call', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'delete_*')
    let sent: any
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: async (args: any) => {
          sent = args
          return {
            model: 'gpt-4o',
            usage: { prompt_tokens: 1, completion_tokens: 1 },
            choices: [{ index: 0, message: { content: null, function_call: { name: 'delete_everything', arguments: '{}' } } }],
          }
        },
        args: [{ model: 'gpt-4o', messages: [], functions: [{ name: 'delete_everything', parameters: {} }, { name: 'search', parameters: {} }] }],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(sent.functions.map((f: any) => f.name)).toEqual(['search'])
  })

  it('refuses a call whose function_call forces a blocked function', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'delete_*')
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: async () => ({}),
        args: [{ model: 'gpt-4o', messages: [], functions: [{ name: 'delete_everything', parameters: {} }], function_call: { name: 'delete_everything' } }],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('holds and refuses a streamed delta.function_call', async () => {
    const { client } = makeClient()
    client._policy.add('block', 'delete_*')
    const chunks = [
      { model: 'gpt-4o', choices: [{ index: 0, delta: { function_call: { name: 'delete_everything', arguments: '' } } }] },
      { model: 'gpt-4o', choices: [{ index: 0, delta: { function_call: { arguments: '{}' } }, finish_reason: 'function_call' }] },
    ]
    const stream = await runEnforcedCall({ client, vendor: 'openai', original: streamOf(chunks), args: [{ model: 'gpt-4o', messages: [], stream: true }] })
    const seen: unknown[] = []
    await expect(
      (async () => {
        for await (const c of stream as AsyncIterable<unknown>) seen.push(c)
      })(),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(seen).toHaveLength(0)
  })
})

describe('tools the vendor runs itself', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  async function sentWith(rules: Array<['block' | 'require_approval', string]>, body: Record<string, unknown>) {
    const { client } = makeClient()
    for (const [a, p] of rules) client._policy.add(a, p)
    let sent: any
    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: async (args: any) => {
        sent = args
        return { model: 'claude-sonnet-4-6', usage: { input_tokens: 1, output_tokens: 1 }, content: [] }
      },
      args: [{ model: 'claude-sonnet-4-6', messages: [], ...body }],
    })
    return sent
  }

  it('strips a server tool a require_approval rule matches, since its call cannot be held', async () => {
    const sent = await sentWith([['require_approval', 'web_*']], {
      tools: [{ type: 'web_search_20250305', name: 'web_search' }, { name: 'web_lookup', input_schema: { type: 'object' } }],
    })
    expect(sent.tools.map((t: any) => t.name)).toEqual(['web_lookup'])
  })

  it('keeps Anthropic client tools (bash, text editor, computer, memory) to gate their calls', async () => {
    const sent = await sentWith([['require_approval', 'bash']], {
      tools: [{ type: 'bash_20250124', name: 'bash' }, { type: 'text_editor_20250728', name: 'str_replace_based_edit_tool' }],
    })
    expect(sent.tools.map((t: any) => t.name)).toEqual(['bash', 'str_replace_based_edit_tool'])
  })

  it('strips MCP toolsets and servers while any restrictive rule exists', async () => {
    const sent = await sentWith([['block', 'delete_*']], {
      tools: [{ type: 'mcp_toolset', mcp_server_name: 'db' }, { name: 'search', input_schema: { type: 'object' } }],
      mcp_servers: [{ type: 'url', url: 'https://mcp.example', name: 'db' }],
    })
    expect(sent.tools.map((t: any) => t.name)).toEqual(['search'])
    expect(sent).not.toHaveProperty('mcp_servers')
  })

  it('drops parallel_tool_calls when every tool is stripped', async () => {
    const { client } = makeClient()
    client._policy.add('block', '*')
    let sent: any
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async (args: any) => {
        sent = args
        return { model: 'gpt-4o', usage: { prompt_tokens: 1, completion_tokens: 1 }, choices: [{ message: {} }] }
      },
      args: [{ model: 'gpt-4o', messages: [], tools: [{ type: 'function', function: { name: 'a' } }], parallel_tool_calls: false }],
    })
    expect(sent).not.toHaveProperty('tools')
    expect(sent).not.toHaveProperty('parallel_tool_calls')
  })
})

describe('abandoned streams', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('records usage for an iterator dropped after next(), once it is collected', async () => {
    v8.setFlagsFromString('--expose_gc')
    const gc = vm.runInNewContext('gc') as () => void
    const { client, spans } = makeClient()
    const chunks = [
      { model: 'gpt-4o', choices: [{ index: 0, delta: { content: 'x'.repeat(4000) } }] },
      { model: 'gpt-4o', choices: [{ index: 0, delta: { content: 'y'.repeat(4000) } }] },
    ]
    const stream = (await runEnforcedCall({ client, vendor: 'openai', original: streamOf(chunks), args: [{ model: 'gpt-4o', messages: [], stream: true }] })) as any
    await (async () => {
      const it = stream[Symbol.asyncIterator]()
      await it.next()
    })()
    for (let i = 0; i < 20 && spans.length === 0; i++) {
      gc()
      await new Promise((r) => setTimeout(r, 10))
    }
    expect(spans).toHaveLength(1)
    expect(spans[0]!.payload?.['stream_incomplete']).toBe(true)
    expect(client._budget.spent).toBeGreaterThan(0)
  })
})

describe('other SDK entry points', () => {
  function stub(json: unknown, captured: any[]) {
    return async (_u: unknown, init?: { body?: unknown }) => {
      captured.push(JSON.parse(String(init?.body ?? '{}')))
      return new Response(JSON.stringify(json), { status: 200, headers: { 'content-type': 'application/json' } })
    }
  }
  beforeEach(async () => {
    await resetActive()
    makeClient()
    patchOpenAI(openaiModule)
    patchAnthropic(anthropicModule)
    getActive()!._policy.add('block', 'delete_*')
  })
  afterEach(async () => {
    unpatchOpenAI()
    unpatchAnthropic()
    await resetActive()
  })

  it('openai responses.create is refused while a restrictive rule is active', async () => {
    const cap: any[] = []
    const client = new OpenAI({ apiKey: 'x', fetch: stub({}, cap) as any })
    expect(() => client.responses.create({ model: 'gpt-4o', input: 'x' })).toThrow(JamjetUnenforceableCall)
    expect(cap).toHaveLength(0)
  })

  it('anthropic beta.messages.create strips blocked tools and refuses their calls', async () => {
    const cap: any[] = []
    const msg = { id: 'm', type: 'message', role: 'assistant', model: 'claude-sonnet-4-6', content: [{ type: 'tool_use', id: 't', name: 'delete_all', input: {} }], stop_reason: 'tool_use', stop_sequence: null, usage: { input_tokens: 1, output_tokens: 1 } }
    const client = new Anthropic({ apiKey: 'x', fetch: stub(msg, cap) as any })
    await expect(
      client.beta.messages.create({
        model: 'claude-sonnet-4-6',
        max_tokens: 10,
        messages: [{ role: 'user', content: 'x' }],
        tools: [{ name: 'delete_all', input_schema: { type: 'object' } }, { name: 'search', input_schema: { type: 'object' } }],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
    expect(cap[0].tools.map((t: any) => t.name)).toEqual(['search'])
  })

  it('a patched create keeps asResponse(), built from the enforced body', async () => {
    const comp = { id: 'c', object: 'chat.completion', created: 1, model: 'gpt-4o', choices: [{ index: 0, message: { role: 'assistant', content: 'ok', refusal: null }, finish_reason: 'stop', logprobs: null }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } }
    const client = new OpenAI({ apiKey: 'x', fetch: stub(comp, []) as any })
    const res = await (client.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] }) as any).asResponse()
    expect(res.status).toBe(200)
    expect((await res.json()).choices[0].message.content).toBe('ok')
    const blocked = { ...comp, choices: [{ index: 0, message: { role: 'assistant', content: null, refusal: null, tool_calls: [{ id: 't', type: 'function', function: { name: 'delete_all', arguments: '{}' } }] }, finish_reason: 'tool_calls', logprobs: null }] }
    const client2 = new OpenAI({ apiKey: 'x', fetch: stub(blocked, []) as any })
    await expect((client2.chat.completions.create({ model: 'gpt-4o', messages: [{ role: 'user', content: 'x' }] }) as any).asResponse()).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('batch APIs are refused while a restrictive rule is active', async () => {
    const cap: any[] = []
    const oai = new OpenAI({ apiKey: 'x', fetch: stub({}, cap) as any })
    const ant = new Anthropic({ apiKey: 'x', fetch: stub({}, cap) as any })
    expect(() => oai.batches.create({ input_file_id: 'f', endpoint: '/v1/chat/completions', completion_window: '24h' })).toThrow(JamjetUnenforceableCall)
    expect(() => ant.messages.batches.create({ requests: [] })).toThrow(JamjetUnenforceableCall)
    expect(() => ant.beta.messages.batches.create({ requests: [] })).toThrow(JamjetUnenforceableCall)
    expect(cap).toHaveLength(0)
  })

  it('a streaming asResponse()/withResponse() body is the enforced stream as SSE', async () => {
    const base = { id: 'c1', object: 'chat.completion.chunk', created: 1, model: 'gpt-4o' }
    const sseBody = (name: string) =>
      [
        { ...base, choices: [{ index: 0, delta: { role: 'assistant', content: 'hi' }, finish_reason: null }] },
        { ...base, choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'c', type: 'function', function: { name, arguments: '{}' } }] }, finish_reason: null }] },
        { ...base, choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
      ]
        .map((c) => `data: ${JSON.stringify(c)}\n\n`)
        .join('') + 'data: [DONE]\n\n'
    const sseFetch = (name: string) => async () => new Response(sseBody(name), { status: 200, headers: { 'content-type': 'text/event-stream' } })
    const ok = new OpenAI({ apiKey: 'x', fetch: sseFetch('search') as any })
    const res = await (ok.chat.completions.create({ model: 'gpt-4o', stream: true, messages: [{ role: 'user', content: 'x' }] }) as any).asResponse()
    expect(res.headers.get('content-type')).toBe('text/event-stream')
    const text = await res.text()
    expect(text.startsWith('data: {')).toBe(true)
    expect(text.trim().endsWith('data: [DONE]')).toBe(true)
    const bad = new OpenAI({ apiKey: 'x', fetch: sseFetch('delete_all') as any })
    const { response } = await (bad.chat.completions.create({ model: 'gpt-4o', stream: true, messages: [{ role: 'user', content: 'x' }] }) as any).withResponse()
    const read = await response.text().then(
      (t: string) => t,
      (e: unknown) => e,
    )
    expect(read).toBeInstanceOf(JamjetPolicyBlocked)
  })
})
