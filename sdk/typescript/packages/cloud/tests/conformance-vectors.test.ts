// Shared vectors with the Python SDK; see sdk/conformance/README.md.
import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { PRICES_PER_MTOK, cacheReadSavingsUsd, estimateCost, normalizeModel } from '../src/cost.js'
import { PolicyEvaluator } from '../src/policy.js'

const vectors = JSON.parse(
  readFileSync(fileURLToPath(new URL('../../../../conformance/cloud-sdk-vectors.json', import.meta.url)), 'utf-8'),
) as {
  glob: Array<{ pattern: string; name: string; match: boolean }>
  prices: { fallback: { input: number; output: number }; models: Record<string, { input: number; output: number; cache_read?: number }> }
  cost: Array<{ model: string; input_tokens: number; output_tokens: number; cache_read_tokens?: number; cache_write_tokens?: number; cache_write_1h_tokens?: number; usd: number }>
  normalize: Array<{ model: string; as: string }>
  cache_read_savings: Array<{ model: string; cache_read_tokens: number; usd: number }>
}

describe('glob vectors', () => {
  for (const v of vectors.glob) {
    it(`${JSON.stringify(v.pattern)} ${v.match ? 'matches' : 'does not match'} ${JSON.stringify(v.name)}`, () => {
      const ev = new PolicyEvaluator()
      ev.add('block', v.pattern)
      expect(ev.evaluate(v.name).blocked).toBe(v.match)
    })
  }
})

describe('price table', () => {
  it('equals the shared table entry by entry', () => {
    const shared = Object.fromEntries(
      Object.entries(vectors.prices.models).map(([m, p]) => [
        m,
        p.cache_read === undefined ? [p.input, p.output] : [p.input, p.output, p.cache_read],
      ]),
    )
    expect(PRICES_PER_MTOK).toEqual(shared)
  })

  for (const v of vectors.cost) {
    it(`cost of ${v.model} ${v.input_tokens}/${v.output_tokens}`, () => {
      const cache = { read: v.cache_read_tokens ?? 0, write: v.cache_write_tokens ?? 0, write1h: v.cache_write_1h_tokens ?? 0 }
      expect(estimateCost(v.model, v.input_tokens, v.output_tokens, cache)).toBeCloseTo(v.usd, 12)
    })
  }

  for (const v of vectors.normalize) {
    it(`normalizes ${v.model}`, () => {
      expect(normalizeModel(v.model)).toBe(v.as)
    })
  }

  for (const v of vectors.cache_read_savings) {
    it(`cache-read savings for ${v.model}`, () => {
      expect(cacheReadSavingsUsd(v.model, v.cache_read_tokens)).toBeCloseTo(v.usd, 12)
    })
  }
})

describe('Anthropic prompt-cache tokens count toward the budget', () => {
  it('charges cache reads and writes on a response and on a stream', async () => {
    const { Client, setActive, resetActive } = await import('../src/client.js')
    const { resolveConfig } = await import('../src/config.js')
    const { runEnforcedCall } = await import('../src/enforcement.js')
    const usage = { input_tokens: 1000, output_tokens: 500, cache_read_input_tokens: 10000, cache_creation_input_tokens: 2000 }
    for (const stream of [false, true]) {
      await resetActive()
      const client = new Client(resolveConfig({ apiKey: 'jj_t', project: 'p', apiUrl: 'https://api.jamjet.test', maxCostUsd: 100 }))
      client.recordSpan = () => {}
      setActive(client)
      const original = stream
        ? async () =>
            (async function* () {
              yield { type: 'message_start', message: { model: 'claude-sonnet-4-6', usage: { ...usage, output_tokens: 1 } } }
              yield { type: 'message_delta', usage: { output_tokens: 500 } }
              yield { type: 'message_stop' }
            })()
        : async () => ({ model: 'claude-sonnet-4-6', usage, content: [] })
      const out = await runEnforcedCall({ client, vendor: 'anthropic', original, args: [{ model: 'claude-sonnet-4-6', messages: [], stream }] })
      if (stream) for await (const _ of out as AsyncIterable<unknown>) { /* drain */ }
      // 1000*$3 + 500*$15 + 10000*$0.30 + 2000*$3.75, per million
      expect(client._budget.spent).toBeCloseTo(0.021, 12)
    }
    await resetActive()
  })
})

describe('Anthropic 1-hour cache writes', () => {
  it('are charged at 2x input when the usage splits them out', async () => {
    const { cacheWrites } = await import('../src/enforcement.js')
    const split = cacheWrites({ cache_creation_input_tokens: 3000, cache_creation: { ephemeral_5m_input_tokens: 1000, ephemeral_1h_input_tokens: 2000 } })
    expect(split).toEqual({ cacheWrite: 1000, cacheWrite1h: 2000 })
    expect(cacheWrites({ cache_creation_input_tokens: 3000 })).toEqual({ cacheWrite: 3000, cacheWrite1h: 0 })
    expect(estimateCost('claude-sonnet-4-6', 0, 0, { write: 1000, write1h: 2000 })).toBeCloseTo((1000 * 3.75 + 2000 * 6) / 1e6, 12)
  })
})
