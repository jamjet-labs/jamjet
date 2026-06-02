import { afterEach, beforeEach, describe, it, expect } from 'vitest'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { CacheInjectResolver } from '../src/cache-inject.js'
import { runEnforcedCall } from '../src/enforcement.js'

function makeClient(): Client {
  const c = new Client(resolveConfig({ apiKey: 'jj_t', project: 'p' }))
  setActive(c)
  return c
}

const okResp = {
  usage: { input_tokens: 1000, output_tokens: 20, cache_read_input_tokens: 1000 },
  model: 'claude-sonnet-4-6',
  content: [],
}

describe('cache_inject in runEnforcedCall', () => {
  beforeEach(async () => {
    await resetActive()
  })
  afterEach(async () => {
    await resetActive()
  })

  it('injects cache_control on a matching prefix hash and records saved_cents', async () => {
    const client = makeClient()
    client._cacheInject = new CacheInjectResolver(['deadbeefcafef00d'])
    let received: any
    const original = async (args: any) => { received = args; return okResp }
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => { recorded = e; return orig(e) }

    await runEnforcedCall({
      client, vendor: 'anthropic', original,
      args: [{ model: 'claude-sonnet-4-6', system: 'stable preamble', messages: [{ role: 'user', content: 'hi' }] }],
      computePromptPrefixHash: () => 'deadbeefcafef00d',
    })

    expect(Array.isArray(received.system)).toBe(true)
    expect(received.system[0].cache_control).toEqual({ type: 'ephemeral' })
    expect(recorded.payload?.saved_cents).toBeGreaterThan(0)
  })

  it('does not inject when the prefix hash does not match', async () => {
    const client = makeClient()
    client._cacheInject = new CacheInjectResolver(['somethingelse'])
    let received: any
    const original = async (args: any) => { received = args; return okResp }
    await runEnforcedCall({
      client, vendor: 'anthropic', original,
      args: [{ model: 'claude-sonnet-4-6', system: 'stable preamble', messages: [] }],
      computePromptPrefixHash: () => 'deadbeefcafef00d',
    })
    expect(received.system).toBe('stable preamble')
  })
})
