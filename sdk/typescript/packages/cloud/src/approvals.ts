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

const DEFAULT_TIMEOUT_MS = 3_600_000
const DEFAULT_POLL_INTERVAL_MS = 5_000
const MAX_CONSECUTIVE_5XX = 3

function abortError(): Error {
  if (typeof DOMException === 'function') {
    return new DOMException('Aborted', 'AbortError')
  }
  const err = new Error('Aborted')
  ;(err as { name: string }).name = 'AbortError'
  return err
}

export async function pollUntilResolved(opts: PollOptions): Promise<string> {
  const timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS
  const pollIntervalMs = opts.pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS
  const headers: Record<string, string> = {
    Authorization: `Bearer ${opts.apiKey}`,
    'Content-Type': 'application/json',
  }

  // Create approval
  const createBody: Record<string, unknown> = { action: opts.action }
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
      const t = setTimeout(resolve, pollIntervalMs)
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
    if (data.status === 'rejected') throw new JamjetApprovalRejected(approvalId, data.reason)
    // else: pending — continue polling
  }
  throw new JamjetApprovalTimeout(approvalId, timeoutMs)
}
