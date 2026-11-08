import { JamjetApprovalRejected, JamjetApprovalTimeout } from './errors.js'

export interface PollOptions {
  apiKey: string
  apiUrl: string
  action: string
  context?: Record<string, unknown>
  timeoutMs?: number
  pollIntervalMs?: number
  signal?: AbortSignal
}

export const DEFAULT_APPROVAL_TIMEOUT_MS = 3_600_000
const DEFAULT_POLL_INTERVAL_MS = 5_000
const MAX_CONSECUTIVE_5XX = 3
// The deadline range Cloud accepts for an approval (POST /v1/approvals
// `timeout_ms`, clamped server-side to [1 minute, 24 hours]). Cloud refuses a
// decision after the deadline, so waiting longer than it holds is pointless.
const CLOUD_MIN_TIMEOUT_MS = 60_000
const CLOUD_MAX_TIMEOUT_MS = 86_400_000

function abortError(): Error {
  if (typeof DOMException === 'function') {
    return new DOMException('Aborted', 'AbortError')
  }
  const err = new Error('Aborted')
  ;(err as { name: string }).name = 'AbortError'
  return err
}

/**
 * Create a Cloud approval and poll until it is decided.
 *
 * The SDK's timeout is sent as the approval's `timeout_ms`, so Cloud holds the
 * request exactly as long as the SDK waits for it. Values outside the range
 * Cloud accepts are clamped in what is sent, and the wait itself is capped at
 * Cloud's 24-hour maximum. A timeout under a minute still fails closed after
 * that time locally; Cloud just keeps the request open for its 1-minute floor.
 */
export async function pollUntilResolved(opts: PollOptions): Promise<string> {
  const timeoutMs = Math.min(opts.timeoutMs ?? DEFAULT_APPROVAL_TIMEOUT_MS, CLOUD_MAX_TIMEOUT_MS)
  const pollIntervalMs = opts.pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS
  const headers: Record<string, string> = {
    Authorization: `Bearer ${opts.apiKey}`,
    'Content-Type': 'application/json',
  }

  // Create approval
  const createBody: Record<string, unknown> = {
    action: opts.action,
    timeout_ms: Math.max(timeoutMs, CLOUD_MIN_TIMEOUT_MS),
  }
  if (opts.context !== undefined) createBody.context = opts.context
  const createRes = await fetch(`${opts.apiUrl}/v1/approvals`, {
    method: 'POST',
    headers,
    body: JSON.stringify(createBody),
    signal: opts.signal ?? null,
  })
  if (!createRes.ok) {
    throw new JamjetApprovalTimeout('', timeoutMs, { cause: 'create_failed' })
  }
  const created = (await createRes.json()) as { id: string }
  const approvalId = created.id

  const deadline = Date.now() + timeoutMs
  let consecutive5xx = 0
  while (Date.now() < deadline) {
    if (opts.signal?.aborted) throw abortError()

    await new Promise<void>((resolve, reject) => {
      const t = setTimeout(resolve, Math.max(0, Math.min(pollIntervalMs, deadline - Date.now())))
      opts.signal?.addEventListener('abort', () => {
        clearTimeout(t)
        reject(abortError())
      }, { once: true })
    })

    let res: Response
    try {
      res = await fetch(`${opts.apiUrl}/v1/approvals/${approvalId}`, {
        method: 'GET',
        headers,
        signal: opts.signal ?? null,
      })
    } catch (err) {
      if (opts.signal?.aborted) throw err
      // network blip: continue polling
      continue
    }

    if (res.status >= 500) {
      consecutive5xx += 1
      if (consecutive5xx >= MAX_CONSECUTIVE_5XX) {
        throw new JamjetApprovalTimeout(approvalId, timeoutMs, { cause: 'server_error' })
      }
      continue
    }

    if (res.status >= 400) {
      const body = (await res.json().catch(() => ({}))) as { reason?: string }
      throw new JamjetApprovalRejected(approvalId, body.reason)
    }

    consecutive5xx = 0
    const data = (await res.json()) as { status?: string; reason?: string }
    if (data.status === 'approved') return approvalId
    // Fail closed: any decided status other than approved (rejected, expired,
    // or one this SDK does not know) refuses the action.
    if (data.status !== undefined && data.status !== 'pending') {
      const reason = data.status === 'rejected' ? data.reason : (data.reason ?? data.status)
      throw new JamjetApprovalRejected(approvalId, reason)
    }
  }
  throw new JamjetApprovalTimeout(approvalId, timeoutMs)
}
