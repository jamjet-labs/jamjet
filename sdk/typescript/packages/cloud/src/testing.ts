import { Client, getActive, resetActive, setActive } from './client.js'
import { resolveConfig } from './config.js'
import type { SpanEventDict } from './span.js'
import type { Transport } from './transport.js'

class InMemoryTransport {
  readonly events: SpanEventDict[] = []
  async send(events: SpanEventDict[]): Promise<void> {
    this.events.push(...events)
  }
}

export type TestHarness = {
  readonly spans: readonly SpanEventDict[]
  readonly totalCostUsd: number
  run<T>(fn: () => Promise<T>): Promise<T>
}

export function createTestHarness(opts: { project: string; agent?: string }): TestHarness {
  const transport = new InMemoryTransport()

  const harness: TestHarness = {
    get spans() {
      return transport.events as readonly SpanEventDict[]
    },
    get totalCostUsd() {
      return transport.events.reduce((sum, e) => sum + (e.cost_usd ?? 0), 0)
    },
    async run<T>(fn: () => Promise<T>): Promise<T> {
      const config = resolveConfig({
        apiKey: 'test-key',
        project: opts.project,
        ...(opts.agent !== undefined ? { agent: opts.agent } : {}),
      })
      const previous = getActive()
      const client = new Client(config, transport as unknown as Transport)
      setActive(client)
      try {
        return await fn()
      } finally {
        await client.shutdown()
        if (previous) setActive(previous)
        else await resetActive()
      }
    },
  }

  return harness
}

export type { SpanEventDict }
