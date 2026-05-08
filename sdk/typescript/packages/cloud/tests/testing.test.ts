import { afterEach, describe, expect, test, vi } from 'vitest'
import { createTestHarness } from '../src/testing.js'
import { requireApproval } from '../src/governance.js'
import { resetActive, setActive } from '../src/client.js'
import { wrap } from '../src/wrap.js'

const fakeOpenAI = () => ({
  chat: {
    completions: {
      create: vi.fn(async (args: any) => ({
        id: 'c1',
        model: args.model,
        usage: { prompt_tokens: 10, completion_tokens: 20 },
      })),
    },
  },
})

describe('createTestHarness', () => {
  test('captures spans without sending to network', async () => {
    const harness = createTestHarness({ project: 'test' })
    const client = wrap(fakeOpenAI())

    await harness.run(async () => {
      await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
      await client.chat.completions.create({ model: 'gpt-4o-mini', messages: [] })
    })

    expect(harness.spans).toHaveLength(2)
    expect(harness.spans[0]?.name).toBe('openai.gpt-4o')
    expect(harness.spans[1]?.name).toBe('openai.gpt-4o-mini')
  })

  test('totalCostUsd sums span costs', async () => {
    const harness = createTestHarness({ project: 'test' })
    const client = wrap(fakeOpenAI())
    await harness.run(async () => {
      await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
    })
    expect(harness.totalCostUsd).toBeGreaterThan(0)
  })

  test('isolates between runs', async () => {
    const h1 = createTestHarness({ project: 'a' })
    const h2 = createTestHarness({ project: 'b' })
    const client = wrap(fakeOpenAI())
    await h1.run(async () => {
      await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
    })
    await h2.run(async () => {
      await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
      await client.chat.completions.create({ model: 'gpt-4o', messages: [] })
    })
    expect(h1.spans).toHaveLength(1)
    expect(h2.spans).toHaveLength(2)
  })
})

describe('createTestHarness Plan 2', () => {
  afterEach(async () => {
    await resetActive()
  })

  test('exposes policy and budget on harness', async () => {
    const h = await createTestHarness()
    expect(h.policy).toBeDefined()
    expect(h.budget).toBeDefined()
    h.policy.add('block', 'x_*')
    expect(h.policy.evaluate('x_y').blocked).toBe(true)
    await h.reset()
  })

  test('mockApproval queues a deterministic approve outcome', async () => {
    const h = await createTestHarness()
    setActive(h.client)
    h.mockApproval('wire_money', 'approve')
    const id = await requireApproval('wire_money', { pollIntervalMs: 1, timeoutMs: 1000 })
    expect(id).toMatch(/^apr_/)
    await h.reset()
  })

  test('mockApproval reject propagates reason', async () => {
    const h = await createTestHarness()
    setActive(h.client)
    h.mockApproval('wire_money', 'reject', { reason: 'no way' })
    await expect(requireApproval('wire_money', { pollIntervalMs: 1, timeoutMs: 1000 }))
      .rejects.toMatchObject({ name: 'JamjetApprovalRejected', reason: 'no way' })
    await h.reset()
  })
})
