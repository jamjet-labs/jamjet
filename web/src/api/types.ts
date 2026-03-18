// ─── Execution ────────────────────────────────────────────────────────────────

export interface Execution {
  id: string
  workflow_id: string
  status: 'pending' | 'running' | 'completed' | 'failed' | 'cancelled'
  created_at: string
}

// ─── Event kinds (discriminated union on `type`) ───────────────────────────────

export type WorkflowStartedEvent = { type: 'workflow_started'; workflow_id: string }
export type WorkflowCompletedEvent = { type: 'workflow_completed'; workflow_id: string }
export type WorkflowFailedEvent = { type: 'workflow_failed'; workflow_id: string; error: string }

export type NodeScheduledEvent = { type: 'node_scheduled'; node_id: string }
export type NodeStartedEvent = { type: 'node_started'; node_id: string }
export type NodeCompletedEvent = { type: 'node_completed'; node_id: string }
export type NodeFailedEvent = { type: 'node_failed'; node_id: string; error: string }

export type CoordinatorDiscoveryEvent = {
  type: 'coordinator_discovery'
  node_id: string
  candidates: string[]
}
export type CoordinatorScoringEvent = {
  type: 'coordinator_scoring'
  node_id: string
  scores: ScoringEntry[]
}
export type CoordinatorDecisionEvent = {
  type: 'coordinator_decision'
  node_id: string
  selected: string
  reasoning?: string
}

export type AgentToolInvokedEvent = { type: 'agent_tool_invoked'; tool: string; input: unknown }
export type AgentToolCompletedEvent = { type: 'agent_tool_completed'; tool: string; output: unknown }
export type AgentToolProgressEvent = { type: 'agent_tool_progress'; tool: string; progress: unknown }
export type AgentTurnEvent = { type: 'agent_turn'; turn: number; content: string }
export type AgentTerminatedEvent = { type: 'agent_terminated'; reason: string }
export type AgentFailedEvent = { type: 'agent_failed'; error: string }

export type UnknownEvent = { type: string; [key: string]: unknown }

export type EventKind =
  | WorkflowStartedEvent
  | WorkflowCompletedEvent
  | WorkflowFailedEvent
  | NodeScheduledEvent
  | NodeStartedEvent
  | NodeCompletedEvent
  | NodeFailedEvent
  | CoordinatorDiscoveryEvent
  | CoordinatorScoringEvent
  | CoordinatorDecisionEvent
  | AgentToolInvokedEvent
  | AgentToolCompletedEvent
  | AgentToolProgressEvent
  | AgentTurnEvent
  | AgentTerminatedEvent
  | AgentFailedEvent
  | UnknownEvent

// ─── Event ────────────────────────────────────────────────────────────────────

export interface Event {
  id: string
  execution_id: string
  sequence: number
  kind: EventKind
  created_at: string
}

// ─── Scoring ──────────────────────────────────────────────────────────────────

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
