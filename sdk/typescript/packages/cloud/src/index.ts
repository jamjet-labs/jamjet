export const VERSION = '0.2.0'

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
export {
  JamjetBudgetExceeded,
  JamjetPolicyBlocked,
  JamjetApprovalRejected,
  JamjetApprovalTimeout,
} from './errors.js'
export type { WrapOptions } from './wrap.js'
