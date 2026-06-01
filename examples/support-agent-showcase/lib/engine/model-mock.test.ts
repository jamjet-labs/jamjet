import { test, expect } from 'vitest'
import { mockModel } from './model-mock.js'
import { SYSTEM_PROMPT } from './knowledge-base.js'
test('SYSTEM_PROMPT is large enough to make caching worthwhile', () => {
  expect(SYSTEM_PROMPT.length).toBeGreaterThan(4000)   // big reused prefix
})
test('mock model reports cache_read tokens only when cache_control is present', async () => {
  const base = { model: 'claude-sonnet-4-6', system: SYSTEM_PROMPT, messages: [{ role: 'user', content: 'how do I reset my password?' }] }
  const cold = await mockModel(base)
  expect(typeof cold.content[0].text).toBe('string')
  expect(cold.usage.input_tokens).toBeGreaterThan(0)
  expect(cold.usage.cache_read_input_tokens).toBe(0)
  const warmArgs = { ...base, system: [{ type: 'text', text: SYSTEM_PROMPT, cache_control: { type: 'ephemeral' } }] }
  const warm = await mockModel(warmArgs)
  expect(warm.usage.cache_read_input_tokens).toBeGreaterThan(0)
})
test('mock model is deterministic for the same question', async () => {
  const a = await mockModel({ model: 'claude-sonnet-4-6', system: SYSTEM_PROMPT, messages: [{ role: 'user', content: 'how do I get a refund?' }] })
  const b = await mockModel({ model: 'claude-sonnet-4-6', system: SYSTEM_PROMPT, messages: [{ role: 'user', content: 'how do I get a refund?' }] })
  expect(a.content[0].text).toBe(b.content[0].text)
})
