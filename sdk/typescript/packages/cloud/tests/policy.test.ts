import { describe, expect, it } from 'vitest'
import { PolicyEvaluator } from '../src/policy.js'

describe('PolicyEvaluator', () => {
  it('returns allow with no rules', () => {
    const e = new PolicyEvaluator()
    const d = e.evaluate('search')
    expect(d.blocked).toBe(false)
    expect(d.policyKind).toBe('allow')
    expect(d.pattern).toBeNull()
  })

  it('blocks on glob match', () => {
    const e = new PolicyEvaluator()
    e.add('block', 'payments.*')
    const d = e.evaluate('payments.send')
    expect(d.blocked).toBe(true)
    expect(d.policyKind).toBe('block')
    expect(d.pattern).toBe('payments.*')
  })

  it('first matching rule wins (specific before catch-all)', () => {
    const e = new PolicyEvaluator()
    e.add('allow', 'payments.read')
    e.add('block', 'payments.*')
    expect(e.evaluate('payments.send').blocked).toBe(true)
    expect(e.evaluate('payments.read').blocked).toBe(false)
  })

  it('supports require_approval action', () => {
    const e = new PolicyEvaluator()
    e.add('require_approval', 'wire_*')
    const d = e.evaluate('wire_money')
    expect(d.blocked).toBe(false)
    expect(d.policyKind).toBe('require_approval')
    expect(d.pattern).toBe('wire_*')
  })

  it('filterTools partitions allowed and blocked', () => {
    const e = new PolicyEvaluator()
    e.add('block', 'wire_*')
    const tools = [
      { type: 'function', function: { name: 'search', parameters: {} } },
      { type: 'function', function: { name: 'wire_money', parameters: {} } },
    ]
    const { allowed, blocked } = e.filterTools(tools)
    expect(allowed).toHaveLength(1)
    expect(allowed[0]!.function.name).toBe('search')
    expect(blocked).toHaveLength(1)
    expect(blocked[0]!.function.name).toBe('wire_money')
  })

  it('filterTools handles tools without function.name gracefully', () => {
    const e = new PolicyEvaluator()
    e.add('block', '*')
    const tools = [{ type: 'function', function: {} }]
    const { allowed, blocked } = e.filterTools(tools as any)
    expect(blocked).toHaveLength(1)
    expect(allowed).toHaveLength(0)
  })
})

describe('PolicyEvaluator first-match-wins', () => {
  it('returns the FIRST matching rule when multiple rules match', () => {
    const ev = new PolicyEvaluator()
    ev.add('allow', '*')
    ev.add('block', '*delete*')
    const decision = ev.evaluate('database.delete_all')
    // first match is the * rule (allow), so:
    expect(decision.blocked).toBe(false)
    expect(decision.pattern).toBe('*')
    expect(decision.policyKind).toBe('allow')
  })

  it('returns the FIRST match when reverse-ordered', () => {
    const ev = new PolicyEvaluator()
    ev.add('block', '*delete*')
    ev.add('allow', '*')
    const decision = ev.evaluate('database.delete_all')
    // first match is *delete* (block):
    expect(decision.blocked).toBe(true)
    expect(decision.pattern).toBe('*delete*')
  })
})

describe('PolicyEvaluator audit action', () => {
  it('treats audit as a recognized action with policyKind="audit"', () => {
    const ev = new PolicyEvaluator()
    ev.add('audit', 'slack.send_message')
    const decision = ev.evaluate('slack.send_message')
    expect(decision.blocked).toBe(false)
    expect(decision.policyKind).toBe('audit')
    expect(decision.pattern).toBe('slack.send_message')
  })
})
