import { describe, expect, test } from 'vitest'
import { Span } from '../src/span.js'
import wireFixture from './fixtures/span-wire.json' with { type: 'json' }

describe('Span', () => {
  test('constructs with required fields and defaults', () => {
    const span = new Span({
      traceId: 'trace-1',
      spanId: 'span-1',
      kind: 'llm_call',
      name: 'openai.gpt-4o',
    })
    expect(span.traceId).toBe('trace-1')
    expect(span.status).toBe('pending')
    expect(span.sequence).toBe(0)
    expect(span.timestamp).toBeInstanceOf(Date)
    expect(span.tags).toEqual([])
  })

  test('finish() sets status and computes duration', async () => {
    const span = new Span({ traceId: 't', spanId: 's', kind: 'k', name: 'n' })
    await new Promise((r) => setTimeout(r, 5))
    span.finish('ok')
    expect(span.status).toBe('ok')
    expect(span.durationMs).toBeGreaterThan(0)
    expect(span.durationMs).toBeLessThan(1000)
  })

  test('finish() accepts explicit duration', () => {
    const span = new Span({ traceId: 't', spanId: 's', kind: 'k', name: 'n' })
    span.finish('error', 123.4)
    expect(span.status).toBe('error')
    expect(span.durationMs).toBe(123.4)
  })

  test('toEventDict() emits required fields', () => {
    const span = new Span({
      traceId: 't1',
      spanId: 's1',
      kind: 'llm_call',
      name: 'openai.gpt-4o',
    })
    span.finish('ok', 200)
    const d = span.toEventDict()
    expect(d.type).toBe('span')
    expect(d.trace_id).toBe('t1')
    expect(d.span_id).toBe('s1')
    expect(d.kind).toBe('llm_call')
    expect(d.name).toBe('openai.gpt-4o')
    expect(d.status).toBe('ok')
    expect(d.duration_ms).toBe(200)
    expect(d.sequence).toBe(0)
    expect(typeof d.timestamp).toBe('string')
    expect(d.timestamp).toMatch(/^\d{4}-\d{2}-\d{2}T/)
  })

  test('toEventDict() omits null optional fields', () => {
    const span = new Span({ traceId: 't', spanId: 's', kind: 'k', name: 'n' })
    const d = span.toEventDict()
    expect(d).not.toHaveProperty('parent_span_id')
    expect(d).not.toHaveProperty('model')
    expect(d).not.toHaveProperty('input_tokens')
    expect(d).not.toHaveProperty('cost_usd')
    expect(d).not.toHaveProperty('agent_name')
  })

  test('toEventDict() includes optional fields when set', () => {
    const span = new Span({ traceId: 't', spanId: 's', kind: 'llm_call', name: 'n' })
    span.model = 'gpt-4o'
    span.inputTokens = 100
    span.outputTokens = 50
    span.costUsd = 0.0015
    span.parentSpanId = 'parent-1'
    span.agentName = 'research-bot'
    span.environment = 'production'
    const d = span.toEventDict()
    expect(d.model).toBe('gpt-4o')
    expect(d.input_tokens).toBe(100)
    expect(d.output_tokens).toBe(50)
    expect(d.cost_usd).toBe(0.0015)
    expect(d.parent_span_id).toBe('parent-1')
    expect(d.agent_name).toBe('research-bot')
    expect(d.environment).toBe('production')
  })

  test('tags ride in payload jsonb under reserved key', () => {
    const span = new Span({ traceId: 't', spanId: 's', kind: 'k', name: 'n' })
    span.tags = ['a', 'b']
    const d = span.toEventDict()
    expect(d.payload).toEqual({ tags: ['a', 'b'] })
  })

  test('payload merges with tag-injected payload', () => {
    const span = new Span({ traceId: 't', spanId: 's', kind: 'k', name: 'n' })
    span.payload = { custom: 'value' }
    span.tags = ['tag1']
    const d = span.toEventDict()
    expect(d.payload).toEqual({ custom: 'value', tags: ['tag1'] })
  })

  test('produces wire-format identical to fixture (Python parity)', () => {
    const span = new Span({
      traceId: 'fixture-trace-1',
      spanId: 'fixture-span-1',
      kind: 'llm_call',
      name: 'openai.gpt-4o',
    })
    span.timestamp = new Date('2026-05-07T12:00:00.000Z')
    span.model = 'gpt-4o'
    span.inputTokens = 120
    span.outputTokens = 35
    span.costUsd = 0.000656
    span.agentName = 'research-bot'
    span.environment = 'production'
    span.tags = ['test', 'wire-fixture']
    span.finish('ok', 250)

    const dict = span.toEventDict()
    expect(dict).toEqual(wireFixture)
  })
})

describe('Span Plan 2 governance attrs', () => {
  it('exposes user identity attrs in toEventDict', () => {
    const span = new Span({ traceId: 'a'.repeat(16), spanId: 'b'.repeat(16), kind: 'llm_call', name: 'test' })
    span.userId = 'u_42'
    span.userEmail = 'a@b.co'
    span.userAttrs = { plan: 'pro' }
    span.releaseVersion = '1.0.0'
    span.finish('ok')
    const dict = span.toEventDict() as Record<string, unknown>
    expect(dict.user_id).toBe('u_42')
    expect(dict.user_email).toBe('a@b.co')
    expect(dict.user_attrs).toEqual({ plan: 'pro' })
    expect(dict.release_version).toBe('1.0.0')
  })

  it('exposes governance enforcement attrs', () => {
    const span = new Span({ traceId: 'a'.repeat(16), spanId: 'b'.repeat(16), kind: 'llm_call', name: 'test' })
    span.policyDecisions = [{ tool_name: 'wire_money', policy_kind: 'block', pattern: 'wire_*' }]
    span.policyBlockedToolCalls = [{ id: 'tc_1', name: 'wire_money' }]
    span.approvalId = 'apr_123'
    span.budgetCheck = { estimated: 0.05, allowed: true }
    span.finish('ok')
    const dict = span.toEventDict() as Record<string, unknown>
    expect(dict.policy_decisions).toHaveLength(1)
    expect(dict.policy_blocked_tool_calls).toHaveLength(1)
    expect(dict.approval_id).toBe('apr_123')
    expect(dict.budget_check).toEqual({ estimated: 0.05, allowed: true })
  })

  it('omits Plan 2 attrs when unset', () => {
    const span = new Span({ traceId: 'a'.repeat(16), spanId: 'b'.repeat(16), kind: 'llm_call', name: 'test' })
    span.finish('ok')
    const dict = span.toEventDict() as Record<string, unknown>
    expect(dict.user_id).toBeUndefined()
    expect(dict.policy_decisions).toBeUndefined()
    expect(dict.approval_id).toBeUndefined()
  })
})
