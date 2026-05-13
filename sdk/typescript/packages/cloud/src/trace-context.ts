// W3C trace-context reader.
//
// audit-event-v1's trace_id field is populated from any of:
//   1. A `traceparent` header on the adapter's incoming HTTP request.
//   2. The OTEL_TRACE_ID env var (Claude Code sets one when running an
//      instrumented session; OS bridges can populate it from the active
//      OTel span context too).
//
// Adapters call readTraceparent() with whatever they have. The result feeds
// the audit-event-v1 `trace_id` field unchanged. Downstream PRD 002 Traces
// page joins audit decisions to trace timelines via this id.
export interface Traceparent {
  version: string
  trace_id: string
  parent_id: string
  flags: string
}

const TRACEPARENT_RE =
  /^([0-9a-f]{2})-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$/i

export function parseTraceparent(
  s: string | undefined | null,
): Traceparent | null {
  if (typeof s !== 'string') return null
  const m = TRACEPARENT_RE.exec(s.trim())
  if (!m) return null
  // Capture groups are required by the regex, so m[1..4] are all defined.
  const version = m[1] as string
  const trace_id = m[2] as string
  const parent_id = m[3] as string
  const flags = m[4] as string
  if (version === 'ff') return null // W3C: reserved
  if (/^0+$/.test(trace_id)) return null
  if (/^0+$/.test(parent_id)) return null
  return {
    version,
    trace_id: trace_id.toLowerCase(),
    parent_id: parent_id.toLowerCase(),
    flags,
  }
}

export interface TraceContextSource {
  headers?: Record<string, string | string[] | undefined>
}

function pickHeader(
  headers: TraceContextSource['headers'],
  name: string,
): string | undefined {
  if (!headers) return undefined
  // node http req.headers lowercases names; some frameworks preserve the case
  // the caller sent. Check both.
  const lower = name.toLowerCase()
  for (const key of Object.keys(headers)) {
    if (key.toLowerCase() === lower) {
      const val = headers[key]
      return Array.isArray(val) ? val[0] : val
    }
  }
  return undefined
}

export function readTraceparent(
  source: TraceContextSource = {},
): Traceparent | null {
  const raw = pickHeader(source.headers, 'traceparent')
  const fromHeader = parseTraceparent(raw)
  if (fromHeader) return fromHeader

  const fromEnv = process.env.OTEL_TRACE_ID
  if (fromEnv && /^[0-9a-f]{32}$/i.test(fromEnv)) {
    return {
      version: '00',
      trace_id: fromEnv.toLowerCase(),
      parent_id: '0'.repeat(16),
      flags: '00',
    }
  }
  return null
}
