import { afterEach, describe, expect, test } from 'vitest'
import { Client, getActive, resetActive, setActive } from '../src/client.js'
import type { ResolvedConfig } from '../src/config.js'

const cfg: ResolvedConfig = {
  apiKey: 'k',
  apiUrl: 'https://api.jamjet.dev',
  project: 'p',
  agent: 'default',
  sampling: { rate: 1, alwaysKeepErrors: true, alwaysKeepApprovals: true },
  redaction: { mode: 'standard', custom: [] },
  debug: false,
}

describe('Client state', () => {
  afterEach(() => resetActive())

  test('getActive returns null before init', () => {
    expect(getActive()).toBeNull()
  })

  test('setActive stores client', () => {
    const c = new Client(cfg)
    setActive(c)
    expect(getActive()).toBe(c)
  })

  test('resetActive clears client and stops batcher', async () => {
    const c = new Client(cfg)
    setActive(c)
    await resetActive()
    expect(getActive()).toBeNull()
  })

  test('Client.recordSpan adds to batcher', () => {
    const c = new Client(cfg)
    c.recordSpan({ type: 'span', trace_id: 't', span_id: 's' } as any)
    expect(c.batcher).toBeDefined()
  })
})
