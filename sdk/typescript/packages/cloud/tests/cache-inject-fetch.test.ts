import { describe, it, expect, vi } from 'vitest'
import { fetchCacheInjectHashes } from '../src/cache-inject-fetch.js'

describe('fetchCacheInjectHashes', () => {
  it('returns prefix hashes (policy patterns) from the cloud', async () => {
    const f = vi.fn(async () => ({ ok: true, json: async () => ({ policies: [{ pattern: 'aaaa1111' }, { pattern: 'bbbb2222' }] }) })) as any
    const hashes = await fetchCacheInjectHashes({ apiBase: 'https://api.test', token: 'jj_t', fetchImpl: f })
    expect(hashes).toEqual(['aaaa1111', 'bbbb2222'])
    expect(f).toHaveBeenCalledWith('https://api.test/v1/policies?kind=cache_inject&active=true', expect.objectContaining({ headers: expect.any(Object) }))
  })
  it('returns [] and never throws on a failed fetch (fail-open)', async () => {
    const f = vi.fn(async () => { throw new Error('network') }) as any
    expect(await fetchCacheInjectHashes({ apiBase: 'https://api.test', token: 'jj_t', fetchImpl: f })).toEqual([])
  })
  it('returns [] when the response is not ok', async () => {
    const f = vi.fn(async () => ({ ok: false, json: async () => ({}) })) as any
    expect(await fetchCacheInjectHashes({ apiBase: 'https://api.test', token: 'jj_t', fetchImpl: f })).toEqual([])
  })
})
