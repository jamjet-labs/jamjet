import { afterEach, describe, expect, test, vi } from 'vitest'
import { resetActive, getActive } from '../src/client.js'

afterEach(async () => {
  vi.resetModules()
  await resetActive()
})

describe('@jamjet/cloud/node', () => {
  test('init() returns without error when openai/anthropic not installed', async () => {
    vi.doMock('openai', () => { throw new Error('not installed') })
    vi.doMock('@anthropic-ai/sdk', () => { throw new Error('not installed') })
    const realFetch = globalThis.fetch
    globalThis.fetch = vi.fn(async () => new Response(null, { status: 200 })) as any
    try {
      const { init } = await import('../src/node.js')
      await init({ apiKey: 'k', project: 'p' })
      expect(getActive()).not.toBeNull()
    } finally {
      globalThis.fetch = realFetch
    }
  })

  test('re-exports universal API surface', async () => {
    const node = await import('../src/node.js')
    expect(typeof node.init).toBe('function')
    expect(typeof node.wrap).toBe('function')
    expect(typeof node.VERSION).toBe('string')
  })
})
