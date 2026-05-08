import { type Client, getActive } from './client.js'
import type { AgentRef, ScopeFrame, UserContext } from './context.js'
import type { PolicyAction } from './policy.js'
import { pollUntilResolved } from './approvals.js'

const NOT_INIT = 'JamJet Cloud not initialized. Call init() first.'

function activeOrThrow(): Client {
  const c = getActive()
  if (!c) throw new Error(NOT_INIT)
  return c
}

/**
 * Create a frozen {@link AgentRef} identifying this agent. Pass the result to
 * {@link withAgent} or to `wrap(client, { agent })` to attach the identity to
 * every span emitted in that scope.
 */
export function agent(
  name: string,
  opts: { cardUri?: string; description?: string } = {},
): AgentRef {
  const trimmed = (name ?? '').trim()
  if (!trimmed) throw new Error('agent name cannot be empty')
  return Object.freeze({
    name: trimmed,
    ...(opts.cardUri !== undefined ? { cardUri: opts.cardUri } : {}),
    ...(opts.description !== undefined ? { description: opts.description } : {}),
  })
}

/**
 * Run `fn` with `ref` set as the active agent in the current
 * async-local-storage scope. Spans emitted inside `fn` will carry the agent
 * identity, overriding any process-level default set via {@link setProcessContext}.
 */
export async function withAgent<T>(
  ref: AgentRef,
  fn: () => T | Promise<T>,
): Promise<T> {
  const client = activeOrThrow()
  return client._governanceContext.runInContext({ agent: ref }, fn)
}

/**
 * Register a glob-pattern policy rule for one or more tools.
 *
 * - `'block'` — matching tools are stripped from the request before it reaches
 *   the model. If the model returns a tool-call for a blocked tool, a
 *   {@link JamjetPolicyBlocked} error is thrown.
 * - `'allow'` — explicitly permits matching tools (useful to whitelist a subset
 *   when a broader `block` pattern is also registered).
 * - `'require_approval'` — **recognised by the evaluator but runtime
 *   enforcement is not yet implemented in 0.2.0.** Tools matched by a
 *   `require_approval` rule pass through to the model unchanged. Pre-call
 *   approval gating is deferred to a future release.
 */
export function policy(action: PolicyAction, tools: string): void {
  activeOrThrow()._policy.add(action, tools)
}

/**
 * Set a cumulative cost ceiling (in USD) for the active client. Once the
 * recorded spend reaches `maxCostUsd`, subsequent LLM calls throw a
 * `JamjetBudgetExceeded` error. The limit applies to the lifetime of the
 * current client instance.
 */
export function budget(maxCostUsd: number): void {
  activeOrThrow()._budget.setLimit(maxCostUsd)
}

/**
 * Set a process-level user context. All spans emitted after this call will
 * carry the user identity. Pass `null` to clear a previously set user.
 * For request-scoped user context prefer {@link withUserContext}.
 */
export function setUserContext(ctx: UserContext | null): void {
  const client = activeOrThrow()
  const current = client._governanceContext.getCurrentContext() ?? {}
  const next: ScopeFrame = ctx
    ? { ...current, user: ctx }
    : { ...(current.agent !== undefined ? { agent: current.agent } : {}) }
  client._governanceContext.setProcessFrame(next)
}

/**
 * Run `fn` with `ctx` as the active user in the current async-local-storage
 * scope. Spans emitted inside `fn` carry the user identity without affecting
 * concurrent or subsequent calls. For a sticky process-level user use
 * {@link setUserContext}.
 */
export async function withUserContext<T>(
  ctx: UserContext,
  fn: () => T | Promise<T>,
): Promise<T> {
  const client = activeOrThrow()
  return client._governanceContext.runInContext({ user: ctx }, fn)
}

/**
 * Set process-level metadata that is attached to every span. Both fields are
 * optional; omitting a key leaves the current value unchanged. Typical values:
 * `environment: 'production'`, `releaseVersion: '1.2.3'`.
 */
export function setProcessContext(opts: {
  environment?: string
  releaseVersion?: string
}): void {
  const client = activeOrThrow()
  Object.assign(client.config, {
    ...(opts.environment !== undefined ? { environment: opts.environment } : {}),
    ...(opts.releaseVersion !== undefined ? { releaseVersion: opts.releaseVersion } : {}),
  })
}

export interface RequireApprovalOptions {
  context?: Record<string, unknown>
  timeoutMs?: number
  signal?: AbortSignal
  pollIntervalMs?: number
}

/**
 * Request human approval for `action` and poll until a decision is returned.
 * Resolves with the approval ID string on approval; throws
 * `JamjetApprovalRejected` on rejection or `JamjetApprovalTimeout` if no
 * decision arrives within `opts.timeoutMs` (default: 30 000 ms). Pass
 * `opts.signal` to cancel the poll via an {@link AbortSignal}.
 */
export async function requireApproval(
  action: string,
  opts: RequireApprovalOptions = {},
): Promise<string> {
  const client = activeOrThrow()
  return pollUntilResolved({
    apiKey: client.config.apiKey,
    apiUrl: client.config.apiUrl,
    action,
    ...(opts.context !== undefined ? { context: opts.context } : {}),
    ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
    ...(opts.pollIntervalMs !== undefined ? { pollIntervalMs: opts.pollIntervalMs } : {}),
    ...(opts.signal !== undefined ? { signal: opts.signal } : {}),
  })
}
