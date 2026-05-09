import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { JamjetBudgetExceeded } from '@jamjet/cloud'
import { createTestHarness, type TestHarness, setActive, resetActive } from '@jamjet/cloud/testing'
import { wrapLanguageModel } from 'ai'
import { jamjetMiddleware } from '../src/middleware.js'

describe('jamjetMiddleware wrapGenerate pre-call', () => {
  let harness: TestHarness

  beforeEach(async () => {
    harness = createTestHarness()
    harness.budget.setLimit(100)
    setActive(harness.client)
  })
  afterEach(async () => {
    await harness.reset()
    await resetActive()
  })

  it('strips blocked tools from params.tools before doGenerate', async () => {
    harness.policy.add('block', 'wire_*')
    let receivedParams: any
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async (params: any) => {
        receivedParams = params
        return {
          content: [{ type: 'text', text: 'ok' }],
          usage: { inputTokens: 10, outputTokens: 5 },
          finishReason: 'stop',
        }
      },
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    await wrapped.doGenerate({
      prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
      tools: [
        { type: 'function', name: 'search', inputSchema: { type: 'object' } },
        { type: 'function', name: 'wire_money', inputSchema: { type: 'object' } },
      ],
    } as any)
    expect(receivedParams.tools).toHaveLength(1)
    expect(receivedParams.tools[0].name).toBe('search')
  })

  it('throws JamjetBudgetExceeded pre-call (no doGenerate call)', async () => {
    // Set tight ceiling — already at limit
    harness.budget.setLimit(0.0001)
    harness.budget.record(0.0001)
    let calls = 0
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => {
        calls += 1
        return { content: [], usage: { inputTokens: 0, outputTokens: 0 }, finishReason: 'stop' as const }
      },
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    await expect(
      wrapped.doGenerate({
        prompt: [{ role: 'user', content: [{ type: 'text', text: 'x'.repeat(10_000) }] }],
      } as any),
    ).rejects.toBeInstanceOf(JamjetBudgetExceeded)
    expect(calls).toBe(0)
  })
})
