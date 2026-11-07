import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import { setupServer } from 'msw/node'
import { http, HttpResponse } from 'msw'
import { pollUntilResolved } from '../src/approvals.js'
import { JamjetApprovalRejected, JamjetApprovalTimeout } from '../src/errors.js'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())

const opts = {
  apiKey: 'jj_test',
  apiUrl: 'https://api.jamjet.test',
}

describe('pollUntilResolved', () => {
  beforeEach(() => {
    vi.useFakeTimers({ shouldAdvanceTime: true })
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('resolves on approved status', async () => {
    let polls = 0
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () =>
        HttpResponse.json({ id: 'apr_1' }),
      ),
      http.get('https://api.jamjet.test/v1/approvals/apr_1', () => {
        polls += 1
        return HttpResponse.json({ status: polls < 2 ? 'pending' : 'approved' })
      }),
    )
    const promise = pollUntilResolved({ ...opts, action: 'wire_money', pollIntervalMs: 100, timeoutMs: 60_000 })
    await vi.advanceTimersByTimeAsync(250)
    await expect(promise).resolves.toBe('apr_1')
  })

  it('rejects with JamjetApprovalRejected on rejected status', async () => {
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () => HttpResponse.json({ id: 'apr_2' })),
      http.get('https://api.jamjet.test/v1/approvals/apr_2', () =>
        HttpResponse.json({ status: 'rejected', reason: 'too risky' }),
      ),
    )
    const promise = pollUntilResolved({ ...opts, action: 'wire_money', pollIntervalMs: 100, timeoutMs: 60_000 })
    promise.catch(() => {})
    await vi.advanceTimersByTimeAsync(150)
    await expect(promise).rejects.toMatchObject({
      name: 'JamjetApprovalRejected',
      approvalId: 'apr_2',
      reason: 'too risky',
    })
  })

  it('throws JamjetApprovalTimeout after timeoutMs', async () => {
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () => HttpResponse.json({ id: 'apr_3' })),
      http.get('https://api.jamjet.test/v1/approvals/apr_3', () => HttpResponse.json({ status: 'pending' })),
    )
    const promise = pollUntilResolved({ ...opts, action: 'x', pollIntervalMs: 100, timeoutMs: 250 })
    promise.catch(() => {})
    await vi.advanceTimersByTimeAsync(400)
    await expect(promise).rejects.toBeInstanceOf(JamjetApprovalTimeout)
  })

  it('honors AbortSignal', async () => {
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () => HttpResponse.json({ id: 'apr_4' })),
      http.get('https://api.jamjet.test/v1/approvals/apr_4', () => HttpResponse.json({ status: 'pending' })),
    )
    const ac = new AbortController()
    const promise = pollUntilResolved({ ...opts, action: 'x', pollIntervalMs: 100, timeoutMs: 60_000, signal: ac.signal })
    promise.catch(() => {})
    await vi.advanceTimersByTimeAsync(50)
    ac.abort()
    await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
  })

  it('continues polling on single network blip', async () => {
    let polls = 0
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () => HttpResponse.json({ id: 'apr_5' })),
      http.get('https://api.jamjet.test/v1/approvals/apr_5', () => {
        polls += 1
        if (polls === 1) return HttpResponse.error()
        return HttpResponse.json({ status: 'approved' })
      }),
    )
    const promise = pollUntilResolved({ ...opts, action: 'x', pollIntervalMs: 100, timeoutMs: 60_000 })
    await vi.advanceTimersByTimeAsync(250)
    await expect(promise).resolves.toBe('apr_5')
  })

  it('fails closed on 3 consecutive 5xx', async () => {
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () => HttpResponse.json({ id: 'apr_6' })),
      http.get('https://api.jamjet.test/v1/approvals/apr_6', () =>
        HttpResponse.json({ error: 'oops' }, { status: 500 }),
      ),
    )
    const promise = pollUntilResolved({ ...opts, action: 'x', pollIntervalMs: 100, timeoutMs: 60_000 })
    promise.catch(() => {})
    await vi.advanceTimersByTimeAsync(400)
    await expect(promise).rejects.toMatchObject({
      name: 'JamjetApprovalTimeout',
      cause: 'server_error',
    })
  })
})

describe('pollUntilResolved: server deadline', () => {
  async function sentBody(timeoutMs: number): Promise<Record<string, unknown>> {
    let body: Record<string, unknown> = {}
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', async ({ request }) => {
        body = (await request.json()) as Record<string, unknown>
        return HttpResponse.json({ id: 'apr_t' })
      }),
      http.get('https://api.jamjet.test/v1/approvals/apr_t', () => HttpResponse.json({ status: 'approved' })),
    )
    await pollUntilResolved({ ...opts, action: 'x', pollIntervalMs: 10, timeoutMs })
    return body
  }

  it('sends the SDK timeout as timeout_ms so Cloud holds the request as long as the SDK waits', async () => {
    expect((await sentBody(120_000))['timeout_ms']).toBe(120_000)
  })

  it('clamps timeout_ms to the range Cloud accepts (1 minute to 24 hours)', async () => {
    expect((await sentBody(5_000))['timeout_ms']).toBe(60_000)
    expect((await sentBody(3 * 86_400_000))['timeout_ms']).toBe(86_400_000)
  })

  it('treats any decided status other than approved as a rejection', async () => {
    server.use(
      http.post('https://api.jamjet.test/v1/approvals', () => HttpResponse.json({ id: 'apr_x' })),
      http.get('https://api.jamjet.test/v1/approvals/apr_x', () => HttpResponse.json({ status: 'expired' })),
    )
    await expect(
      pollUntilResolved({ ...opts, action: 'x', pollIntervalMs: 10, timeoutMs: 60_000 }),
    ).rejects.toBeInstanceOf(JamjetApprovalRejected)
  })
})
