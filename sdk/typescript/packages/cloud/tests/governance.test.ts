import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { getActive, resetActive } from '../src/client.js'
import {
  agent,
  budget,
  policy,
  setProcessContext,
  setUserContext,
  withAgent,
  withUserContext,
} from '../src/governance.js'
import { init } from '../src/init.js'

describe('agent()', () => {
  it('returns frozen AgentRef with name', () => {
    const ref = agent('researcher', { cardUri: 'https://x/agent', description: 'r' })
    expect(ref.name).toBe('researcher')
    expect(ref.cardUri).toBe('https://x/agent')
    expect(ref.description).toBe('r')
    expect(Object.isFrozen(ref)).toBe(true)
  })

  it('rejects empty name', () => {
    expect(() => agent('')).toThrow(/empty/i)
    expect(() => agent('   ')).toThrow(/empty/i)
  })

  it('trims whitespace', () => {
    expect(agent('  bot  ').name).toBe('bot')
  })
})

describe('not-initialized errors', () => {
  beforeEach(async () => {
    await resetActive()
  })

  it('policy() throws before init', () => {
    expect(() => policy('block', '*')).toThrow(/not initialized/)
  })

  it('budget() throws before init', () => {
    expect(() => budget(10)).toThrow(/not initialized/)
  })

  it('withAgent throws before init', async () => {
    await expect(withAgent(agent('x'), async () => 1)).rejects.toThrow(/not initialized/)
  })
})

describe('after init', () => {
  beforeEach(async () => {
    await resetActive()
    await init({ apiKey: 'jj_test', project: 'p' })
  })

  afterEach(async () => {
    await resetActive()
  })

  it('policy() registers a rule on the active client', () => {
    policy('block', 'wire_*')
    const c = getActive()!
    expect(c._policy.evaluate('wire_money').blocked).toBe(true)
  })

  it('budget() updates the active client budget', () => {
    budget(99)
    const c = getActive()!
    expect(c._budget.remaining).toBe(99)
  })

  it('withAgent propagates ref across awaits', async () => {
    const ref = agent('researcher')
    const result = await withAgent(ref, async () => {
      await Promise.resolve()
      return getActive()!._governanceContext.getCurrentContext()
    })
    expect(result?.agent?.name).toBe('researcher')
  })

  it('withUserContext propagates user across awaits', async () => {
    const result = await withUserContext({ userId: 'u_42', email: 'a@b.co' }, async () => {
      await Promise.resolve()
      return getActive()!._governanceContext.getCurrentContext()
    })
    expect(result?.user?.userId).toBe('u_42')
  })

  it('setUserContext sets process-level user', () => {
    setUserContext({ userId: 'admin' })
    expect(getActive()!._governanceContext.getCurrentContext()?.user?.userId).toBe('admin')
  })

  it('setProcessContext stores environment + releaseVersion', () => {
    setProcessContext({ environment: 'staging', releaseVersion: '2.0.0' })
    expect(getActive()!.config.environment).toBe('staging')
    expect((getActive()!.config as any).releaseVersion).toBe('2.0.0')
  })
})
