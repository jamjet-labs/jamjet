import { describe, it, expect } from 'vitest'
import { applyCacheInject } from '../src/cache-inject.js'
import { CacheInjectResolver } from '../src/cache-inject.js'

describe('applyCacheInject', () => {
  it('wraps a string system prompt into a text block with cache_control', () => {
    const { mutated, injected } = applyCacheInject({
      model: 'claude-sonnet-4-6',
      system: 'You are a careful assistant.',
      messages: [{ role: 'user', content: 'hi' }],
    })
    expect(injected).toBe(true)
    expect(mutated.system).toEqual([
      { type: 'text', text: 'You are a careful assistant.', cache_control: { type: 'ephemeral' } },
    ])
  })

  it('falls back to the first user message when there is no system prompt', () => {
    const { mutated, injected } = applyCacheInject({
      model: 'claude-sonnet-4-6',
      messages: [{ role: 'user', content: 'long stable preamble' }],
    })
    expect(injected).toBe(true)
    const m0 = (mutated.messages as any[])[0]
    expect(m0.content).toEqual([
      { type: 'text', text: 'long stable preamble', cache_control: { type: 'ephemeral' } },
    ])
  })

  it('is idempotent — no double injection when cache_control already present', () => {
    const once = applyCacheInject({ system: 'x', messages: [] }).mutated
    const twice = applyCacheInject(once)
    expect(twice.injected).toBe(false)
    expect(twice.mutated.system).toEqual(once.system)
  })

  it('no-ops with neither system nor user message', () => {
    const { injected } = applyCacheInject({ messages: [] })
    expect(injected).toBe(false)
  })

  it('does not mutate the input object', () => {
    const input = { system: 'x', messages: [] }
    applyCacheInject(input)
    expect(input.system).toBe('x')
  })

  it('caches only the last block of an array system prompt', () => {
    const { mutated, injected } = applyCacheInject({
      system: [{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }],
      messages: [],
    })
    expect(injected).toBe(true)
    const blocks = mutated.system as any[]
    expect(blocks[0].cache_control).toBeUndefined()
    expect(blocks[1].cache_control).toEqual({ type: 'ephemeral' })
  })

  it('no-ops when neither system nor first user message is injectable', () => {
    expect(applyCacheInject({ system: 123 as unknown as string, messages: [] }).injected).toBe(false)
  })

  it('preserves non-text fields of the last block when caching an array message', () => {
    const { mutated, injected } = applyCacheInject({
      messages: [{ role: 'user', content: [
        { type: 'text', text: 'analyze this' },
        { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'AAAA' } },
      ] }],
    })
    expect(injected).toBe(true)
    const blocks = (mutated.messages as any[])[0].content as any[]
    expect(blocks[1].source).toEqual({ type: 'base64', media_type: 'image/png', data: 'AAAA' })
    expect(blocks[1].cache_control).toEqual({ type: 'ephemeral' })
    expect(blocks[0].cache_control).toBeUndefined()
  })
})

describe('CacheInjectResolver', () => {
  it('injects only for prefix hashes it was given', () => {
    const r = new CacheInjectResolver(['aaaa1111', 'bbbb2222'])
    expect(r.shouldInject('aaaa1111')).toBe(true)
    expect(r.shouldInject('zzzz9999')).toBe(false)
  })
  it('empty resolver never injects', () => {
    expect(new CacheInjectResolver([]).shouldInject('aaaa1111')).toBe(false)
  })
})
