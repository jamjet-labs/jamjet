import { Batcher } from './batcher.js'
import { BudgetManager } from './budget.js'
import { CacheInjectResolver } from './cache-inject.js'
import type { ResolvedConfig } from './config.js'
import { GovernanceContext } from './context.js'
import { PolicyEvaluator } from './policy.js'
import { redactDict } from './redaction.js'
import type { SpanEventDict } from './span.js'
import { Transport } from './transport.js'

export class Client {
  readonly config: ResolvedConfig
  readonly transport: Transport
  readonly batcher: Batcher
  readonly _policy: PolicyEvaluator
  _cacheInject: CacheInjectResolver
  readonly _budget: BudgetManager
  readonly _governanceContext: GovernanceContext

  constructor(config: ResolvedConfig, transportOverride?: Transport) {
    this.config = config
    this.transport =
      transportOverride ??
      new Transport({
        apiKey: config.apiKey,
        apiUrl: config.apiUrl,
        project: config.project,
      })
    this.batcher = new Batcher({
      send: (events) => this.transport.send(events),
      onError: (err) => {
        if (config.debug) {
          console.warn('[jamjet] transport error:', err)
        }
      },
    })
    this._policy = new PolicyEvaluator()
    this._cacheInject = new CacheInjectResolver()
    this._budget = new BudgetManager(config.maxCostUsd ?? null)
    this._governanceContext = new GovernanceContext()
  }

  recordSpan(event: SpanEventDict): void {
    if (this.shouldDrop(event)) return
    const redacted =
      this.config.redaction.mode === 'off' ? event : (redactDict(event) as SpanEventDict)
    this.batcher.add(redacted)
  }

  async shutdown(): Promise<void> {
    await this.batcher.shutdown()
  }

  private shouldDrop(event: SpanEventDict): boolean {
    const { sampling } = this.config
    if (sampling.rate >= 1) return false
    if (sampling.alwaysKeepErrors && event.status === 'error') return false
    if (sampling.alwaysKeepApprovals && event.kind === 'approval') return false
    return Math.random() > sampling.rate
  }
}

let active: Client | null = null

export function getActive(): Client | null {
  return active
}

export function setActive(client: Client): void {
  active = client
}

export async function resetActive(): Promise<void> {
  if (active) {
    await active.shutdown()
    active = null
  }
}
