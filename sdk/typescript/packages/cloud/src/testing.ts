import { http, HttpResponse } from 'msw'
import { setupServer, type SetupServer } from 'msw/node'
import { BudgetManager } from './budget.js'
import { Client, getActive, resetActive, setActive } from './client.js'
import { resolveConfig } from './config.js'
import { PolicyEvaluator } from './policy.js'
import type { SpanEventDict } from './span.js'
import type { Transport } from './transport.js'

class InMemoryTransport {
  readonly events: SpanEventDict[] = []
  async send(events: SpanEventDict[]): Promise<void> {
    this.events.push(...events)
  }
}

// --- msw mock-approval state ---

let mswServer: SetupServer | null = null

type QueuedMock = {
  outcome: 'approve' | 'reject'
  reason?: string
  delayMs: number
}

const queuedByAction = new Map<string, QueuedMock>()
const queuedById = new Map<string, QueuedMock>()

function ensureMswServer(apiUrl: string): void {
  if (mswServer) return
  const server = setupServer(
    http.post(`${apiUrl}/v1/approvals`, async ({ request }) => {
      const body = (await request.json()) as { action?: string }
      const action = body.action ?? ''
      const queued = queuedByAction.get(action)
      if (!queued) return HttpResponse.json({ error: 'no mock queued for action' }, { status: 500 })
      const id = `apr_${Math.random().toString(36).slice(2, 10)}`
      queuedById.set(id, queued)
      queuedByAction.delete(action)
      return HttpResponse.json({ id })
    }),
    http.get(`${apiUrl}/v1/approvals/:id`, async ({ params }) => {
      const id = String(params['id'])
      const q = queuedById.get(id)
      if (!q) return HttpResponse.json({ status: 'pending' })
      if (q.delayMs > 0) {
        await new Promise<void>((r) => setTimeout(r, q.delayMs))
      }
      return HttpResponse.json(
        q.outcome === 'approve'
          ? { status: 'approved' }
          : { status: 'rejected', reason: q.reason },
      )
    }),
  )
  server.listen({ onUnhandledRequest: 'bypass' })
  mswServer = server
}

// --- TestHarness interface ---

export type TestHarness = {
  /** The underlying Client instance (used for setActive in tests). */
  readonly client: Client
  /** Direct access to the policy evaluator on the default client. */
  readonly policy: PolicyEvaluator
  /** Direct access to the budget manager on the default client. */
  readonly budget: BudgetManager
  /**
   * Queue a deterministic approval outcome for `action`.
   * The next `requireApproval(action)` call will return/reject based on `outcome`.
   */
  mockApproval(
    action: string,
    outcome: 'approve' | 'reject',
    opts?: { delayMs?: number; reason?: string },
  ): void
  /** Clear queued mocks and reset the default client state. */
  reset(): Promise<void>
  /** All spans captured by the in-memory transport (populated via `run()`). */
  readonly spans: readonly SpanEventDict[]
  /** Sum of cost_usd across all captured spans. */
  readonly totalCostUsd: number
  /**
   * Run `fn` in an isolated client (separate from `harness.client`).
   *
   * Note: policy and budget rules added via `harness.policy.add(...)` /
   * `harness.budget.setLimit(...)` apply to `harness.client`, not to the
   * client created for this run. Add rules manually inside `fn` if you
   * need them in the isolated context.
   */
  run<T>(fn: () => Promise<T>): Promise<T>
}

// Default test API URL — distinct from URLs used in other test files to avoid
// msw handler collisions (approvals.test.ts uses api.jamjet.test,
// transport.test.ts uses api.jamjet.dev).
const TEST_API_URL = 'https://api.jamjet.internal'

export function createTestHarness(opts: { project?: string; agent?: string } = {}): TestHarness {
  const project = opts.project ?? 'test-harness'

  // Set up the module-level msw server (no-op if already running).
  ensureMswServer(TEST_API_URL)

  // Default client used for .policy / .budget / .client / .mockApproval access.
  const defaultTransport = new InMemoryTransport()
  const defaultConfig = resolveConfig({
    apiKey: 'test-key',
    project,
    apiUrl: TEST_API_URL,
    ...(opts.agent !== undefined ? { agent: opts.agent } : {}),
  })
  const defaultClient = new Client(defaultConfig, defaultTransport as unknown as Transport)

  // Separate in-memory transport for run() spans.
  const runTransport = new InMemoryTransport()

  const harness: TestHarness = {
    get client() {
      return defaultClient
    },

    get policy() {
      return defaultClient._policy
    },

    get budget() {
      return defaultClient._budget
    },

    mockApproval(
      action: string,
      outcome: 'approve' | 'reject',
      mopts: { delayMs?: number; reason?: string } = {},
    ) {
      queuedByAction.set(action, {
        outcome,
        ...(mopts.reason !== undefined ? { reason: mopts.reason } : {}),
        delayMs: mopts.delayMs ?? 0,
      })
    },

    async reset() {
      queuedByAction.clear()
      queuedById.clear()
      // Clear any in-memory spans accumulated on the default transport.
      defaultTransport.events.length = 0
      runTransport.events.length = 0
    },

    get spans() {
      return runTransport.events as readonly SpanEventDict[]
    },

    get totalCostUsd() {
      return runTransport.events.reduce((sum, e) => sum + (e.cost_usd ?? 0), 0)
    },

    async run<T>(fn: () => Promise<T>): Promise<T> {
      const config = resolveConfig({
        apiKey: 'test-key',
        project,
        apiUrl: TEST_API_URL,
        ...(opts.agent !== undefined ? { agent: opts.agent } : {}),
      })
      const previous = getActive()
      const client = new Client(config, runTransport as unknown as Transport)
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
export { setActive, resetActive } from './client.js'
