import type { SpanEventDict } from '@jamjet/cloud'
import { estimateCost, getActive, Span } from '@jamjet/cloud'

type Client = ReturnType<typeof getActive> & object

interface OtelSpanLike {
  name: string
  attributes: Record<string, unknown>
  startTime: [number, number]
  endTime: [number, number]
  status: { code: number }
  spanContext(): { traceId: string; spanId: string }
}

function hrTimeToMs(t: [number, number]): number {
  return t[0] * 1000 + t[1] / 1_000_000
}

function num(v: unknown): number | null {
  if (typeof v === 'number' && Number.isFinite(v)) return v
  if (typeof v === 'string') {
    const n = Number(v)
    return Number.isFinite(n) ? n : null
  }
  return null
}

function str(v: unknown): string | null {
  return typeof v === 'string' ? v : null
}

export function translateAISDKSpan(otel: OtelSpanLike, client: Client): SpanEventDict | null {
  const attrs = otel.attributes
  const inputTokens =
    num(attrs['ai.usage.promptTokens']) ??
    num(attrs['ai.usage.inputTokens']) ??
    num(attrs['gen_ai.usage.input_tokens'])
  const outputTokens =
    num(attrs['ai.usage.completionTokens']) ??
    num(attrs['ai.usage.outputTokens']) ??
    num(attrs['gen_ai.usage.output_tokens'])
  if (inputTokens === null || outputTokens === null) {
    return null
  }
  const modelId = str(attrs['ai.model.id']) ?? str(attrs['gen_ai.response.model']) ?? 'unknown'

  const ctx = otel.spanContext()
  const span = new Span({
    traceId: ctx.traceId.slice(0, 16),
    spanId: ctx.spanId,
    kind: 'llm_call',
    name: otel.name,
  })
  span.model = modelId
  span.inputTokens = inputTokens
  span.outputTokens = outputTokens
  span.costUsd = estimateCost(modelId, inputTokens, outputTokens)
  if (client.config.agent) span.agentName = client.config.agent
  if (client.config.environment) span.environment = client.config.environment
  if (client.config.releaseVersion) span.releaseVersion = client.config.releaseVersion
  span.finish(otel.status.code === 2 ? 'error' : 'ok')

  const dict = span.toEventDict() as Record<string, unknown>
  dict.start_time_ms = hrTimeToMs(otel.startTime)
  dict.end_time_ms = hrTimeToMs(otel.endTime)
  dict.source = 'otel'
  const fnId = str(attrs['ai.telemetry.functionId']) ?? str(attrs['resource.name'])
  if (fnId) dict.ai_sdk_function_id = fnId
  // Pull ai.telemetry.metadata.* into a sub-object
  const metadata: Record<string, unknown> = {}
  for (const k of Object.keys(attrs)) {
    if (k.startsWith('ai.telemetry.metadata.')) {
      metadata[k.slice('ai.telemetry.metadata.'.length)] = attrs[k]
    }
  }
  if (Object.keys(metadata).length > 0) dict.ai_sdk_metadata = metadata
  const finish = str(attrs['ai.response.finishReason'])
  if (finish) (dict as any).payload = { ...(dict as any).payload, finishReason: finish }
  return dict as SpanEventDict
}
