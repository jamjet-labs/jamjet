import { afterEach, beforeEach, describe, it, expect } from 'vitest'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { CompactionResolver } from '../src/compaction.js'
import { runEnforcedCall } from '../src/enforcement.js'

function makeClient(): Client {
  const c = new Client(resolveConfig({ apiKey: 'jj_t', project: 'p' }))
  setActive(c)
  return c
}

const okResp = {
  usage: { input_tokens: 1000, output_tokens: 20 },
  model: 'claude-sonnet-4-6',
  content: [],
}

const BIG = 'y'.repeat(400)

describe('tool_compaction in runEnforcedCall', () => {
  beforeEach(async () => {
    await resetActive()
  })
  afterEach(async () => {
    await resetActive()
  })

  it('truncates an oversized tool_result before calling the model and records tokens_saved', async () => {
    const client = makeClient()
    client._compaction = new CompactionResolver([{ toolPattern: 'search.*', maxResultTokens: 5 }])

    let received: any
    const original = async (args: any) => { received = args; return okResp }

    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => { recorded = e; return orig(e) }

    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [{
        model: 'claude-sonnet-4-6',
        messages: [
          { role: 'assistant', content: [{ type: 'tool_use', id: 'tu_1', name: 'search.web', input: {} }] },
          { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'tu_1', content: BIG }] },
        ],
      }],
    })

    // model saw the truncated content
    const sent = received.messages[1].content[0].content as string
    expect(sent.length).toBeLessThan(BIG.length)
    expect(sent).toContain('truncated by JamJet')

    // span recorded tokens_saved + compacted flag
    expect(recorded.payload?.compacted).toBe(true)
    expect(recorded.payload?.tokens_saved).toBeGreaterThan(0)
  })

  it('does not truncate when no compaction rules are set', async () => {
    const client = makeClient()
    // _compaction defaults to empty CompactionResolver — no rules

    let received: any
    const original = async (args: any) => { received = args; return okResp }

    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [{
        model: 'claude-sonnet-4-6',
        messages: [
          { role: 'assistant', content: [{ type: 'tool_use', id: 'tu_1', name: 'search.web', input: {} }] },
          { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'tu_1', content: BIG }] },
        ],
      }],
    })

    const sent = received.messages[1].content[0].content as string
    expect(sent).toBe(BIG)
  })

  it('does not truncate a tool_result that is under the token cap', async () => {
    const client = makeClient()
    client._compaction = new CompactionResolver([{ toolPattern: 'search.*', maxResultTokens: 5000 }])

    let received: any
    const original = async (args: any) => { received = args; return okResp }
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => { recorded = e; return orig(e) }

    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [{
        model: 'claude-sonnet-4-6',
        messages: [
          { role: 'assistant', content: [{ type: 'tool_use', id: 'tu_1', name: 'search.web', input: {} }] },
          { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'tu_1', content: 'short result' }] },
        ],
      }],
    })

    const sent = received.messages[1].content[0].content as string
    expect(sent).toBe('short result')
    expect(recorded.payload?.compacted).toBeUndefined()
    expect(recorded.payload?.tokens_saved).toBeUndefined()
  })
})
