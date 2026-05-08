import { describe, expect, it } from 'vitest'
import { BudgetManager } from '../src/budget.js'
import { JamjetBudgetExceeded } from '../src/errors.js'

describe('BudgetManager', () => {
  it('with no limit, never throws', () => {
    const b = new BudgetManager()
    b.record(1_000_000)
    expect(() => b.checkOrThrow()).not.toThrow()
    expect(b.spent).toBe(1_000_000)
    expect(b.remaining).toBeNull()
  })

  it('record accumulates', () => {
    const b = new BudgetManager(10)
    b.record(2)
    b.record(3)
    expect(b.spent).toBe(5)
    expect(b.remaining).toBe(5)
  })

  it('checkOrThrow with estimatedCost throws when over', () => {
    const b = new BudgetManager(10)
    b.record(8)
    expect(() => b.checkOrThrow({ estimatedCost: 3 })).toThrow(JamjetBudgetExceeded)
  })

  it('checkOrThrow does not throw when within', () => {
    const b = new BudgetManager(10)
    b.record(5)
    expect(() => b.checkOrThrow({ estimatedCost: 3 })).not.toThrow()
  })

  it('thrown error carries spent and limit', () => {
    const b = new BudgetManager(10)
    b.record(8)
    try {
      b.checkOrThrow({ estimatedCost: 5 })
    } catch (e) {
      expect(e).toBeInstanceOf(JamjetBudgetExceeded)
      const err = e as JamjetBudgetExceeded
      expect(err.spent).toBe(8)
      expect(err.limit).toBe(10)
    }
  })

  it('failed pre-check leaves budget unchanged', () => {
    const b = new BudgetManager(10)
    b.record(8)
    expect(() => b.checkOrThrow({ estimatedCost: 5 })).toThrow()
    expect(b.spent).toBe(8) // didn't record the estimate
  })

  it('remaining clamps to zero', () => {
    const b = new BudgetManager(5)
    b.record(7)
    expect(b.remaining).toBe(0)
  })
})
