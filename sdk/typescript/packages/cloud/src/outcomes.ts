const VALID_OUTCOMES = new Set([
  'success',
  'failure',
  'approved',
  'rejected',
  'resolved',
  'unresolved',
] as const)

export type Outcome =
  | 'success'
  | 'failure'
  | 'approved'
  | 'rejected'
  | 'resolved'
  | 'unresolved'

export interface RecordOutcomeOptions {
  score?: number
  metadata?: Record<string, unknown>
}

function isValidOutcome(value: string): value is Outcome {
  return VALID_OUTCOMES.has(value as Outcome)
}

/**
 * Record the outcome of a trace run.
 *
 * POSTs to `POST /v1/outcomes` on the JamJet Cloud API.
 *
 * @param apiKey   Bearer token.
 * @param apiUrl   Base URL (e.g. `https://api.jamjet.dev`).
 * @param traceId  The trace whose outcome is being recorded.
 * @param outcome  One of the six outcome values.
 * @param opts     Optional `score` (0–1) and free-form `metadata`.
 */
export async function recordOutcome(
  apiKey: string,
  apiUrl: string,
  traceId: string,
  outcome: Outcome,
  opts: RecordOutcomeOptions = {},
): Promise<void> {
  if (!isValidOutcome(outcome)) {
    throw new TypeError(
      `Invalid outcome "${outcome}". Must be one of: ${[...VALID_OUTCOMES].join(', ')}.`,
    )
  }
  if (opts.score !== undefined) {
    if (typeof opts.score !== 'number' || opts.score < 0 || opts.score > 1) {
      throw new RangeError(
        `score must be a number between 0 and 1 (inclusive), got ${opts.score}.`,
      )
    }
  }

  const body: Record<string, unknown> = { trace_id: traceId, outcome }
  if (opts.score !== undefined) body.score = opts.score
  if (opts.metadata !== undefined) body.metadata = opts.metadata

  const res = await fetch(`${apiUrl}/v1/outcomes`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${apiKey}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify(body),
  })

  if (!res.ok) {
    throw new Error(`JamJet recordOutcome failed: ${res.status} ${res.statusText}`)
  }
}
