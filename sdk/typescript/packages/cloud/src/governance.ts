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

export async function withAgent<T>(
  ref: AgentRef,
  fn: () => T | Promise<T>,
): Promise<T> {
  const client = activeOrThrow()
  return client._governanceContext.runInContext({ agent: ref }, fn)
}

export function policy(action: PolicyAction, tools: string): void {
  activeOrThrow()._policy.add(action, tools)
}

export function budget(maxCostUsd: number): void {
  activeOrThrow()._budget.setLimit(maxCostUsd)
}

export function setUserContext(ctx: UserContext | null): void {
  const client = activeOrThrow()
  const current = client._governanceContext.getCurrentContext() ?? {}
  const next: ScopeFrame = ctx
    ? { ...current, user: ctx }
    : { ...(current.agent !== undefined ? { agent: current.agent } : {}) }
  client._governanceContext.setProcessFrame(next)
}

export async function withUserContext<T>(
  ctx: UserContext,
  fn: () => T | Promise<T>,
): Promise<T> {
  const client = activeOrThrow()
  return client._governanceContext.runInContext({ user: ctx }, fn)
}

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
