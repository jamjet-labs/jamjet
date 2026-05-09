import { getActive } from '@jamjet/cloud'
import { trace } from '@opentelemetry/api'
import type { ReadableSpan, SpanProcessor } from '@opentelemetry/sdk-trace-base'
import { translateAISDKSpan } from './translate.js'

type Client = ReturnType<typeof getActive> & object

const DEFAULT_SPAN_NAMES = [
  'ai.generateText',
  'ai.generateText.doGenerate',
  'ai.streamText',
  'ai.streamText.doStream',
  'ai.generateObject',
  'ai.generateObject.doGenerate',
  'ai.streamObject',
  'ai.streamObject.doStream',
  'ai.embed',
  'ai.embedMany',
  'ai.toolCall',
]

const REGISTERED_FLAG = Symbol.for('jamjet.cloud-vercel.telemetry-registered')

/** Options for `registerJamjetTelemetry`. Both fields are optional and have
 *  sensible defaults (built-in AI SDK span names, auto-created span processor). */
export interface RegisterTelemetryOptions {
  spanProcessor?: SpanProcessor
  spanNames?: string[]
}

class JamjetSpanProcessor implements SpanProcessor {
  constructor(private readonly client: Client, private readonly spanNames: Set<string>) {}

  onStart(): void {
    // no-op
  }

  onEnd(span: ReadableSpan): void {
    if (!this.spanNames.has(span.name)) return
    try {
      const dict = translateAISDKSpan(span as any, this.client)
      if (dict) this.client.recordSpan(dict)
    } catch (err) {
      if (this.client.config.debug) {
        console.warn('[jamjet-vercel] telemetry translate failed:', err)
      }
    }
  }

  async forceFlush(): Promise<void> {
    // no-op; client batcher flushes independently
  }

  async shutdown(): Promise<void> {
    // no-op; client lifecycle is managed separately
  }
}

/**
 * Registers a JamJet Cloud span processor on the global OpenTelemetry
 * `TracerProvider`. Translates AI SDK spans into JamJet `SpanEventDict` records
 * and forwards them to the active client's batcher.
 *
 * Idempotent — safe to call multiple times; only the first call takes effect.
 * Requires `init()` from `@jamjet/cloud` to have been called.
 */
export function registerJamjetTelemetry(opts: RegisterTelemetryOptions = {}): void {
  const client = getActive()
  if (!client) throw new Error('JamJet Cloud not initialized. Call init() first.')

  // trace.getTracerProvider() returns a ProxyTracerProvider wrapper; unwrap to the real provider
  // if it supports getDelegate() (standard OTel SDK pattern).
  const raw = trace.getTracerProvider() as any
  const provider: any =
    typeof raw.getDelegate === 'function' ? (raw.getDelegate() ?? raw) : raw

  if (provider[REGISTERED_FLAG]) {
    if (client.config.debug) console.debug('[jamjet-vercel] telemetry already registered')
    return
  }

  const processor =
    opts.spanProcessor ??
    new JamjetSpanProcessor(client, new Set(opts.spanNames ?? DEFAULT_SPAN_NAMES))

  if (typeof provider.addSpanProcessor === 'function') {
    provider.addSpanProcessor(processor)
  } else {
    throw new Error(
      'global TracerProvider has no addSpanProcessor; pass a custom one or use a BasicTracerProvider',
    )
  }
  provider[REGISTERED_FLAG] = true
}
