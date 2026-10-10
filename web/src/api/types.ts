// These types mirror the runtime's serde output (runtime/core/src/workflow.rs
// for executions, runtime/state/src/event.rs for events). Field names must
// match the JSON exactly; the runtime does not rename anything for the UI.

// ─── Execution ────────────────────────────────────────────────────────────────

export type ExecutionStatus =
  | 'pending'
  | 'running'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'cancelled'
  | 'limit_exceeded'

export interface Execution {
  execution_id: string
  workflow_id: string
  workflow_version: string
  status: ExecutionStatus
  initial_input: unknown
  current_state: unknown
  started_at: string
  updated_at: string
  completed_at: string | null
  session_type?: string
  parent_execution_id?: string
  segment_number?: number
}

// ─── Event kinds (discriminated union on `type`) ───────────────────────────────

export type WorkflowStartedEvent = {
  type: 'workflow_started'
  workflow_id: string
  workflow_version: string
  initial_input: unknown
}
export type WorkflowCompletedEvent = { type: 'workflow_completed'; final_state: unknown }
export type WorkflowFailedEvent = { type: 'workflow_failed'; error: string }
export type WorkflowCancelledEvent = { type: 'workflow_cancelled'; reason: string | null }

export type NodeScheduledEvent = { type: 'node_scheduled'; node_id: string; queue_type: string }
export type NodeStartedEvent = {
  type: 'node_started'
  node_id: string
  worker_id: string
  attempt: number
}
export type NodeCompletedEvent = {
  type: 'node_completed'
  node_id: string
  output: unknown
  state_patch: unknown
  duration_ms: number
  cost_usd?: number
  provenance?: Provenance
}
export type NodeFailedEvent = {
  type: 'node_failed'
  node_id: string
  error: string
  attempt: number
  retryable: boolean
}

/** One discovered agent (scheduler AgentCandidate). */
export interface AgentCandidate {
  uri: string
  skills?: string[]
  [key: string]: unknown
}

export type CoordinatorDiscoveryEvent = {
  type: 'coordinator_discovery'
  node_id: string
  query_skills: string[]
  query_trust_domain: string | null
  candidates: AgentCandidate[]
  filtered_out: unknown[]
}
export type CoordinatorScoringEvent = {
  type: 'coordinator_scoring'
  node_id: string
  rankings: ScoringEntry[]
  spread: number
  weights: unknown
}
export type CoordinatorDecisionEvent = {
  type: 'coordinator_decision'
  node_id: string
  selected: string | null
  method: string
  reasoning: string | null
  confidence: number
  rejected: unknown[]
}

export type AgentToolInvokedEvent = {
  type: 'agent_tool_invoked'
  node_id: string
  agent_uri: string
  mode: string
  protocol: string
  input_hash: string
}
export type AgentToolProgressEvent = {
  type: 'agent_tool_progress'
  node_id: string
  chunk_index: number
  partial_output_summary: string
}
export type AgentToolTurnEvent = {
  type: 'agent_tool_turn'
  node_id: string
  turn_number: number
  direction: string
  content_summary: string
  tokens: number
  cost: number
}
export type AgentToolCompletedEvent = {
  type: 'agent_tool_completed'
  node_id: string
  output: unknown
  provenance: unknown
  total_cost: number
  latency_ms: number
  total_turns: number | null
}
export type AgentToolTerminatedEvent = {
  type: 'agent_tool_terminated'
  node_id: string
  reason: string
  chunks_received: number
  partial_output: unknown
  cost: number
}
export type AgentToolFailedEvent = {
  type: 'agent_tool_failed'
  node_id: string
  failure_type: string
  message: string
  retryable: boolean
}

export type UnknownEvent = { type: string; [key: string]: unknown }

export type EventKind =
  | WorkflowStartedEvent
  | WorkflowCompletedEvent
  | WorkflowFailedEvent
  | WorkflowCancelledEvent
  | NodeScheduledEvent
  | NodeStartedEvent
  | NodeCompletedEvent
  | NodeFailedEvent
  | CoordinatorDiscoveryEvent
  | CoordinatorScoringEvent
  | CoordinatorDecisionEvent
  | AgentToolInvokedEvent
  | AgentToolProgressEvent
  | AgentToolTurnEvent
  | AgentToolCompletedEvent
  | AgentToolTerminatedEvent
  | AgentToolFailedEvent
  | UnknownEvent

// ─── Event ────────────────────────────────────────────────────────────────────

export interface Event {
  id: string
  execution_id: string
  sequence: number
  kind: EventKind
  created_at: string
}

// ─── Response envelopes ───────────────────────────────────────────────────────

/** GET /executions */
export interface ExecutionsResponse {
  executions: Execution[]
}

/** GET /executions/:id/events and GET /executions/:id/coordinator-decisions */
export interface EventsResponse {
  events: Event[]
}

/** GET /executions/:id/nodes/:node_id/scoring (coordinator_scoring events) */
export interface NodeScoringResponse {
  node_id: string
  scoring: Event[]
}

/** GET /executions/:id/nodes/:node_id/reasoning (coordinator_decision events) */
export interface NodeReasoningResponse {
  node_id: string
  decisions: Event[]
}

// ─── Scoring ──────────────────────────────────────────────────────────────────

/** One ranked agent inside a coordinator_scoring event (scheduler ScoringEntry). */
export interface ScoringEntry {
  uri: string
  scores: Record<string, number>
  composite: number
}

// ─── Provenance ───────────────────────────────────────────────────────────────

export interface Provenance {
  model_id?: string
  confidence?: number
  source?: string
  trust_domain?: string
}

// ─── Workflow IR ──────────────────────────────────────────────────────────────

export interface WorkflowIrNode {
  id: string
  kind: string
  [key: string]: unknown
}

export interface WorkflowIrEdge {
  from: string
  to: string
  [key: string]: unknown
}

export interface WorkflowIr {
  workflow_id: string
  nodes: WorkflowIrNode[]
  edges: WorkflowIrEdge[]
  start_node: string
}
