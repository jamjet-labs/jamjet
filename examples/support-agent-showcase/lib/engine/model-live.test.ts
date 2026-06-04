import { test, expect, vi } from 'vitest'
vi.mock('@anthropic-ai/sdk', () => {
  return { default: class { messages = { create: vi.fn(async () => ({
    content: [{ type: 'text', text: 'live reply' }],
    model: 'claude-sonnet-4-6',
    usage: { input_tokens: 2000, output_tokens: 20, cache_read_input_tokens: 1800 },
  })) } } }
})
import { liveModel } from './model-live.js'
test('liveModel returns the normalized shape from a real-SDK-shaped response', async () => {
  process.env.ANTHROPIC_API_KEY = 'sk-test'
  const res = await liveModel({ model: 'claude-sonnet-4-6', system: 'KB', messages: [{ role: 'user', content: 'hi' }] })
  expect(res.content[0].text).toBe('live reply')
  expect(res.usage.cache_read_input_tokens).toBe(1800)
})
