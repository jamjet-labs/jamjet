use crate::event::EventSequence;
use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn default_status_pending() -> WorkflowStatus {
    WorkflowStatus::Pending
}

/// A point-in-time snapshot of workflow state.
///
/// Each snapshot carries the full materialized state so that
/// `materialize()` can resume from it without replaying all prior events.
///
/// Current state = latest snapshot + events with sequence > snapshot.at_sequence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: Uuid,
    pub execution_id: ExecutionId,
    /// The event sequence number at which this snapshot was taken.
    pub at_sequence: EventSequence,
    /// Materialized workflow state (current_state) at this point.
    pub state: serde_json::Value,
    /// Workflow lifecycle status at snapshot time.
    #[serde(default = "default_status_pending")]
    pub status: WorkflowStatus,
    /// All nodes that had reached a terminal state at snapshot time,
    /// keyed by node_id with their output values.
    #[serde(default)]
    pub completed_nodes: std::collections::HashMap<String, serde_json::Value>,
    /// Nodes that were active (scheduled or running) at snapshot time.
    #[serde(default)]
    pub active_nodes: std::collections::HashSet<String>,
    /// The highest event sequence number seen at snapshot time.
    #[serde(default)]
    pub last_sequence: EventSequence,
    pub created_at: DateTime<Utc>,
}

impl Snapshot {
    /// Create a snapshot from explicit fields. New fields default to neutral
    /// values so existing callers do not need to change.
    pub fn new(
        execution_id: ExecutionId,
        at_sequence: EventSequence,
        state: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            execution_id,
            at_sequence,
            state,
            status: WorkflowStatus::Pending,
            completed_nodes: std::collections::HashMap::new(),
            active_nodes: std::collections::HashSet::new(),
            last_sequence: at_sequence,
            created_at: Utc::now(),
        }
    }

    /// Build a snapshot directly from a `MaterializedState`, capturing the
    /// full per-turn resume base.
    pub fn from_materialized(
        execution_id: ExecutionId,
        mat: &crate::materializer::MaterializedState,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            execution_id,
            at_sequence: mat.last_sequence,
            state: mat.current_state.clone(),
            status: mat.status.clone(),
            completed_nodes: mat.completed_nodes.clone(),
            active_nodes: mat.active_nodes.clone(),
            last_sequence: mat.last_sequence,
            created_at: Utc::now(),
        }
    }
}

/// Default number of events between automatic snapshots.
pub const DEFAULT_SNAPSHOT_INTERVAL: i64 = 50;
