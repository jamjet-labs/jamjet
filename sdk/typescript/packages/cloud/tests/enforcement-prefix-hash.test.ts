import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { MessageParam } from '@anthropic-ai/sdk/resources/messages'
import { Client, resetActive, setActive } from '../src/client.js'
import { resolveConfig } from '../src/config.js'
import { runEnforcedCall } from '../src/enforcement.js'

function makeClient(): Client {
  const cfg = resolveConfig({ apiKey: 'jj_t', project: 'p', maxCostUsd: 100 })
  const c = new Client(cfg)
  setActive(c)
  return c
}

const okAnthropicResponse = {
  usage: { input_tokens: 10, output_tokens: 5 },
  model: 'claude-sonnet-4-6',
  content: [{ type: 'text', text: 'hi' }],
}

describe('runEnforcedCall prompt_prefix_hash', () => {
  beforeEach(async () => {
    await resetActive()
  })
  afterEach(async () => {
    await resetActive()
  })

  it('sets prompt_prefix_hash on recorded span when callback is provided', async () => {
    const client = makeClient()
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => {
      recorded = e
      return orig(e)
    }

    const original = async () => okAnthropicResponse
    const callback = vi.fn((_input: string | MessageParam[]) => 'deadbeefcafef00d')

    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [
        {
          model: 'claude-sonnet-4-6',
          messages: [{ role: 'user', content: 'hello world' }],
        },
      ],
      computePromptPrefixHash: callback,
    })

    expect(callback).toHaveBeenCalledTimes(1)
    // The callback receives the messages array as extracted by enforcement.
    expect(callback.mock.calls[0]?.[0]).toEqual([{ role: 'user', content: 'hello world' }])
    expect(recorded.prompt_prefix_hash).toBe('deadbeefcafef00d')
  })

  it('omits prompt_prefix_hash when no callback provided', async () => {
    const client = makeClient()
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => {
      recorded = e
      return orig(e)
    }
    const original = async () => okAnthropicResponse
    await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [{ model: 'claude-sonnet-4-6', messages: [] }],
    })
    expect(recorded).toBeDefined()
    expect(recorded).not.toHaveProperty('prompt_prefix_hash')
  })

  it('LLM call still succeeds when hash callback throws; field is absent', async () => {
    const client = makeClient()
    let recorded: any
    const orig = client.recordSpan.bind(client)
    client.recordSpan = (e) => {
      recorded = e
      return orig(e)
    }

    const original = vi.fn(async () => okAnthropicResponse)
    const callback = vi.fn(() => {
      throw new Error('hash module exploded')
    })

    const result = await runEnforcedCall({
      client,
      vendor: 'anthropic',
      original,
      args: [{ model: 'claude-sonnet-4-6', messages: [] }],
      computePromptPrefixHash: callback,
    })

    // 1. LLM call completed normally.
    expect(original).toHaveBeenCalledTimes(1)
    expect(result).toBe(okAnthropicResponse)
    // 2. Span was still recorded.
    expect(recorded).toBeDefined()
    expect(recorded.status).toBe('ok')
    // 3. Hash field is absent (not present as undefined either).
    expect(recorded).not.toHaveProperty('prompt_prefix_hash')
  })
})
