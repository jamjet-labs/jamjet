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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProvenanceMetadata {
    /// Model identifier used for this node (e.g., "claude-haiku-4-5-20251001")
    pub model_id: Option<String>,
    /// Model version or checkpoint (e.g., "20251001")
    pub model_version: Option<String>,
    /// Confidence score (0.0-1.0) if available
    pub confidence: Option<f64>,
    /// Whether the output was verified by another model/check
    pub verified: bool,
    /// Source identifier (e.g., "mcp:brave-search", "a2a:research-agent")
    pub source: Option<String>,
}

impl Default for ProvenanceMetadata {
    fn default() -> Self {
        Self {
            model_id: None,
            model_version: None,
            confidence: None,
            verified: false,
            source: None,
        }
    }
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
        provenance: Option<ProvenanceMetadata>,
    },
    NodeFailed {
        node_id: NodeId,
        error: String,
        attempt: u32,
        retryable: bool,
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
        user_id: String,
        decision: ApprovalDecision,
        comment: Option<String>,
        state_patch: Option<serde_json::Value>,
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
        /// "max_iterations" | "cost_budget" | "token_budget" | "max_tool_calls"
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
}

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
            | Self::EscalationRequired { node_id, .. }
            | Self::PolicyViolation { node_id, .. }
            | Self::ToolApprovalRequired { node_id, .. }
            | Self::ChildWorkflowStarted { node_id, .. }
            | Self::ChildWorkflowCompleted { node_id, .. }
            | Self::ChildWorkflowFailed { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        }
    }
}
