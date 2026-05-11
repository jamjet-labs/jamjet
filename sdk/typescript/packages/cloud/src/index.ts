export const VERSION = '0.2.2'

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

// Policy loader (v1 policy.yaml) — used by Phase 2 adapters
// (claude-code-hook, openai-guardrail, mcp-shim) to load policy from the
// canonical lookup order (explicit path > env > cwd > ~/.jamjet/).
export { loadPolicy } from './load-policy.js'
export type { Policy, PolicyRule, PolicyBudget } from './load-policy.js'

// Audit writer (v1 JSONL schema) — append-only, daily rotation by default
export { AuditWriter } from './audit-writer.js'
export type {
  AdapterName,
  HostName,
  Decision,
  AuditEventInput,
  AuditWriterOptions,
} from './audit-writer.js'
