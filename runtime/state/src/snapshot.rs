use crate::event::EventSequence;
use chrono::{DateTime, Utc};
use jamjet_core::workflow::ExecutionId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A point-in-time snapshot of workflow state.
///
/// Current state = latest snapshot + events with sequence > snapshot.at_sequence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: Uuid,
    pub execution_id: ExecutionId,
    /// The event sequence number at which this snapshot was taken.
    pub at_sequence: EventSequence,
    /// Materialized workflow state at this point.
    pub state: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Snapshot {
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
            created_at: Utc::now(),
        }
    }
}

/// Default number of events between automatic snapshots.
pub const DEFAULT_SNAPSHOT_INTERVAL: i64 = 50;
