export const VERSION = '0.4.0-alpha.0'

export { init } from './init.js'
export { wrap } from './wrap.js'
export { Span } from './span.js'
export type { SpanEventDict, SpanInit } from './span.js'
export type { InitOptions, ResolvedConfig } from './config.js'
export { ConfigError } from './config.js'
export { redact, redactDict, DEFAULT_PII_TYPES } from './redaction.js'
export type { PiiType, RedactOptions } from './redaction.js'
export { TransportError } from './transport.js'

// Plan 2 governance API
export {
  agent,
  withAgent,
  policy,
  budget,
  requireApproval,
  setUserContext,
  withUserContext,
  setProcessContext,
} from './governance.js'
export type { RequireApprovalOptions } from './governance.js'
export type { AgentRef, UserContext } from './context.js'
export type { PolicyAction, PolicyDecision } from './policy.js'
export { PolicyEvaluator } from './policy.js'
export { BudgetManager } from './budget.js'
export {
  JamjetBudgetExceeded,
  JamjetPolicyBlocked,
  JamjetApprovalRejected,
  JamjetApprovalTimeout,
} from './errors.js'
export type { WrapOptions } from './wrap.js'

// Low-level accessor — for ecosystem packages that need to inspect whether a
// client has been initialised (e.g. @jamjet/cloud-vercel middleware).
export { getActive } from './client.js'

// Cost utility — exported for ecosystem packages (e.g. @jamjet/cloud-vercel middleware).
export { estimateCost } from './cost.js'

// Phase 2 Node-only utilities (loadPolicy / AuditWriter / ApprovalQueue) are
// exported from `@jamjet/cloud/node` instead of the universal entry — they use
// node:fs / node:crypto and would break the universal bundle for Cloudflare
// Workers / Vercel Edge / browser consumers. Phase 2 adapters that need them
// import from '@jamjet/cloud/node'.
//
// Types describing the file-format schemas (Policy, AuditEventInput, etc.) are
// safe to expose universally — type-only re-exports follow.
export type { Policy, PolicyRule, PolicyBudget } from './load-policy.js'
export type {
  AdapterName,
  HostName,
  Decision,
  AuditEventInput,
  AuditWriterOptions,
} from './audit-writer.js'
export type {
  PendingApproval,
  ApprovalResult,
  ApprovalQueueOptions,
} from './approval-queue.js'

// Cloud Sync v0.1 direct-push (Path B). Universal exports because they're
// fetch-based + env-var-driven — no node:fs / node:crypto entanglement.
export { CloudPusher } from './cloud-pusher.js'
export type { CloudPusherOptions, CloudPusherEvent } from './cloud-pusher.js'
export { detectPathMode } from './path-mode.js'
export type { PathMode } from './path-mode.js'
export { parseTraceparent, readTraceparent } from './trace-context.js'
export type { Traceparent, TraceContextSource } from './trace-context.js'
