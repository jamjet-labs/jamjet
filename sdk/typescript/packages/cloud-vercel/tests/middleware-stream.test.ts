import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { JamjetPolicyBlocked } from '@jamjet/cloud'
import { createTestHarness, type TestHarness, setActive, resetActive } from '@jamjet/cloud/testing'
import { wrapLanguageModel } from 'ai'
import { jamjetMiddleware } from '../src/middleware.js'

function streamFromParts(parts: any[]): ReadableStream<any> {
  return new ReadableStream({
    start(controller) {
      for (const p of parts) controller.enqueue(p)
      controller.close()
    },
  })
}

describe('jamjetMiddleware wrapStream', () => {
  let harness: TestHarness

  beforeEach(async () => {
    harness = await createTestHarness()
    harness.budget.setLimit(100)
    setActive(harness.client)
  })
  afterEach(async () => {
    await harness.reset()
    await resetActive()
  })

  it('forwards text and tool-call parts unchanged when policy allows', async () => {
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({ content: [], usage: { inputTokens: 0, outputTokens: 0 }, finishReason: 'stop' as const }),
      doStream: async () => ({
        stream: streamFromParts([
          { type: 'stream-start', warnings: [] },
          { type: 'text', text: 'hello' },
          { type: 'tool-call', toolCallId: 'tc_1', toolName: 'search', args: '{}' },
          { type: 'finish', finishReason: 'tool-calls', usage: { inputTokens: 5, outputTokens: 2 } },
        ]),
        warnings: [],
      }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    const { stream } = await wrapped.doStream({
      prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
    } as any)
    const collected: any[] = []
    const reader = stream.getReader()
    while (true) {
      const { value, done } = await reader.read()
      if (done) break
      collected.push(value)
    }
    expect(collected.map((p) => p.type)).toEqual(['stream-start', 'text', 'tool-call', 'finish'])
  })

  it('throws JamjetPolicyBlocked mid-stream on blocked tool-call', async () => {
    harness.policy.add('block', 'wire_*')
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({ content: [], usage: { inputTokens: 0, outputTokens: 0 }, finishReason: 'stop' as const }),
      doStream: async () => ({
        stream: streamFromParts([
          { type: 'stream-start', warnings: [] },
          { type: 'text', text: 'thinking' },
          { type: 'tool-call', toolCallId: 'tc_1', toolName: 'wire_money', args: '{}' },
          { type: 'text', text: 'this should not be seen' },
          { type: 'finish', finishReason: 'tool-calls', usage: { inputTokens: 5, outputTokens: 2 } },
        ]),
        warnings: [],
      }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    const { stream } = await wrapped.doStream({
      prompt: [{ role: 'user', content: [{ type: 'text', text: 'wire it' }] }],
    } as any)
    const reader = stream.getReader()
    let thrownErr: unknown = null
    const collected: any[] = []
    try {
      while (true) {
        const { value, done } = await reader.read()
        if (done) break
        collected.push(value)
      }
    } catch (e) {
      thrownErr = e
    }
    expect(thrownErr).toBeInstanceOf(JamjetPolicyBlocked)
    // stream-start and text were forwarded BEFORE the blocked tool-call
    expect(collected.map((p) => p.type)).toEqual(['stream-start', 'text'])
  })

  it('records cost on finish part', async () => {
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({ content: [], usage: { inputTokens: 0, outputTokens: 0 }, finishReason: 'stop' as const }),
      doStream: async () => ({
        stream: streamFromParts([
          { type: 'finish', finishReason: 'stop', usage: { inputTokens: 1000, outputTokens: 500 } },
        ]),
        warnings: [],
      }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    const { stream } = await wrapped.doStream({
      prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
    } as any)
    const reader = stream.getReader()
    while (true) {
      const { done } = await reader.read()
      if (done) break
    }
    expect(harness.client._budget.spent).toBeGreaterThan(0)
  })
})
