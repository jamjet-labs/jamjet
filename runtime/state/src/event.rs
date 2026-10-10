use chrono::{DateTime, Utc};
use jamjet_core::node::NodeId;
use jamjet_core::workflow::ExecutionId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Monotonically increasing sequence number within a workflow execution.
pub type EventSequence = i64;

/// A durable, immutable record of a state transition. Events are appended
/// to the event log and never modified or deleted (except by compaction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub execution_id: ExecutionId,
    pub sequence: EventSequence,
    pub kind: EventKind,
    pub created_at: DateTime<Utc>,
}

impl Event {
    pub fn new(execution_id: ExecutionId, sequence: EventSequence, kind: EventKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            execution_id,
            sequence,
            kind,
            created_at: Utc::now(),
        }
    }
}

/// Provenance metadata attached to node completions for research traceability.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProvenanceMetadata {
    /// Model identifier used for this node (e.g., "claude-haiku-4-5-20251001")
    pub model_id: Option<String>,
    /// Model version or checkpoint (e.g., "20251001")
    pub model_version: Option<String>,
    /// Confidence score (0.0-1.0) if available
    pub confidence: Option<f64>,
    /// Whether the output was verified by another model/check
    #[serde(default)]
    pub verified: bool,
    /// Source identifier (e.g., "mcp:brave-search", "a2a:research-agent")
    pub source: Option<String>,
    /// Trust domain this output belongs to (e.g., "internal", "external:partner-org").
    pub trust_domain: Option<String>,
    /// References to supporting evidence (URIs, document IDs, event sequences).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

/// All possible event kinds in the JamJet event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    // ── Workflow lifecycle ───────────────────────────────────────────────
    WorkflowStarted {
        workflow_id: String,
        workflow_version: String,
        initial_input: serde_json::Value,
        /// Principal that started the run (`"token:<ApiToken.id>"`), used for
        /// separation of duties on approvals. A continue-as-new segment carries
        /// its run's initiator forward. `None` in dev mode and on old logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initiated_by: Option<String>,
    },
    WorkflowCompleted {
        final_state: serde_json::Value,
    },
    WorkflowFailed {
        error: String,
    },
    WorkflowCancelled {
        reason: Option<String>,
    },

    // ── Node lifecycle ───────────────────────────────────────────────────
    NodeScheduled {
        node_id: NodeId,
        queue_type: String,
    },
    NodeStarted {
        node_id: NodeId,
        worker_id: String,
        attempt: u32,
    },
    NodeCompleted {
        node_id: NodeId,
        output: serde_json::Value,
        /// JSON merge patch to apply to workflow state.
        state_patch: serde_json::Value,
        duration_ms: u64,
        // ── GenAI telemetry (populated for model nodes) ──────────────────────
        /// AI provider system (e.g. "anthropic", "openai"). None for non-model nodes.
        #[serde(skip_serializing_if = "Option::is_none")]
        gen_ai_system: Option<String>,
        /// Model name used.
        #[serde(skip_serializing_if = "Option::is_none")]
        gen_ai_model: Option<String>,
        /// Input tokens consumed.
        #[serde(skip_serializing_if = "Option::is_none")]
        input_tokens: Option<u64>,
        /// Output tokens generated.
        #[serde(skip_serializing_if = "Option::is_none")]
        output_tokens: Option<u64>,
        /// Finish reason (e.g. "stop", "length", "tool_calls").
        #[serde(skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        /// Estimated USD cost for this node.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// Provenance metadata for research traceability.
        #[serde(skip_serializing_if = "Option::is_none")]
        provenance: Option<Box<ProvenanceMetadata>>,
        /// Deterministic idempotency key for this node fire:
        /// `content_hash({run, segment, step, node, input_hash})`.
        /// Including `node` in the key prevents cross-node collisions when two
        /// different nodes share the same `(run, segment, step, input_hash)` —
        /// e.g. sibling nodes in the same segment at the same step count.
        /// When set, `commit_turn` records the result in `tool_effects` atomically.
        /// The worker reads it back before re-firing a reclaimed node (Task 2).
        #[serde(skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    NodeFailed {
        node_id: NodeId,
        error: String,
        attempt: u32,
        retryable: bool,
    },
    /// Emitted when a work item is parked on a provider rate-limit (429).
    /// The item is reset to `pending` with `retry_after` set; the node stays
    /// in `scheduled` and is re-picked up by a worker when the not-before
    /// time passes. This is an audit/observability event — it does not mutate
    /// the ExecProgress state (the node is already in `scheduled`).
    NodeParked {
        node_id: NodeId,
        attempt: u32,
        /// RFC3339 timestamp (now + retry_after_secs) stored as TEXT, matching
        /// the `retry_after` column type in `work_items`.
        retry_after: String,
    },
    NodeSkipped {
        node_id: NodeId,
        reason: String,
    },
    NodeCancelled {
        node_id: NodeId,
    },

    // ── Retry ────────────────────────────────────────────────────────────
    RetryScheduled {
        node_id: NodeId,
        attempt: u32,
        delay_ms: u64,
    },

    // ── Human approval / interrupt ────────────────────────────────────────
    InterruptRaised {
        node_id: NodeId,
        reason: String,
        state_for_review: serde_json::Value,
    },
    ApprovalReceived {
        node_id: NodeId,
        /// Display name of the approver. With auth on, the API token's name; in
        /// dev mode, whatever the caller supplied.
        user_id: String,
        decision: ApprovalDecision,
        comment: Option<String>,
        state_patch: Option<serde_json::Value>,
        /// Authenticated identity behind the decision, `"token:<ApiToken.id>"`.
        /// `None` in dev mode (no auth) and on events written before it existed.
        /// Unlike `user_id` it is unique and not caller-chosen.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<String>,
        /// The `action_digest` this approval authorises (see
        /// `approvals::action_digest`). Equals the request's digest, or, when
        /// the approver supplied a `state_patch`, the digest of the patched
        /// state. The worker fires only if the digest it computes at resume
        /// matches. `None` on rejections, on approvals of requests that carry
        /// no digest, and on old logs; a `None` never authorises a digest-bound
        /// node.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approved_digest: Option<String>,
        /// Sequence of the `ToolApprovalRequired` this decision answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_sequence: Option<i64>,
    },

    // ── Timers ────────────────────────────────────────────────────────────
    TimerCreated {
        node_id: NodeId,
        fire_at: DateTime<Utc>,
        correlation_key: Option<String>,
    },
    TimerFired {
        node_id: NodeId,
        correlation_key: Option<String>,
    },

    // ── External events ───────────────────────────────────────────────────
    ExternalEventReceived {
        correlation_key: String,
        payload: serde_json::Value,
    },

    // ── Child workflows ───────────────────────────────────────────────────
    ChildWorkflowStarted {
        node_id: NodeId,
        child_execution_id: String,
        child_workflow_id: String,
    },
    ChildWorkflowCompleted {
        node_id: NodeId,
        child_execution_id: String,
        result: serde_json::Value,
    },
    ChildWorkflowFailed {
        node_id: NodeId,
        child_execution_id: String,
        error: String,
    },

    // ── Budget / autonomy ─────────────────────────────────────────────────
    BudgetExceeded {
        node_id: NodeId,
        kind: String,
        limit: u64,
        current: u64,
    },
    TokenBudgetExceeded {
        node_id: NodeId,
        /// "input_tokens" | "output_tokens" | "total_tokens"
        kind: String,
        limit: u64,
        current: u64,
    },
    CostBudgetExceeded {
        node_id: NodeId,
        limit_usd: f64,
        current_usd: f64,
    },
    AutonomyLimitReached {
        node_id: NodeId,
        agent_ref: String,
        /// "max_iterations" | "max_tool_calls" | "token_budget" | "cost_budget"
        /// | "time_budget"
        limit_type: String,
        limit_value: serde_json::Value,
        actual_value: serde_json::Value,
    },
    CircuitBreakerTripped {
        node_id: NodeId,
        agent_ref: String,
        consecutive_errors: u32,
        threshold: u32,
    },
    /// A node acting for one or more agents (`agent_ref`) was admitted to
    /// fire: one step of each of `agents`, using `tool_calls` tool calls.
    /// Appended by the shared guard (`node_guard`) on both transports, only
    /// when every agent's limits allow the step (or an approval covers it),
    /// in the same compare-and-set that checked them. The autonomy counters
    /// (iterations, tool calls, time, and through the node's later
    /// `NodeCompleted` / `NodeFailed`, tokens, cost and consecutive errors)
    /// are folded from these, so they survive a restart and a rollover.
    AgentStepAdmitted {
        node_id: NodeId,
        /// The work item that was admitted. A result reported over HTTP is
        /// recorded only for an admitted item.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_item_id: Option<Uuid>,
        /// Every agent the node acts for (the workflow's and the node's
        /// `agent_ref`), each charged one step.
        agents: Vec<String>,
        /// Tool calls this step makes: one for a node that uses a tool, the
        /// number of calls in an agent tool dispatch batch.
        tool_calls: u32,
        /// For a step past a limit released by a human (`escalate_to_human`):
        /// the sequence of the approval request it used. Each request
        /// releases one step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval_sequence: Option<i64>,
    },
    EscalationRequired {
        node_id: NodeId,
        agent_ref: String,
        /// "circuit_breaker" | "autonomy_limit" | "budget_exceeded"
        reason: String,
        /// "supervisor_agent:<id>" | "human_approval"
        escalation_target: String,
    },

    // ── Policy ────────────────────────────────────────────────────────────
    PolicyViolation {
        node_id: NodeId,
        /// Which rule triggered (e.g. "block_tool:payments.*")
        rule: String,
        /// "blocked" | "require_approval"
        decision: String,
        /// "global" | "tenant" | "workflow" | "node"
        policy_scope: String,
    },
    ToolApprovalRequired {
        node_id: NodeId,
        tool_name: String,
        approver: String,
        context: serde_json::Value,
    },
    /// The request at `request_sequence` reached its deadline undecided. A
    /// rejection by [`DEADLINE_PRINCIPAL`]: the node fails as on a rejection,
    /// and a later decision is refused. Appended by the scheduler's deadline
    /// sweep only while that request is still pending, in the same step that
    /// checks it, so it never lands beside a human decision of that request.
    ApprovalExpired {
        node_id: NodeId,
        /// Sequence of the `ToolApprovalRequired` that expired.
        request_sequence: i64,
        /// The deadline the request carried (`context.expires_at`).
        expires_at: DateTime<Utc>,
        /// Always [`DEADLINE_PRINCIPAL`].
        principal: String,
    },
    /// The request at `request_sequence` is still pending at its
    /// `escalate_at`. Recorded once per request, for external notifiers (the
    /// hook point for Slack or email routing); it changes no approval state.
    ApprovalEscalated {
        node_id: NodeId,
        /// Sequence of the `ToolApprovalRequired` that escalated.
        request_sequence: i64,
        /// When it was due to escalate (`context.escalate_at`).
        escalate_at: DateTime<Utc>,
        /// When it will expire (`context.expires_at`).
        expires_at: DateTime<Utc>,
    },

    /// An event of this run could not be sealed into the signed audit chain
    /// (the audit write failed, or its tenant could not be read). The event
    /// itself is durable; this marks the gap so it can be found, since
    /// `verify_chain` cannot see an entry that was never written. Changes no
    /// state.
    AuditSealFailed {
        /// The `Event.id` of the unsealed event.
        event_id: Uuid,
        /// Its sequence in this run's log.
        sequence: i64,
        /// Its `type` tag (e.g. `tool_approval_required`).
        event_type: String,
        /// Why the seal failed.
        error: String,
    },

    /// The scheduler stopped advancing this run: its log holds an event kind
    /// this build cannot read (`Unknown`), so it cannot tell what the run may
    /// do next. Recorded once; the run stays as it is until a build that
    /// reads the whole log takes over.
    RunStalled {
        reason: String,
    },

    // ── Reasoning strategy lifecycle (§14.5) ─────────────────────────────
    /// Emitted when a reasoning strategy begins execution.
    StrategyStarted {
        strategy: String,
        config: serde_json::Value,
    },
    /// Emitted by plan-and-execute when the plan is generated.
    PlanGenerated {
        steps: Vec<String>,
    },
    /// Emitted at the start of each reasoning loop iteration.
    IterationStarted {
        iteration: u32,
    },
    /// Emitted each time a tool is invoked within a strategy loop.
    ToolCalled {
        node_id: NodeId,
        tool: String,
    },
    /// Emitted by critic/verifier nodes with a quality score.
    CriticVerdict {
        node_id: NodeId,
        score: f64,
        passed: bool,
        feedback: Option<String>,
    },
    /// Emitted at the end of each iteration with cost/token delta.
    IterationCompleted {
        iteration: u32,
        cost_delta_usd: Option<f64>,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Emitted when a strategy limit (max_iterations, max_cost_usd, timeout) is hit.
    /// Workflow transitions to `LimitExceeded` after this event.
    StrategyLimitHit {
        limit_type: String,
        limit_value: serde_json::Value,
        actual_value: serde_json::Value,
    },
    /// Emitted when strategy execution completes successfully.
    StrategyCompleted {
        iterations: u32,
        total_cost_usd: Option<f64>,
    },

    // ── Coordinator events ────────────────────────────────────────────────
    CoordinatorDiscovery {
        node_id: NodeId,
        query_skills: Vec<String>,
        query_trust_domain: Option<String>,
        candidates: Vec<serde_json::Value>,
        filtered_out: Vec<serde_json::Value>,
    },
    CoordinatorScoring {
        node_id: NodeId,
        rankings: Vec<serde_json::Value>,
        spread: f64,
        weights: serde_json::Value,
    },
    CoordinatorDecision {
        node_id: NodeId,
        selected: Option<String>,
        method: String,
        reasoning: Option<String>,
        confidence: f64,
        rejected: Vec<serde_json::Value>,
        tiebreaker_tokens: Option<serde_json::Value>,
        tiebreaker_cost: Option<f64>,
    },

    // ── Agent-as-Tool events ──────────────────────────────────────────────
    AgentToolInvoked {
        node_id: NodeId,
        agent_uri: String,
        mode: String,
        protocol: String,
        input_hash: String,
    },
    AgentToolProgress {
        node_id: NodeId,
        chunk_index: u32,
        partial_output_summary: String,
    },
    AgentToolTurn {
        node_id: NodeId,
        turn_number: u32,
        direction: String,
        content_summary: String,
        tokens: u32,
        cost: f64,
    },
    AgentToolCompleted {
        node_id: NodeId,
        output: serde_json::Value,
        provenance: Option<serde_json::Value>,
        total_cost: f64,
        latency_ms: u64,
        total_turns: Option<u32>,
    },
    AgentToolTerminated {
        node_id: NodeId,
        reason: String,
        chunks_received: u32,
        partial_output: Option<serde_json::Value>,
        cost: f64,
    },
    AgentToolFailed {
        node_id: NodeId,
        failure_type: String,
        message: String,
        retryable: bool,
    },
    /// Audit record written to the OLD execution when a continue-as-new rollover
    /// occurs. Records which segment was created next and the new execution_id.
    /// Pure audit — does NOT mutate `MaterializedState`.
    SegmentBoundary {
        /// The segment_number of the newly created execution (old + 1).
        segment_number: u32,
        /// The execution_id of the new segment (as a string).
        next_execution_id: String,
    },

    /// An event kind this build does not know, written by a later release.
    /// Read instead of failing the whole log, so the log still loads and
    /// lists (its fields are dropped; re-serializing it writes only
    /// `{"type": "unknown"}`). It is NOT ignored where it could matter: it may
    /// be a revocation or a denial, so every fold that decides coverage or
    /// permission fails closed on it. The approval fold reports
    /// `NodeApprovalStatus::Indeterminate` (nothing released, no decision
    /// accepted) and the scheduler does not advance the run (`RunStalled`).
    #[serde(other)]
    Unknown,
}

/// The principal an expired approval request is rejected by.
pub const DEADLINE_PRINCIPAL: &str = "system:deadline";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved,
    Rejected,
}

impl EventKind {
    /// Returns the node_id associated with this event, if any.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::NodeScheduled { node_id, .. }
            | Self::NodeStarted { node_id, .. }
            | Self::NodeCompleted { node_id, .. }
            | Self::NodeFailed { node_id, .. }
            | Self::NodeParked { node_id, .. }
            | Self::NodeSkipped { node_id, .. }
            | Self::NodeCancelled { node_id }
            | Self::RetryScheduled { node_id, .. }
            | Self::InterruptRaised { node_id, .. }
            | Self::ApprovalReceived { node_id, .. }
            | Self::TimerCreated { node_id, .. }
            | Self::TimerFired { node_id, .. }
            | Self::BudgetExceeded { node_id, .. }
            | Self::TokenBudgetExceeded { node_id, .. }
            | Self::CostBudgetExceeded { node_id, .. }
            | Self::AutonomyLimitReached { node_id, .. }
            | Self::CircuitBreakerTripped { node_id, .. }
            | Self::AgentStepAdmitted { node_id, .. }
            | Self::EscalationRequired { node_id, .. }
            | Self::PolicyViolation { node_id, .. }
            | Self::ToolApprovalRequired { node_id, .. }
            | Self::ApprovalExpired { node_id, .. }
            | Self::ApprovalEscalated { node_id, .. }
            | Self::ChildWorkflowStarted { node_id, .. }
            | Self::ChildWorkflowCompleted { node_id, .. }
            | Self::ChildWorkflowFailed { node_id, .. }
            | Self::CoordinatorDiscovery { node_id, .. }
            | Self::CoordinatorScoring { node_id, .. }
            | Self::CoordinatorDecision { node_id, .. }
            | Self::AgentToolInvoked { node_id, .. }
            | Self::AgentToolProgress { node_id, .. }
            | Self::AgentToolTurn { node_id, .. }
            | Self::AgentToolCompleted { node_id, .. }
            | Self::AgentToolTerminated { node_id, .. }
            | Self::AgentToolFailed { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        }
    }
}
