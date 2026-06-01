import { test, expect } from 'vitest'
import { runTurn } from './run-turn.js'
import { resolveRefund } from './refund.js'
import { createSession } from '../session.js'
test('refund request opens an approval; approving writes an audit event', async () => {
  const s = createSession({ budgetCents: 1000, model: 'claude-sonnet-4-6' })
  const r = await runTurn(s, { text: 'please refund my last order' })
  const ev = r.events.find(e => e.kind === 'approval_required')
  expect(ev).toBeTruthy()
  const done = await resolveRefund(s, (ev as any).id, 'approved')
  expect(done.events.some(e => e.kind === 'approval_resolved' && (e as any).decision === 'approved')).toBe(true)
  expect(done.events.some(e => e.kind === 'audit')).toBe(true)
})
test('rejecting a refund does not write an audit event', async () => {
  const s = createSession({ budgetCents: 1000, model: 'claude-sonnet-4-6' })
  const r = await runTurn(s, { text: 'I want a refund now' })
  const ev = r.events.find(e => e.kind === 'approval_required')!
  const done = await resolveRefund(s, (ev as any).id, 'rejected')
  expect(done.events.some(e => e.kind === 'approval_resolved' && (e as any).decision === 'rejected')).toBe(true)
  expect(done.events.some(e => e.kind === 'audit')).toBe(false)
})
test('non-refund turns do not open an approval', async () => {
  const s = createSession({ budgetCents: 1000, model: 'claude-sonnet-4-6' })
  const r = await runTurn(s, { text: 'how do I reset my password?' })
  expect(r.events.some(e => e.kind === 'approval_required')).toBe(false)
})
