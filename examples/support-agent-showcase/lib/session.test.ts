import { test, expect } from 'vitest'
import { createSession } from './session.js'
test('tracks spend against a budget cap (cents)', () => {
  const s = createSession({ budgetCents: 10, model: 'claude-sonnet-4-6' })
  expect(s.addSpend(6)).toBe(false)   // 6 <= 10
  expect(s.spentCents).toBe(6)
  expect(s.addSpend(6)).toBe(true)    // 12 > 10 -> exceeded
  expect(s.spentCents).toBe(12)
})
test('cache-inject toggle', () => {
  const s = createSession({ budgetCents: 100, model: 'claude-sonnet-4-6' })
  expect(s.cacheInjectOn).toBe(false)
  s.setCacheInject(true)
  expect(s.cacheInjectOn).toBe(true)
})
test('pending approval open -> resolve roundtrip', () => {
  const s = createSession({ budgetCents: 100, model: 'claude-sonnet-4-6' })
  const id = s.openApproval('issue_refund')
  expect(typeof id).toBe('string')
  expect(s.pendingApproval(id)?.tool).toBe('issue_refund')
  expect(s.resolveApproval(id, 'approved')).toBe(true)
  expect(s.pendingApproval(id)).toBeUndefined()   // resolved -> no longer pending
  expect(s.resolveApproval('nope', 'approved')).toBe(false)
})
test('exposes a WasteTracker bound to the model', () => {
  const s = createSession({ budgetCents: 100, model: 'claude-sonnet-4-6' })
  s.tracker.record('a1b2c3d4e5f6a7b8', 4000); s.tracker.record('a1b2c3d4e5f6a7b8', 4000)
  expect(s.tracker.detect().length).toBe(1)
})
