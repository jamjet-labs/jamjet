import { afterEach, describe, expect, test } from 'vitest'
import { patchAnthropic, unpatchAnthropic } from '../../src/patcher/anthropic.js'
import { Client, getActive, resetActive, setActive } from '../../src/client.js'
import type { ResolvedConfig } from '../../src/config.js'
import type { SpanEventDict } from '../../src/span.js'
import type { Transport } from '../../src/transport.js'

class FakeMessages {
  async create(args: any): Promise<any> {
    return {
      id: 'msg-1',
      model: args.model,
      usage: { input_tokens: 50, output_tokens: 100 },
      content: [{ type: 'text', text: 'hi' }],
    }
  }
}

class CapturingTransport {
  events: SpanEventDict[] = []
  async send(e: SpanEventDict[]) { this.events.push(...e) }
}

const cfg: ResolvedConfig = {
  apiKey: 'k', apiUrl: 'https://api.jamjet.dev', project: 'p', agent: 'default',
  sampling: { rate: 1, alwaysKeepErrors: true, alwaysKeepApprovals: true },
  redaction: { mode: 'off', custom: [] }, debug: true,
}

describe('patchAnthropic', () => {
  afterEach(async () => {
    unpatchAnthropic()
    await resetActive()
  })

  test('patches Messages.create at prototype level', async () => {
    const fakeModule = { resources: { messages: { Messages: FakeMessages } } }
    patchAnthropic(fakeModule as any)
    const transport = new CapturingTransport()
    setActive(new Client(cfg, transport as unknown as Transport))

    await new FakeMessages().create({ model: 'claude-sonnet-4-6', messages: [] })

    await getActive()?.shutdown()
    expect(transport.events).toHaveLength(1)
    expect(transport.events[0]?.name).toBe('anthropic.claude-sonnet-4-6')
    expect(transport.events[0]?.input_tokens).toBe(50)
    expect(transport.events[0]?.output_tokens).toBe(100)
  })

  test('idempotent', () => {
    const fakeModule = { resources: { messages: { Messages: FakeMessages } } }
    const original = FakeMessages.prototype.create
    patchAnthropic(fakeModule as any)
    const afterFirst = FakeMessages.prototype.create
    patchAnthropic(fakeModule as any)
    expect(FakeMessages.prototype.create).toBe(afterFirst)
    expect(afterFirst).not.toBe(original)
  })
})
