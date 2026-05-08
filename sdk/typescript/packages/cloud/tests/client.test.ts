import { afterEach, describe, expect, it, test } from 'vitest'
import { Client, getActive, resetActive, setActive } from '../src/client.js'
import { BudgetManager } from '../src/budget.js'
import { GovernanceContext } from '../src/context.js'
import { PolicyEvaluator } from '../src/policy.js'
import type { ResolvedConfig } from '../src/config.js'
import { resolveConfig } from '../src/config.js'

const cfg: ResolvedConfig = {
  apiKey: 'k',
  apiUrl: 'https://api.jamjet.dev',
  project: 'p',
  agent: 'default',
  sampling: { rate: 1, alwaysKeepErrors: true, alwaysKeepApprovals: true },
  redaction: { mode: 'standard', custom: [] },
  debug: false,
}

describe('Client state', () => {
  afterEach(() => resetActive())

  test('getActive returns null before init', () => {
    expect(getActive()).toBeNull()
  })

  test('setActive stores client', () => {
    const c = new Client(cfg)
    setActive(c)
    expect(getActive()).toBe(c)
  })

  test('resetActive clears client and stops batcher', async () => {
    const c = new Client(cfg)
    setActive(c)
    await resetActive()
    expect(getActive()).toBeNull()
  })

  test('Client.recordSpan adds to batcher', () => {
    const c = new Client(cfg)
    c.recordSpan({ type: 'span', trace_id: 't', span_id: 's' } as any)
    expect(c.batcher).toBeDefined()
  })
})

describe('Client Plan 2 fields', () => {
  it('exposes _policy, _budget, _governanceContext', () => {
    const config = resolveConfig({ apiKey: 'jj_test', project: 'p' })
    const c = new Client(config)
    expect(c._policy).toBeInstanceOf(PolicyEvaluator)
    expect(c._budget).toBeInstanceOf(BudgetManager)
    expect(c._governanceContext).toBeInstanceOf(GovernanceContext)
  })

  it('honors maxCostUsd in config to seed budget', () => {
    const config = resolveConfig({ apiKey: 'jj_test', project: 'p', maxCostUsd: 25 })
    const c = new Client(config)
    expect(c._budget.remaining).toBe(25)
  })

  it('budget without maxCostUsd has null remaining', () => {
    const config = resolveConfig({ apiKey: 'jj_test', project: 'p' })
    const c = new Client(config)
    expect(c._budget.remaining).toBeNull()
  })
})
