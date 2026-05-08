import { describe, expect, it } from 'vitest'
import { GovernanceContext, type ScopeFrame } from '../src/context.js'

describe('GovernanceContext (ALS)', () => {
  it('runInContext propagates frame across awaits', async () => {
    const ctx = new GovernanceContext()
    const frame: ScopeFrame = { agent: { name: 'a1' } }
    const result = await ctx.runInContext(frame, async () => {
      await Promise.resolve()
      return ctx.getCurrentContext()
    })
    expect(result).toEqual(frame)
  })

  it('getCurrentContext returns null outside any scope', () => {
    const ctx = new GovernanceContext()
    expect(ctx.getCurrentContext()).toBeNull()
  })

  it('nested scopes restore parent frame on exit', async () => {
    const ctx = new GovernanceContext()
    const f1: ScopeFrame = { agent: { name: 'outer' } }
    const f2: ScopeFrame = { agent: { name: 'inner' } }
    let inner: ScopeFrame | null = null
    let outerAfter: ScopeFrame | null = null
    await ctx.runInContext(f1, async () => {
      await ctx.runInContext(f2, async () => {
        inner = ctx.getCurrentContext()
      })
      outerAfter = ctx.getCurrentContext()
    })
    expect(inner).toEqual(f2)
    expect(outerAfter).toEqual(f1)
  })

  it('returned frame merges agent + user from parent if not provided', async () => {
    const ctx = new GovernanceContext()
    const outer: ScopeFrame = { agent: { name: 'a' }, user: { userId: 'u1' } }
    const inner: ScopeFrame = { agent: { name: 'b' } } // no user
    let observed: ScopeFrame | null = null
    await ctx.runInContext(outer, async () => {
      await ctx.runInContext(inner, async () => {
        observed = ctx.getCurrentContext()
      })
    })
    expect(observed?.agent?.name).toBe('b')
    expect(observed?.user?.userId).toBe('u1')
  })
})

describe('GovernanceContext (no-ALS fallback)', () => {
  it('still runs fn synchronously when ALS unavailable', async () => {
    const ctx = new GovernanceContext({ alsAvailable: false })
    const frame: ScopeFrame = { agent: { name: 'a' } }
    let capturedInside: ScopeFrame | null = null
    await ctx.runInContext(frame, async () => {
      capturedInside = ctx.getCurrentContext()
    })
    // Synchronous capture works
    expect(capturedInside).toEqual(frame)
  })

  it('warns once across multiple calls when ALS unavailable', async () => {
    const warns: string[] = []
    const ctx = new GovernanceContext({
      alsAvailable: false,
      warn: (msg) => warns.push(msg),
    })
    await ctx.runInContext({ agent: { name: 'a' } }, async () => {})
    await ctx.runInContext({ agent: { name: 'b' } }, async () => {})
    expect(warns).toHaveLength(1)
    expect(warns[0]).toMatch(/AsyncLocalStorage unavailable/)
  })
})
