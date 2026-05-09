import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { createTestHarness, type TestHarness, setActive, resetActive } from '@jamjet/cloud/testing'
import { trace, type Tracer } from '@opentelemetry/api'
import { BasicTracerProvider } from '@opentelemetry/sdk-trace-base'
import { wrapLanguageModel } from 'ai'
import { jamjetMiddleware } from '../src/middleware.js'
import { registerJamjetTelemetry } from '../src/telemetry.js'

describe('composition: middleware + telemetry exporter', () => {
  let harness: TestHarness
  let provider: BasicTracerProvider
  let tracer: Tracer
  let captured: any[]

  beforeEach(async () => {
    harness = await createTestHarness()
    harness.budget.setLimit(100)
    captured = []
    // Override recordSpan synchronously before setActive so all captured events land in `captured`
    ;(harness.client as any).recordSpan = (e: any) => captured.push(e)
    setActive(harness.client)
    // Instantiate and register the global provider BEFORE calling registerJamjetTelemetry so the
    // JamjetSpanProcessor is attached to the same provider instance that issues spans.
    provider = new BasicTracerProvider()
    trace.setGlobalTracerProvider(provider as any)
    registerJamjetTelemetry()
    tracer = trace.getTracer('jamjet-test')
  })

  afterEach(async () => {
    await harness.reset()
    await resetActive()
    trace.disable()
  })

  it('emits both middleware and otel spans for same call (different sources)', async () => {
    // 1. Middleware path: wrap a fake model and call doGenerate
    const fakeModel = {
      specificationVersion: 'v2',
      provider: 'test',
      modelId: 'gpt-4o',
      doGenerate: async () => ({
        content: [{ type: 'text', text: 'ok' }],
        usage: { inputTokens: 5, outputTokens: 2 },
        finishReason: 'stop',
      }),
      doStream: async () => ({ stream: new ReadableStream(), warnings: [] }),
    }
    const wrapped = wrapLanguageModel({ model: fakeModel as any, middleware: jamjetMiddleware() })
    await wrapped.doGenerate({
      prompt: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
    } as any)

    // 2. OTel path: emit a span that matches DEFAULT_SPAN_NAMES with required usage attrs so
    //    translateAISDKSpan returns a non-null dict and recordSpan is called.
    const otelSpan = tracer.startSpan('ai.generateText', {
      attributes: {
        'ai.model.id': 'gpt-4o',
        'ai.usage.promptTokens': 10,
        'ai.usage.completionTokens': 5,
      },
    })
    otelSpan.end()

    // 3. Force flush to ensure all processors have processed their queued spans.
    await provider.forceFlush()

    // 4. Assert: both sources are present in captured spans
    const sources = captured.map((s) => s.source).sort()
    expect(sources).toContain('middleware')
    expect(sources).toContain('otel')
    expect(captured.length).toBeGreaterThanOrEqual(2)
  })
})
