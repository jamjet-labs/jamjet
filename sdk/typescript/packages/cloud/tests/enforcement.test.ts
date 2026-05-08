import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { runEnforcedCall } from '../src/enforcement.js'
import { JamjetBudgetExceeded, JamjetPolicyBlocked } from '../src/errors.js'

function makeClient(opts: { maxCostUsd?: number } = {}): Client {
  const cfg = resolveConfig({ apiKey: 'jj_t', project: 'p', ...(opts.maxCostUsd !== undefined ? { maxCostUsd: opts.maxCostUsd } : {}) })
  const c = new Client(cfg)
  setActive(c)
  return c
}

describe('runEnforcedCall', () => {
  beforeEach(async () => {
    await resetActive()
  })
  afterEach(async () => {
    await resetActive()
  })

  it('strips blocked tools from openai args before calling original', async () => {
    const client = makeClient()
    client._policy.add('block', 'wire_*')
    let receivedArgs: any
    const original = async (args: any) => {
      receivedArgs = args
      return { usage: { prompt_tokens: 10, completion_tokens: 5 }, model: 'gpt-4o', choices: [{ message: {} }] }
    }
    const args = {
      model: 'gpt-4o',
      messages: [],
      tools: [
        { type: 'function', function: { name: 'search' } },
        { type: 'function', function: { name: 'wire_money' } },
      ],
    }
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original,
      args: [args],
    })
    expect(receivedArgs.tools).toHaveLength(1)
    expect(receivedArgs.tools[0].function.name).toBe('search')
  })

  it('throws JamjetBudgetExceeded pre-call', async () => {
    const client = makeClient({ maxCostUsd: 0.001 })
    client._budget.record(0.0009)
    const original = async () => ({ usage: { prompt_tokens: 1000, completion_tokens: 1000 }, model: 'gpt-4o' })
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original,
        args: [{ model: 'gpt-4o', messages: [{ role: 'user', content: 'x'.repeat(10_000) }] }],
      }),
    ).rejects.toBeInstanceOf(JamjetBudgetExceeded)
  })

  it('throws JamjetPolicyBlocked post-decision when model invents blocked name', async () => {
    const client = makeClient()
    client._policy.add('block', 'wire_*')
    const original = async () => ({
      usage: { prompt_tokens: 10, completion_tokens: 5 },
      model: 'gpt-4o',
      choices: [{
        message: {
          tool_calls: [{ id: 'tc_1', type: 'function', function: { name: 'wire_money', arguments: '{}' } }],
        },
      }],
    })
    await expect(
      runEnforcedCall({
        client,
        vendor: 'openai',
        original,
        args: [{ model: 'gpt-4o', messages: [], tools: [] }],
      }),
    ).rejects.toBeInstanceOf(JamjetPolicyBlocked)
  })

  it('records actual cost post-call', async () => {
    const client = makeClient({ maxCostUsd: 100 })
    const original = async () => ({
      usage: { prompt_tokens: 1000, completion_tokens: 500 },
      model: 'gpt-4o',
      choices: [{ message: {} }],
    })
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original,
      args: [{ model: 'gpt-4o', messages: [] }],
    })
    expect(client._budget.spent).toBeGreaterThan(0)
  })

  it('attaches agent + user from context to span', async () => {
    const client = makeClient()
    const original = async () => ({
      usage: { prompt_tokens: 1, completion_tokens: 1 },
      model: 'gpt-4o',
      choices: [{ message: {} }],
    })
    let recordedSpan: any
    const origRecord = client.recordSpan.bind(client)
    client.recordSpan = (e) => {
      recordedSpan = e
      return origRecord(e)
    }
    await client._governanceContext.runInContext(
      { agent: { name: 'researcher' }, user: { userId: 'u_42' } },
      async () => {
        await runEnforcedCall({
          client,
          vendor: 'openai',
          original,
          args: [{ model: 'gpt-4o', messages: [] }],
        })
      },
    )
    expect(recordedSpan.agent_name).toBe('researcher')
    expect(recordedSpan.user_id).toBe('u_42')
  })

  it('explicitOverride agent/user beats context', async () => {
    const client = makeClient()
    const original = async () => ({ usage: { prompt_tokens: 1, completion_tokens: 1 }, model: 'gpt-4o', choices: [{ message: {} }] })
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => { recorded = e; return orig(e) }
    await client._governanceContext.runInContext(
      { agent: { name: 'ctx_agent' } },
      async () => {
        await runEnforcedCall({
          client,
          vendor: 'openai',
          original,
          args: [{ model: 'gpt-4o', messages: [] }],
          override: { agent: { name: 'override_agent' } },
        })
      },
    )
    expect(recorded.agent_name).toBe('override_agent')
  })

  it('streaming requests skip post-decision check but still record span', async () => {
    const client = makeClient()
    client._policy.add('block', 'wire_*')
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => { recorded = e; return orig(e) }
    const original = async () => ({
      [Symbol.asyncIterator]: async function* () {
        yield { choices: [] }
      },
    })
    await runEnforcedCall({
      client,
      vendor: 'openai',
      original,
      args: [{ model: 'gpt-4o', stream: true, messages: [], tools: [{ type: 'function', function: { name: 'wire_money' } }] }],
    })
    expect(recorded).toBeDefined()
  })
})
