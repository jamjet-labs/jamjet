import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { createTestHarness, type TestHarness, setActive, resetActive } from '@jamjet/cloud/testing'
import { trace } from '@opentelemetry/api'
import { BasicTracerProvider } from '@opentelemetry/sdk-trace-base'
import { registerJamjetTelemetry } from '../src/telemetry.js'

describe('registerJamjetTelemetry', () => {
  let harness: TestHarness | null = null
  let provider: BasicTracerProvider

  beforeEach(async () => {
    provider = new BasicTracerProvider()
    trace.setGlobalTracerProvider(provider as any)
  })
  afterEach(async () => {
    if (harness) {
      await harness.reset()
      harness = null
    }
    await resetActive()
    trace.disable()
  })

  it('throws when called before init()', () => {
    expect(() => registerJamjetTelemetry()).toThrow(/not initialized/)
  })

  it('adds a SpanProcessor to the global tracer provider', async () => {
    harness = await createTestHarness()
    setActive(harness.client)
    const before = (provider as any)._registeredSpanProcessors?.length ?? 0
    registerJamjetTelemetry()
    const after = (provider as any)._registeredSpanProcessors?.length ?? 0
    expect(after).toBe(before + 1)
  })

  it('is idempotent (second call is no-op)', async () => {
    harness = await createTestHarness()
    setActive(harness.client)
    registerJamjetTelemetry()
    const after1 = (provider as any)._registeredSpanProcessors?.length ?? 0
    registerJamjetTelemetry()
    const after2 = (provider as any)._registeredSpanProcessors?.length ?? 0
    expect(after2).toBe(after1)
  })
})
