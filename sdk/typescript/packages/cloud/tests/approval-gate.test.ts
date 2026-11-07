// require_approval rules gate the tool calls a model returns: the SDK opens a
// Cloud approval and waits for the decision before handing the call to user
// code, and fails closed on rejection, timeout, or an unreachable Cloud.
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from 'vitest'
import { setupServer } from 'msw/node'
import { http, HttpResponse } from 'msw'
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
        arguments: '{"amount":120,"to":"ACME"}',
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
    expect(created[0]!.body['context']['arguments']).not.toContain('jane.doe@example.com')
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
