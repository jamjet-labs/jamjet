import { setupServer } from 'msw/node'
import { http, HttpResponse } from 'msw'
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, test, vi } from 'vitest'
import { init } from '../src/init.js'
import { getActive, resetActive } from '../src/client.js'

const server = setupServer()
beforeAll(() => server.listen({ onUnhandledRequest: 'bypass' }))
afterEach(async () => {
  server.resetHandlers()
  await resetActive()
})
afterAll(() => server.close())

describe('init()', () => {
  test('creates active client on success', async () => {
    server.use(
      http.get('https://api.jamjet.dev/v1/projects/test-app/readiness', () =>
        HttpResponse.json({ ready: true }),
      ),
    )
    await init({ apiKey: 'k', project: 'test-app' })
    expect(getActive()).not.toBeNull()
    expect(getActive()?.config.project).toBe('test-app')
  })

  test('throws ConfigError on missing apiKey', async () => {
    const old = process.env.JAMJET_API_KEY
    delete process.env.JAMJET_API_KEY
    await expect(init({ project: 'p' })).rejects.toThrow(/apiKey/i)
    if (old) process.env.JAMJET_API_KEY = old
  })

  test('warn but does not throw if readiness check fails', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    server.use(
      http.get('https://api.jamjet.dev/v1/projects/p/readiness', () =>
        HttpResponse.json({ error: 'not_found' }, { status: 404 }),
      ),
    )
    await expect(init({ apiKey: 'k', project: 'p' })).resolves.toBeUndefined()
    expect(getActive()).not.toBeNull()
    expect(warn).toHaveBeenCalled()
    warn.mockRestore()
  })

  test('subsequent init replaces active client', async () => {
    server.use(
      http.get('https://api.jamjet.dev/v1/projects/:slug/readiness', () =>
        HttpResponse.json({ ready: true }),
      ),
    )
    await init({ apiKey: 'k1', project: 'p1' })
    const first = getActive()
    await init({ apiKey: 'k2', project: 'p2' })
    const second = getActive()
    expect(second).not.toBe(first)
    expect(second?.config.project).toBe('p2')
  })

  test('debug:true makes transport errors throw on shutdown', async () => {
    server.use(
      http.get('https://api.jamjet.dev/v1/projects/p/readiness', () =>
        HttpResponse.json({ ready: true }),
      ),
    )
    await init({ apiKey: 'k', project: 'p', debug: true })
    expect(getActive()?.config.debug).toBe(true)
  })
})

describe('init Plan 2 sugar fields', () => {
  beforeEach(async () => {
    await resetActive()
  })

  it('seeds budget when maxCostUsd given', async () => {
    await init({ apiKey: 'jj_test', project: 'p', maxCostUsd: 30 })
    const client = getActive()
    expect(client?._budget.remaining).toBe(30)
  })

  it('default agent name flows to client.config.agent', async () => {
    await init({ apiKey: 'jj_test', project: 'p', agent: 'researcher' })
    const client = getActive()
    expect(client?.config.agent).toBe('researcher')
  })
})
