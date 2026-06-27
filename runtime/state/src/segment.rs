//! Continue-as-new: create the state-seeded continuation execution (segment N+1).
//!
//! A long-running durable agent bounds its event-log/replay cost by rolling over
//! to a new execution at a strategy ceiling. `start_next_segment` mirrors the
//! execution-creation pattern in `api/src/routes.rs` (WorkflowStarted seq 1 +
//! NodeScheduled seq 2 + work-item enqueue), substituting the carried
//! `MaterializedState.current_state` as the new execution's seed.
//!
//! The seed snapshot written here means `materialize(new_id)` loads only the new
//! segment's own events (bounded replay), not the parent's full history.

use crate::backend::{BackendResult, StateBackend, WorkItem};
use crate::event::{Event, EventKind};
use crate::materializer::MaterializedState;
use crate::snapshot::Snapshot;
use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use uuid::Uuid;

/// Create segment N+1: a fresh execution linked to `parent_id`, seeded with the
/// carried materialized state, ready to resume the workflow from its start node.
///
/// Mirrors `routes.rs` execution-creation (WorkflowStarted seq 1 + NodeScheduled
/// seq 2 + work-item enqueue). The seed snapshot written here ensures that
/// `materialize(new_id)` returns the carried state, not an empty base.
///
/// # State carry invariant
/// `new_exec.initial_input == new_exec.current_state == carried_state.current_state`
/// (byte-identical canonical JSON).
///
/// # Returns
/// The `ExecutionId` of the newly created segment execution.
// Nine parameters are required by the plan interface; suppressing the lint rather
// than breaking the caller signature with a builder/struct at this stage.
#[allow(clippy::too_many_arguments)]
pub async fn start_next_segment(
    backend: &dyn StateBackend,
    parent_id: &ExecutionId,
    carried_state: &MaterializedState,
    workflow_id: &str,
    workflow_version: &str,
    next_segment_number: u32,
    start_node_id: &str,
    queue_type: &str,
    tenant_id: &str,
) -> BackendResult<ExecutionId> {
    let new_id = ExecutionId::new();
    let now = Utc::now();

    // Step 1: create the new execution row with carried state as seed.
    backend
        .create_execution(WorkflowExecution {
            execution_id: new_id.clone(),
            workflow_id: workflow_id.to_string(),
            workflow_version: workflow_version.to_string(),
            status: WorkflowStatus::Running,
            initial_input: carried_state.current_state.clone(),
            current_state: carried_state.current_state.clone(),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: Some(parent_id.clone()),
            segment_number: next_segment_number,
        })
        .await?;

    // Step 2: write the seed snapshot so `materialize(new_id)` picks up the
    // carried state as its base without replaying the parent's event log.
    let seed = Snapshot::from_materialized(new_id.clone(), carried_state);
    backend.write_snapshot(seed).await?;

    // Step 3: WorkflowStarted at sequence 1 (mirrors routes.rs).
    backend
        .append_event(Event::new(
            new_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: workflow_id.to_string(),
                workflow_version: workflow_version.to_string(),
                initial_input: carried_state.current_state.clone(),
            },
        ))
        .await?;

    // Step 4: NodeScheduled at sequence 2 (mirrors routes.rs).
    backend
        .append_event(Event::new(
            new_id.clone(),
            2,
            EventKind::NodeScheduled {
                node_id: start_node_id.to_string(),
                queue_type: queue_type.to_string(),
            },
        ))
        .await?;

    // Step 5: enqueue the start-node work item (mirrors routes.rs).
    backend
        .enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: new_id.clone(),
            node_id: start_node_id.to_string(),
            queue_type: queue_type.to_string(),
            payload: serde_json::json!({
                "workflow_id": workflow_id,
                "workflow_version": workflow_version,
            }),
            attempt: 0,
            max_attempts: 3,
            created_at: now,
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: tenant_id.to_string(),
        })
        .await?;

    Ok(new_id)
}
