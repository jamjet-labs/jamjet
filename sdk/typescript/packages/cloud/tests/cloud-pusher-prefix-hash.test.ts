// Span events with `prompt_prefix_hash` must round-trip into the outgoing
// `Transport.send` request body so the Cloud can group cost-waste candidates
// by header.
//
// Note: the file name preserves the brief's spelling, but the actual span
// pusher in this package is `Transport` (cloud-pusher.ts is for policy-audit
// CloudPusherEvents, a different shape entirely).

import { setupServer } from 'msw/node'
import { afterAll, afterEach, beforeAll, describe, expect, test } from 'vitest'
import { Span } from '../src/span.js'
import { Transport } from '../src/transport.js'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'error' }))
afterEach(() => server.resetHandlers())
afterAll(() => server.close())

describe('Span -> Transport round-trip for prompt_prefix_hash', () => {
  test('field reaches the wire when set', async () => {
    let receivedBody: any
    const { http, HttpResponse } = await import('msw')
    server.use(
      http.post('https://api.jamjet.dev/v1/events/ingest', async ({ request }) => {
        receivedBody = await request.json()
        return new HttpResponse(null, { status: 200 })
      }),
    )

    const span = new Span({ traceId: 't', spanId: 's', kind: 'llm_call', name: 'anthropic.claude' })
    span.setPromptPrefixHash('cafef00ddeadbeef')
    span.finish('ok', 12)

    const t = new Transport({ apiKey: 'k', apiUrl: 'https://api.jamjet.dev', project: 'p' })
    await t.send([span.toEventDict()])

    expect(receivedBody.events).toHaveLength(1)
    expect(receivedBody.events[0].prompt_prefix_hash).toBe('cafef00ddeadbeef')
  })

  test('field is absent on the wire when unset', async () => {
    let receivedBody: any
    const { http, HttpResponse } = await import('msw')
    server.use(
      http.post('https://api.jamjet.dev/v1/events/ingest', async ({ request }) => {
        receivedBody = await request.json()
        return new HttpResponse(null, { status: 200 })
      }),
    )

    const span = new Span({ traceId: 't', spanId: 's', kind: 'llm_call', name: 'anthropic.claude' })
    span.finish('ok', 12)

    const t = new Transport({ apiKey: 'k', apiUrl: 'https://api.jamjet.dev', project: 'p' })
    await t.send([span.toEventDict()])

    expect(receivedBody.events).toHaveLength(1)
    expect(receivedBody.events[0]).not.toHaveProperty('prompt_prefix_hash')
  })
})
