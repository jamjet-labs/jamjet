export type SpanInit = {
  traceId: string
  spanId: string
  kind: string
  name: string
}

export type SpanEventDict = {
  type: 'span'
  trace_id: string
  span_id: string
  kind: string
  name: string
  sequence: number
  timestamp: string
  status: string
  parent_span_id?: string
  duration_ms?: number
  model?: string
  input_tokens?: number
  output_tokens?: number
  cost_usd?: number
  payload?: Record<string, unknown>
  agent_name?: string
  agent_card_uri?: string
  originating_trace_id?: string
  originating_span_id?: string
  originating_agent_name?: string
  session_id?: string
  environment?: string
  release_version?: string
  end_user_id?: string
  end_user_email?: string
  user_id?: string
  user_email?: string
  user_attrs?: Record<string, string | number | boolean>
  policy_decisions?: Array<{ tool_name: string; policy_kind: string; pattern: string | null }>
  policy_blocked_tool_calls?: Array<{ id?: string; name: string }>
  approval_id?: string
  budget_check?: { estimated: number; allowed: boolean }
}

export class Span {
  traceId: string
  spanId: string
  kind: string
  name: string
  parentSpanId: string | null = null
  sequence = 0
  timestamp: Date = new Date()
  durationMs: number | null = null
  model: string | null = null
  inputTokens: number | null = null
  outputTokens: number | null = null
  costUsd: number | null = null
  status = 'pending'
  payload: Record<string, unknown> = {}
  agentName: string | null = null
  agentCardUri: string | null = null
  originatingTraceId: string | null = null
  originatingSpanId: string | null = null
  originatingAgentName: string | null = null
  sessionId: string | null = null
  environment: string | null = null
  releaseVersion: string | null = null
  endUserId: string | null = null
  endUserEmail: string | null = null
  userId?: string
  userEmail?: string
  userAttrs?: Record<string, string | number | boolean>
  policyDecisions?: Array<{ tool_name: string; policy_kind: string; pattern: string | null }>
  policyBlockedToolCalls?: Array<{ id?: string; name: string }>
  approvalId?: string
  budgetCheck?: { estimated: number; allowed: boolean }
  tags: string[] = []
  private startTime: number = performance.now()

  constructor(init: SpanInit) {
    this.traceId = init.traceId
    this.spanId = init.spanId
    this.kind = init.kind
    this.name = init.name
  }

  finish(status = 'ok', durationMs?: number): void {
    this.status = status
    this.durationMs = durationMs ?? performance.now() - this.startTime
  }

  toEventDict(): SpanEventDict {
    const d: SpanEventDict = {
      type: 'span',
      trace_id: this.traceId,
      span_id: this.spanId,
      kind: this.kind,
      name: this.name,
      sequence: this.sequence,
      timestamp: this.timestamp.toISOString(),
      status: this.status,
    }
    if (this.parentSpanId !== null) d.parent_span_id = this.parentSpanId
    if (this.durationMs !== null) d.duration_ms = Math.round(this.durationMs)
    if (this.model !== null) d.model = this.model
    if (this.inputTokens !== null) d.input_tokens = this.inputTokens
    if (this.outputTokens !== null) d.output_tokens = this.outputTokens
    if (this.costUsd !== null) d.cost_usd = this.costUsd
    if (Object.keys(this.payload).length > 0) d.payload = { ...this.payload }
    if (this.agentName !== null) d.agent_name = this.agentName
    if (this.agentCardUri !== null) d.agent_card_uri = this.agentCardUri
    if (this.originatingTraceId !== null) d.originating_trace_id = this.originatingTraceId
    if (this.originatingSpanId !== null) d.originating_span_id = this.originatingSpanId
    if (this.originatingAgentName !== null) d.originating_agent_name = this.originatingAgentName
    if (this.sessionId !== null) d.session_id = this.sessionId
    if (this.environment !== null) d.environment = this.environment
    if (this.releaseVersion !== null) d.release_version = this.releaseVersion
    if (this.endUserId !== null) d.end_user_id = this.endUserId
    if (this.endUserEmail !== null) d.end_user_email = this.endUserEmail
    if (this.userId !== undefined) d.user_id = this.userId
    if (this.userEmail !== undefined) d.user_email = this.userEmail
    if (this.userAttrs !== undefined) d.user_attrs = this.userAttrs
    if (this.policyDecisions !== undefined) d.policy_decisions = this.policyDecisions
    if (this.policyBlockedToolCalls !== undefined) d.policy_blocked_tool_calls = this.policyBlockedToolCalls
    if (this.approvalId !== undefined) d.approval_id = this.approvalId
    if (this.budgetCheck !== undefined) d.budget_check = this.budgetCheck
    if (this.tags.length > 0) {
      d.payload = { ...(d.payload ?? {}), tags: [...this.tags] }
    }
    return d
  }
}
