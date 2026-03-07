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
            | Self::ChildWorkflowStarted { node_id, .. }
            | Self::ChildWorkflowCompleted { node_id, .. }
            | Self::ChildWorkflowFailed { node_id, .. } => Some(node_id.as_str()),
            _ => None,
        }
    }
}
