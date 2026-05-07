import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, test, vi } from 'vitest'
import { Transport } from '../src/transport.js'
import {
  flakyHandler,
  rateLimitHandler,
  successHandler,
  unauthorizedHandler,
} from './fixtures/api-mocks.js'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('Transport.send', () => {
  test('sends batch successfully on 200', async () => {
    server.use(successHandler)
    const t = new Transport({ apiKey: 'test', apiUrl: 'https://api.jamjet.dev', project: 'p' })
    await expect(t.send([{ type: 'span', trace_id: 't', span_id: 's' } as any])).resolves.toBeUndefined()
  })

  test('sets Authorization header', async () => {
    let receivedAuth: string | null = null
    const { http, HttpResponse } = await import('msw')
    server.use(
      http.post('https://api.jamjet.dev/v1/events/ingest', ({ request }) => {
        receivedAuth = request.headers.get('authorization')
        return new HttpResponse(null, { status: 200 })
      }),
    )
    const t = new Transport({ apiKey: 'secret-key', apiUrl: 'https://api.jamjet.dev', project: 'p' })
    await t.send([{ type: 'span' } as any])
    expect(receivedAuth).toBe('Bearer secret-key')
  })

  test('retries on 5xx with exponential backoff', async () => {
    server.use(flakyHandler(2))
    const t = new Transport({
      apiKey: 'k',
      apiUrl: 'https://api.jamjet.dev',
      project: 'p',
      maxRetries: 5,
      initialBackoffMs: 1,
    })
    await expect(t.send([{ type: 'span' } as any])).resolves.toBeUndefined()
  })

  test('honors Retry-After on 429', async () => {
    server.use(rateLimitHandler(0))
    const t = new Transport({
      apiKey: 'k',
      apiUrl: 'https://api.jamjet.dev',
      project: 'p',
      maxRetries: 3,
      initialBackoffMs: 1,
    })
    await expect(t.send([{ type: 'span' } as any])).resolves.toBeUndefined()
  })

  test('drops on 4xx without retry', async () => {
    server.use(unauthorizedHandler)
    const t = new Transport({
      apiKey: 'k',
      apiUrl: 'https://api.jamjet.dev',
      project: 'p',
      maxRetries: 3,
      initialBackoffMs: 1,
    })
    await expect(t.send([{ type: 'span' } as any])).rejects.toThrow(/401/)
  })

  test('gives up after maxRetries on persistent 5xx', async () => {
    server.use(flakyHandler(99))
    const t = new Transport({
      apiKey: 'k',
      apiUrl: 'https://api.jamjet.dev',
      project: 'p',
      maxRetries: 2,
      initialBackoffMs: 1,
    })
    await expect(t.send([{ type: 'span' } as any])).rejects.toThrow()
  })

  test('gzips body when payload exceeds 4KB', async () => {
    let receivedEncoding: string | null = null
    const { http, HttpResponse } = await import('msw')
    server.use(
      http.post('https://api.jamjet.dev/v1/events/ingest', ({ request }) => {
        receivedEncoding = request.headers.get('content-encoding')
        return HttpResponse.json({ accepted: true })
      }),
    )
    const big = Array.from({ length: 200 }, (_, i) => ({
      type: 'span',
      trace_id: 't' + i,
      span_id: 's' + i,
      payload: { junk: 'x'.repeat(100) },
    }))
    const t = new Transport({ apiKey: 'k', apiUrl: 'https://api.jamjet.dev', project: 'p' })
    await t.send(big as any)
    expect(receivedEncoding).toBe('gzip')
  })
})
