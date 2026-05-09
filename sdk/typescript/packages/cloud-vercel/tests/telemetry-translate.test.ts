import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { createTestHarness, type TestHarness, setActive, resetActive } from '@jamjet/cloud/testing'
import { translateAISDKSpan } from '../src/translate.js'

function fakeOtelSpan(opts: {
  name: string
  attributes: Record<string, unknown>
  startMs?: number
  endMs?: number
  status?: 'ok' | 'error'
}): any {
  const start = opts.startMs ?? Date.now() - 100
  const end = opts.endMs ?? Date.now()
  return {
    name: opts.name,
    attributes: opts.attributes,
    startTime: [Math.floor(start / 1000), (start % 1000) * 1_000_000],
    endTime: [Math.floor(end / 1000), (end % 1000) * 1_000_000],
    status: { code: opts.status === 'error' ? 2 : 1 },
    spanContext: () => ({ traceId: 'a'.repeat(32), spanId: 'b'.repeat(16) }),
  }
}

describe('translateAISDKSpan', () => {
  let harness: TestHarness
  beforeEach(async () => {
    harness = await createTestHarness()
    setActive(harness.client)
  })
  afterEach(async () => {
    await harness.reset()
    await resetActive()
  })

  it('translates ai.generateText span with usage', () => {
    const otel = fakeOtelSpan({
      name: 'ai.generateText',
      attributes: {
        'ai.model.id': 'gpt-4o',
        'ai.model.provider': 'openai',
        'ai.usage.promptTokens': 100,
        'ai.usage.completionTokens': 50,
        'ai.response.finishReason': 'stop',
      },
    })
    const dict = translateAISDKSpan(otel, harness.client) as Record<string, unknown>
    expect(dict.kind).toBe('llm_call')
    expect(dict.name).toBe('ai.generateText')
    expect(dict.model).toBe('gpt-4o')
    expect(dict.input_tokens).toBe(100)
    expect(dict.output_tokens).toBe(50)
    expect(dict.source).toBe('otel')
    expect(dict.cost_usd).toBeGreaterThan(0)
  })

  it('handles ai.usage.inputTokens (newer attr name) as fallback', () => {
    const otel = fakeOtelSpan({
      name: 'ai.streamText',
      attributes: {
        'ai.model.id': 'gpt-4o',
        'ai.usage.inputTokens': 200,
        'ai.usage.outputTokens': 100,
      },
    })
    const dict = translateAISDKSpan(otel, harness.client) as Record<string, unknown>
    expect(dict.input_tokens).toBe(200)
    expect(dict.output_tokens).toBe(100)
  })

  it('propagates ai.telemetry.functionId', () => {
    const otel = fakeOtelSpan({
      name: 'ai.generateText',
      attributes: {
        'ai.model.id': 'gpt-4o',
        'ai.usage.promptTokens': 10,
        'ai.usage.completionTokens': 5,
        'ai.telemetry.functionId': 'summarise-doc',
      },
    })
    const dict = translateAISDKSpan(otel, harness.client) as Record<string, unknown>
    expect(dict.ai_sdk_function_id).toBe('summarise-doc')
  })

  it('returns null for spans missing both usage attribute names', () => {
    const otel = fakeOtelSpan({
      name: 'ai.generateText',
      attributes: { 'ai.model.id': 'gpt-4o' },
    })
    const dict = translateAISDKSpan(otel, harness.client)
    expect(dict).toBeNull()
  })
})
