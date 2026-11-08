// require_approval rules gate the tool calls a model returns: the SDK opens a
// Cloud approval and waits for the decision before handing the call to user
// code, and fails closed on rejection, timeout, or an unreachable Cloud.
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest'
import { setupServer } from 'msw/node'
import { http, HttpResponse } from 'msw'
import { createHash } from 'node:crypto'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { runEnforcedCall } from '../src/enforcement.js'
import { JamjetApprovalRejected, JamjetApprovalTimeout } from '../src/errors.js'
import { policy } from '../src/governance.js'
import type { SpanEventDict } from '../src/span.js'

const API = 'https://api.jamjet.test'
const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

function makeClient(opts: { approvalTimeoutMs?: number; redaction?: 'off' | 'standard' } = {}) {
  const cfg = resolveConfig({
    apiKey: 'jj_t',
    project: 'p',
    apiUrl: API,
    approvalPollIntervalMs: 10,
    redaction: { mode: opts.redaction ?? 'off' },
    ...(opts.approvalTimeoutMs !== undefined ? { approvalTimeoutMs: opts.approvalTimeoutMs } : {}),
  })
  const client = new Client(cfg)
  const spans: SpanEventDict[] = []
  client.recordSpan = (e) => {
    spans.push(e)
  }
  setActive(client)
  return { client, spans }
}

type Created = { body: Record<string, any> }

function approvals(decide: () => { status: string; reason?: string }, created: Created[]) {
  server.use(
    http.post(`${API}/v1/approvals`, async ({ request }) => {
      created.push({ body: (await request.json()) as Record<string, any> })
      return HttpResponse.json({ id: `apr_${created.length}` })
    }),
    http.get(`${API}/v1/approvals/:id`, () => HttpResponse.json(decide())),
  )
}

const openaiToolCall = (name: string, args = '{"amount":120,"to":"ACME"}') => async () => ({
  model: 'gpt-4o',
  usage: { prompt_tokens: 10, completion_tokens: 5 },
  choices: [
    { index: 0, message: { content: null, tool_calls: [{ id: 'call_9', type: 'function', function: { name, arguments: args } }] } },
  ],
})

describe('require_approval on returned tool calls', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('opens a Cloud approval with the call details and the SDK timeout, then returns once approved', async () => {
    const { client, spans } = makeClient({ approvalTimeoutMs: 120_000 })
    policy('require_approval', 'payments.*')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    const result = (await runEnforcedCall({
      client,
      vendor: 'openai',
      original: openaiToolCall('payments.refund'),
      args: [{ model: 'gpt-4o', messages: [] }],
    })) as any
    expect(result.choices[0].message.tool_calls[0].function.name).toBe('payments.refund')
    expect(created).toHaveLength(1)
    expect(created[0]!.body).toMatchObject({
      action: 'payments.refund',
      timeout_ms: 120_000,
      context: {
        tool: 'payments.refund',
        tool_call_id: 'call_9',
        arguments: { amount: 120, to: 'ACME' },
        // sha256 of the canonical JSON {"amount":120,"to":"ACME"}
        arguments_sha256: createHash('sha256').update('{"amount":120,"to":"ACME"}').digest('hex'),
        model: 'gpt-4o',
        vendor: 'openai',
        rule: 'payments.*',
      },
    })
    expect(spans[0]!.status).toBe('ok')
    expect(spans[0]!.approval_id).toBe('apr_1')
  })

  it('throws JamjetApprovalRejected when the approver rejects', async () => {
    const { client, spans } = makeClient()
    policy('require_approval', 'payments.*')
    approvals(() => ({ status: 'rejected', reason: 'not this vendor' }), [])
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: openaiToolCall('payments.refund'),
        args: [{ model: 'gpt-4o', messages: [] }],
      }),
    ).rejects.toMatchObject({ name: 'JamjetApprovalRejected', reason: 'not this vendor' })
    expect(spans).toHaveLength(1)
    expect(spans[0]!.status).toBe('error')
  })

  it('throws JamjetApprovalTimeout when no decision arrives in time', async () => {
    const { client } = makeClient({ approvalTimeoutMs: 80 })
    policy('require_approval', 'payments.*')
    approvals(() => ({ status: 'pending' }), [])
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: openaiToolCall('payments.refund'),
        args: [{ model: 'gpt-4o', messages: [] }],
      }),
    ).rejects.toBeInstanceOf(JamjetApprovalTimeout)
  })

  it('fails closed when the approval cannot be created', async () => {
    const { client } = makeClient()
    policy('require_approval', 'payments.*')
    server.use(http.post(`${API}/v1/approvals`, () => HttpResponse.json({ error: 'down' }, { status: 503 })))
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original: openaiToolCall('payments.refund'),
        args: [{ model: 'gpt-4o', messages: [] }],
      }),
    ).rejects.toBeInstanceOf(JamjetApprovalTimeout)
  })

  it('does not ask for approval when the model does not call a gated tool', async () => {
    const { client } = makeClient()
    policy('require_approval', 'payments.*')
    // onUnhandledRequest: 'error' fails the test on any approval request.
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original: openaiToolCall('search'),
      args: [{ model: 'gpt-4o', messages: [] }],
    })
  })

  it('redacts PII in the approval context when redaction is on', async () => {
    const { client } = makeClient({ redaction: 'standard' })
    policy('require_approval', 'email.*')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original: openaiToolCall('email.send', '{"to":"jane.doe@example.com"}'),
      args: [{ model: 'gpt-4o', messages: [] }],
    })
    expect(JSON.stringify(created[0]!.body['context']['arguments'])).not.toContain('jane.doe@example.com')
  })
})

describe('approval context and binding', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('masks credentials but keeps what the reviewer needs to judge', async () => {
    const { client } = makeClient({ redaction: 'standard' })
    policy('require_approval', 'pay')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    const args = JSON.stringify({ to: 'DE89370400440532013000', payee: 'ACME GmbH', amount: 5, auth: 'Bearer sk-live-abcdef0123456789', note: 'token sk-live-abcdef0123456789' })
    await runEnforcedCall({ client, vendor: 'openai', original: openaiToolCall('pay', args), args: [{ model: 'gpt-4o', messages: [] }] })
    const ctx = created[0]!.body['context']['arguments']
    expect(ctx).toMatchObject({ to: '****3000', payee: 'ACME GmbH', amount: 5, auth: '[SECRET]' })
    expect(JSON.stringify(ctx)).not.toContain('sk-live-abcdef0123456789')
  })

  it('masks credentials even with redaction off', async () => {
    const { client } = makeClient({ redaction: 'off' })
    policy('require_approval', 'pay')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    const args = JSON.stringify({ password: 'hunter2', header: 'Authorization: Bearer abc.def.ghi', to: 'DE89370400440532013000' })
    await runEnforcedCall({ client, vendor: 'openai', original: openaiToolCall('pay', args), args: [{ model: 'gpt-4o', messages: [] }] })
    const ctx = created[0]!.body['context']['arguments']
    expect(ctx).toMatchObject({ password: '[SECRET]', to: 'DE89370400440532013000' })
    expect(JSON.stringify(ctx)).not.toContain('abc.def.ghi')
  })

  it('refuses to release a call whose arguments changed while awaiting approval', async () => {
    const { client } = makeClient()
    policy('require_approval', 'pay')
    const response = {
      model: 'gpt-4o',
      usage: { prompt_tokens: 1, completion_tokens: 1 },
      choices: [{ index: 0, message: { tool_calls: [{ id: 'c1', type: 'function', function: { name: 'pay', arguments: '{"amount":5}' } }] } }],
    }
    server.use(
      http.post(`${API}/v1/approvals`, () => HttpResponse.json({ id: 'apr_1' })),
      http.get(`${API}/v1/approvals/:id`, () => {
        response.choices[0]!.message.tool_calls[0]!.function.arguments = '{"amount":5000}'
        return HttpResponse.json({ status: 'approved' })
      }),
    )
    await expect(
      runEnforcedCall({ client, vendor: 'openai', original: async () => response, args: [{ model: 'gpt-4o', messages: [] }] }),
    ).rejects.toMatchObject({ name: 'JamjetApprovalRejected', reason: 'tool call arguments changed after approval' })
  })

  it("stops waiting when the caller's AbortSignal fires", async () => {
    const { client } = makeClient()
    policy('require_approval', 'pay')
    approvals(() => ({ status: 'pending' }), [])
    const ac = new AbortController()
    const p = runEnforcedCall({
      client,
      vendor: 'openai',
      original: openaiToolCall('pay'),
      args: [{ model: 'gpt-4o', messages: [] }, { signal: ac.signal }],
    })
    p.catch(() => {})
    setTimeout(() => ac.abort(), 30)
    await expect(p).rejects.toMatchObject({ name: 'AbortError' })
  })

  it('gates a streamed tool name split across deltas by its joined name', async () => {
    const { client } = makeClient()
    policy('require_approval', 'payments.*')
    const created: Created[] = []
    approvals(() => ({ status: 'rejected' }), created)
    const chunks = [
      { model: 'gpt-4o', choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'c1', type: 'function', function: { name: 'payments', arguments: '' } }] } }] },
      { model: 'gpt-4o', choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { name: '.refund', arguments: '{"amount":1}' } }] } }] },
      { model: 'gpt-4o', choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
    ]
    const s = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () => ({ [Symbol.asyncIterator]: async function* () { for (const c of chunks) yield c } }),
      args: [{ model: 'gpt-4o', messages: [], stream: true }],
    })
    await expect(
      (async () => {
        for await (const _ of s as AsyncIterable<unknown>) { /* read */ }
      })(),
    ).rejects.toBeInstanceOf(JamjetApprovalRejected)
    expect(created[0]!.body['action']).toBe('payments.refund')
  })
})

describe('approval re-review fixes', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  it('masks credentials by shape and an explicit field list, not every *_token field', async () => {
    const { client } = makeClient()
    policy('require_approval', 'pay')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    const args = JSON.stringify({
      payee_token: 'ACME-PAYEE-001',
      access_token: 'short',
      note: 'key Zx8Qm2Lp9Rt4Vb7Nc1Kd6Hs3Jf5Gw0Ye',
      digest: 'a3f5c9e1b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2',
      jwt: 'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl',
    })
    await runEnforcedCall({ client, vendor: 'openai', original: openaiToolCall('pay', args), args: [{ model: 'gpt-4o', messages: [] }] })
    expect(created[0]!.body['context']['arguments']).toEqual({
      payee_token: 'ACME-PAYEE-001',
      access_token: '[SECRET]',
      note: 'key [SECRET]',
      digest: 'a3f5c9e1b2d4f6a8c0e2b4d6f8a0c2e4b6d8f0a2',
      jwt: '[SECRET]',
    })
  })

  it('two choices calling the same legacy function with different arguments are each approved', async () => {
    const { client } = makeClient()
    policy('require_approval', 'pay')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () => ({
        model: 'gpt-4o',
        usage: { prompt_tokens: 1, completion_tokens: 1 },
        choices: [
          { index: 0, message: { function_call: { name: 'pay', arguments: '{"amount":1}' } } },
          { index: 1, message: { function_call: { name: 'pay', arguments: '{"amount":2}' } } },
        ],
      }),
      args: [{ model: 'gpt-4o', messages: [], n: 2 }],
    })
    expect(created.map((c) => c.body['context']['tool_call_id'])).toEqual(['function_call:0', 'function_call:1'])
  })

  it('opens one approval when both the last fragment and the joined name are gated', async () => {
    const { client } = makeClient()
    policy('require_approval', '*refund')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    const chunks = [
      { model: 'gpt-4o', choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'c1', type: 'function', function: { name: 'pay', arguments: '' } }] } }] },
      { model: 'gpt-4o', choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { name: 'ments.refund', arguments: '{}' } }] } }] },
      { model: 'gpt-4o', choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] },
    ]
    const s = await runEnforcedCall({
      client,
      vendor: 'openai',
      original: async () => ({ [Symbol.asyncIterator]: async function* () { for (const c of chunks) yield c } }),
      args: [{ model: 'gpt-4o', messages: [], stream: true }],
    })
    for await (const _ of s as AsyncIterable<unknown>) { /* read */ }
    expect(created.map((c) => c.body['action'])).toEqual(['payments.refund'])
  })
})

describe('require_approval on streamed tool calls', () => {
  beforeEach(resetActive)
  afterEach(resetActive)

  const events = [
    { type: 'message_start', message: { model: 'claude-sonnet-4-6', usage: { input_tokens: 10, output_tokens: 1 } } },
    { type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: 'tu_7', name: 'payments.refund', input: {} } },
    { type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: '{"amount":5}' } },
    { type: 'content_block_stop', index: 0 },
    { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 20 } },
    { type: 'message_stop' },
  ]
  const stream = async () => ({
    [Symbol.asyncIterator]: async function* () {
      for (const e of events) yield e
    },
  })

  it('holds the tool call until it is approved, then releases it', async () => {
    const { client, spans } = makeClient()
    policy('require_approval', 'payments.*')
    const created: Created[] = []
    approvals(() => ({ status: 'approved' }), created)
    const s = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: stream,
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    const seen: any[] = []
    for await (const e of s as AsyncIterable<any>) seen.push(e)
    expect(seen).toEqual(events)
    expect(created[0]!.body['context']).toMatchObject({ tool: 'payments.refund', tool_call_id: 'tu_7', arguments: { amount: 5 } })
    expect(spans[0]!.approval_id).toBe('apr_1')
  })

  it('never hands a rejected tool call to the caller', async () => {
    const { client, spans } = makeClient()
    policy('require_approval', 'payments.*')
    approvals(() => ({ status: 'rejected' }), [])
    const s = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original: stream,
      args: [{ model: 'claude-sonnet-4-6', stream: true, messages: [] }],
    })
    const seen: any[] = []
    await expect(
      (async () => {
        for await (const e of s as AsyncIterable<any>) seen.push(e)
      })(),
    ).rejects.toBeInstanceOf(JamjetApprovalRejected)
    expect(seen.map((e) => e.type)).toEqual(['message_start'])
    expect(spans).toHaveLength(1)
    expect(spans[0]!.status).toBe('error')
    // 10 in * $3/MTok + 20 out * $15/MTok, counted once
    expect(client._budget.spent).toBeCloseTo(10 * 3e-6 + 20 * 15e-6, 15)
  })
})
