import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { createTestHarness, setActive, resetActive, type TestHarness } from '@jamjet/cloud/testing'
import { jamjetMiddleware } from '../src/middleware.js'

describe('jamjetMiddleware before init()', () => {
  it('factory call does NOT throw (allows module-load-order setups)', () => {
    expect(() => jamjetMiddleware()).not.toThrow()
  })

  it('wrapGenerate throws on first invocation when not initialized', async () => {
    const mw = jamjetMiddleware()
    const stub = async () => ({
      content: [],
      usage: { inputTokens: 0, outputTokens: 0 },
      finishReason: 'stop' as const,
    })
    await expect(
      mw.wrapGenerate!({
        doGenerate: stub as any,
        params: { prompt: [], inputFormat: 'messages' } as any,
        model: {} as any,
      } as any),
    ).rejects.toThrow(/not initialized/)
  })

  it('wrapStream throws on first invocation when not initialized', async () => {
    const mw = jamjetMiddleware()
    const stub = async () => ({ stream: new ReadableStream(), warnings: [] })
    await expect(
      mw.wrapStream!({
        doStream: stub as any,
        params: { prompt: [], inputFormat: 'messages' } as any,
        model: {} as any,
      } as any),
    ).rejects.toThrow(/not initialized/)
  })
})

describe('jamjetMiddleware after init()', () => {
  let harness: TestHarness
  beforeEach(async () => {
    harness = createTestHarness()
    setActive(harness.client)
  })
  afterEach(async () => {
    await harness.reset()
    await resetActive()
  })

  it('wrapGenerate does not throw not-initialized when active client exists', async () => {
    const mw = jamjetMiddleware()
    const stub = async () => ({
      content: [{ type: 'text' as const, text: 'ok' }],
      usage: { inputTokens: 1, outputTokens: 1 },
      finishReason: 'stop' as const,
    })
    // We just need this NOT to throw the not-initialized error.
    // Tasks 3-4 add real enforcement. For now skeleton passes through.
    await expect(
      mw.wrapGenerate!({
        doGenerate: stub as any,
        params: { prompt: [], inputFormat: 'messages' } as any,
        model: { modelId: 'fake' } as any,
      } as any),
    ).resolves.toBeDefined()
  })
})
