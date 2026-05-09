import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { JamjetBudgetExceeded, JamjetPolicyBlocked } from '@jamjet/cloud'
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

describe('jamjetMiddleware wrapGenerate post-decision', () => {
  let harness: TestHarness
  let capturedEvents: any[]

  beforeEach(() => {
    harness = createTestHarness()
    harness.budget.setLimit(100)
    capturedEvents = []
    // Override recordSpan to capture synchronously (bypasses batcher flush delay)
    ;(harness.client as any).recordSpan = (e: any) => {
      capturedEvents.push(e)
    }
    setActive(harness.client)
  })
  afterEach(async () => {
    await harness.reset()
    await resetActive()
  })

  it('throws JamjetPolicyBlocked when result contains a blocked tool-call', async () => {
    harness.policy.add('block', 'wire_*')
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({
        content: [
          { type: 'tool-call', toolCallId: 'tc_1', toolName: 'wire_money', input: '{}' },
        ],
        usage: { inputTokens: 5, outputTokens: 2 },
        finishReason: 'tool-calls',
      }),
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    await expect(
      wrapped.doGenerate({
        prompt: [{ role: 'user', content: [{ type: 'text', text: 'wire it' }] }],
        tools: [],
      } as any),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('records actual cost post-call', async () => {
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({
        content: [{ type: 'text', text: 'hi' }],
        usage: { inputTokens: 1000, outputTokens: 500 },
        finishReason: 'stop',
      }),
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    await wrapped.doGenerate({
      prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
    } as any)
    expect(harness.client._budget.spent).toBeGreaterThan(0)
  })

  it('emits span with source=middleware and identity from context', async () => {
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({
        content: [{ type: 'text', text: 'ok' }],
        usage: { inputTokens: 10, outputTokens: 5 },
        finishReason: 'stop',
      }),
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    await harness.client._governanceContext.runInContext(
      { agent: { name: 'researcher' }, user: { userId: 'u_42' } },
      async () => {
        await wrapped.doGenerate({
          prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
        } as any)
      },
    )
    const middlewareSpan = capturedEvents.find((e: any) => e.source === 'middleware')
    expect(middlewareSpan).toBeDefined()
    expect(middlewareSpan.agent_name).toBe('researcher')
    expect(middlewareSpan.user_id).toBe('u_42')
  })
})
