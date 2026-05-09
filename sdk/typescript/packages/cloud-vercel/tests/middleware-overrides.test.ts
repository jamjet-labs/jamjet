import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { agent } from '@jamjet/cloud'
import { createTestHarness, type TestHarness, setActive, resetActive } from '@jamjet/cloud/testing'
import { wrapLanguageModel } from 'ai'
import { jamjetMiddleware } from '../src/middleware.js'

describe('jamjetMiddleware identity overrides', () => {
  let harness: TestHarness
  let captured: any[]

  beforeEach(() => {
    harness = createTestHarness()
    harness.budget.setLimit(100)
    captured = []
    ;(harness.client as any).recordSpan = (e: any) => captured.push(e)
    setActive(harness.client)
  })
  afterEach(async () => {
    await harness.reset()
    await resetActive()
  })

  it('opts.agent beats ALS context agent', async () => {
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({
        content: [{ type: 'text', text: 'ok' }],
        usage: { inputTokens: 1, outputTokens: 1 },
        finishReason: 'stop',
      }),
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({
      model: fakeModel as any,
      middleware: jamjetMiddleware({ agent: agent('explicit_agent') }),
    })
    await harness.client._governanceContext.runInContext(
      { agent: { name: 'context_agent' } },
      async () => {
        await wrapped.doGenerate({
          prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
        } as any)
      },
    )
    const recorded = captured.find((e: any) => e.source === 'middleware')
    expect(recorded).toBeDefined()
    expect(recorded.agent_name).toBe('explicit_agent')
  })

  it('opts.user beats ALS context user', async () => {
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({
        content: [],
        usage: { inputTokens: 1, outputTokens: 1 },
        finishReason: 'stop',
      }),
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({
      model: fakeModel as any,
      middleware: jamjetMiddleware({ user: { userId: 'override_user' } }),
    })
    await harness.client._governanceContext.runInContext(
      { user: { userId: 'context_user' } },
      async () => {
        await wrapped.doGenerate({
          prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
        } as any)
      },
    )
    const recorded = captured.find((e: any) => e.source === 'middleware')
    expect(recorded).toBeDefined()
    expect(recorded.user_id).toBe('override_user')
  })
})
