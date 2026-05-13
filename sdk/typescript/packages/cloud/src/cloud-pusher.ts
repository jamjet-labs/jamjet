// CloudPusher — fire-and-forget Path B push for adapters in serverless / CI.
//
// The contract differs from the daemon's CloudClient (in jamjet-policy):
//   - Never throws. push() returns boolean. The agent's tool-call latency
//     budget MUST NOT be affected by Cloud reachability.
//   - Short timeout (default 500ms). A hung Cloud aborts quickly so the
//     adapter can keep serving its tool call.
//   - Circuit breaker: 5 consecutive failures opens the breaker for 60s.
//     During an open breaker, push() short-circuits to false without an
//     HTTP attempt — saves the 500ms budget on every subsequent call.
//   - 4xx and 5xx alike count as failures and return false. Direct-push has
//     no outbox to retry from, so anything other than a 2xx is dropped.
//     Operators wanting durable retry should run the sidecar daemon (Path A).
//
// Uses Node's built-in fetch (18+) so adapters don't pull in undici as a
// hard dependency. AbortController gives us a single primitive for both
// timeout and external cancellation.

export interface CloudPusherOptions {
  apiBase: string
  apiKey: string
  /** Default 500ms. */
  timeoutMs?: number
  /** Default 5. */
  circuitBreakerThreshold?: number
  /** Default 60_000. */
  circuitBreakerResetMs?: number
  userAgent?: string
}

// Loose shape — adapters can pass their own AuditEventV1-compatible objects.
// The Cloud API validates via its own audit-event-v1 schema, so we don't
// re-validate at this layer.
export interface CloudPusherEvent {
  ts: string
  run_id: string
  adapter: string
  host: string
  tool: string
  decision: string
  executed: boolean
  schema_version: 1
  args?: Record<string, unknown>
  args_redaction?: string
  trace_id?: string | null
  decision_id?: string | null
  rule?: string | null
  rule_kind?: string | null
  server?: string | null
  policy_version?: string
}

interface ResolvedOptions {
  apiBase: string
  apiKey: string
  timeoutMs: number
  circuitBreakerThreshold: number
  circuitBreakerResetMs: number
  userAgent: string
}

export class CloudPusher {
  consecutiveFailures = 0
  private circuitOpenedAt: number | undefined
  private readonly opts: ResolvedOptions

  constructor(opts: CloudPusherOptions) {
    this.opts = {
      apiBase: opts.apiBase,
      apiKey: opts.apiKey,
      timeoutMs: opts.timeoutMs ?? 500,
      circuitBreakerThreshold: opts.circuitBreakerThreshold ?? 5,
      circuitBreakerResetMs: opts.circuitBreakerResetMs ?? 60_000,
      userAgent: opts.userAgent ?? '@jamjet/cloud direct-push',
    }
  }

  isCircuitOpen(): boolean {
    if (this.circuitOpenedAt === undefined) return false
    if (Date.now() - this.circuitOpenedAt > this.opts.circuitBreakerResetMs) {
      this.circuitOpenedAt = undefined
      this.consecutiveFailures = 0
      return false
    }
    return true
  }

  async push(event: CloudPusherEvent): Promise<boolean> {
    if (this.isCircuitOpen()) return false

    const ac = new AbortController()
    const timer = setTimeout(() => ac.abort(), this.opts.timeoutMs)
    try {
      const resp = await fetch(`${this.opts.apiBase}/v1/policy-audit/events`, {
        method: 'POST',
        headers: {
          authorization: `Bearer ${this.opts.apiKey}`,
          'content-type': 'application/json',
          'user-agent': this.opts.userAgent,
        },
        body: JSON.stringify({ events: [event], path: 'direct' }),
        signal: ac.signal,
      })
      if (resp.ok) {
        this.consecutiveFailures = 0
        // Drain the body so the connection can be reused; otherwise Node
        // keeps the socket open until GC.
        try {
          await resp.text()
        } catch {
          // ignore drain failures
        }
        return true
      }
      this.recordFailure()
      return false
    } catch {
      this.recordFailure()
      return false
    } finally {
      clearTimeout(timer)
    }
  }

  private recordFailure(): void {
    this.consecutiveFailures++
    if (this.consecutiveFailures >= this.opts.circuitBreakerThreshold) {
      this.circuitOpenedAt = Date.now()
    }
  }
}
