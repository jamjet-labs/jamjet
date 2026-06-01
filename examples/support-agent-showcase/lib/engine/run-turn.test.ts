import { test, expect } from 'vitest'
import { runTurn } from './run-turn.js'
import { createSession } from '../session.js'
test('detect waste on repeats, prevent with cache_inject, redact PII', async () => {
  const s = createSession({ budgetCents: 1000, model: 'claude-sonnet-4-6' })
  for (let i = 0; i < 5; i++) await runTurn(s, { text: `support question number ${i}` })
  const waste = s.tracker.detect()
  expect(waste.length).toBe(1)            // same KB prefix every turn -> grouped
  expect(waste[0].repeats).toBe(5)
  s.setCacheInject(true)
  const warm = await runTurn(s, { text: 'another support question' })
  expect(warm.events.some(e => e.kind === 'cache_saved')).toBe(true)   // prevention
  const pii = await runTurn(s, { text: 'my ssn is 123-45-6789 please help' })
  expect(pii.events.some(e => e.kind === 'redaction')).toBe(true)       // governance
  expect(pii.reply).not.toContain('123-45-6789')
})
test('every turn emits a cost event; budget cap blocks further calls', async () => {
  const s = createSession({ budgetCents: 0.5, model: 'claude-sonnet-4-6' })  // tiny cap (~one call)
  const first = await runTurn(s, { text: 'hello' })
  expect(first.events.some(e => e.kind === 'cost')).toBe(true)
  // keep going until the cap trips, then a turn is blocked pre-call
  let blocked = false
  for (let i = 0; i < 5 && !blocked; i++) {
    const r = await runTurn(s, { text: `q${i}` })
    if (r.events.some(e => e.kind === 'budget_exceeded')) blocked = true
  }
  expect(blocked).toBe(true)
})
