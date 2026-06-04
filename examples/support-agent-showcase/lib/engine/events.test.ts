import { test, expect } from 'vitest'
import { type CostEvent, type GovEvent, type FeatureEvent } from './events.js'
test('cost + gov events are typed unions that narrow on kind', () => {
  const w: CostEvent = { kind: 'waste_detected', prefixHash: 'a1', repeats: 5, rePaidTokens: 16000, wastedCents: 4.2 }
  const s: CostEvent = { kind: 'cache_saved', savedCents: 3.1, cacheReadTokens: 12000 }
  const c: CostEvent = { kind: 'cost', cents: 0.9, model: 'claude-sonnet-4-6', inTok: 1000, outTok: 50 }
  const b: CostEvent = { kind: 'budget_exceeded', spentCents: 120, capCents: 100 }
  const g: GovEvent = { kind: 'redaction', type: 'US_SSN', count: 1 }
  const all: FeatureEvent[] = [w, s, c, b, g,
    { kind: 'policy_blocked', tool: 'issue_refund' },
    { kind: 'approval_required', id: 'ap1', tool: 'issue_refund' },
    { kind: 'approval_resolved', id: 'ap1', decision: 'approved' },
    { kind: 'audit', id: 'au1', tool: 'issue_refund' },
  ]
  expect(all.length).toBe(9)
  expect(w.kind === 'waste_detected' && w.rePaidTokens).toBe(16000)
})
