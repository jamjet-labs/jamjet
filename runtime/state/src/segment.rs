//! Continue-as-new: create the state-seeded continuation execution (segment N+1)
//! and walk the segment chain.
//!
//! A long-running durable agent bounds its event-log/replay cost by rolling over
//! to a new execution at a strategy ceiling. `start_next_segment` mirrors the
//! execution-creation pattern in `api/src/routes.rs` (WorkflowStarted seq 1 +
//! NodeScheduled seq 2 + work-item enqueue), substituting the carried
//! `MaterializedState.current_state` as the new execution's seed.
//!
//! The seed snapshot written here means `materialize(new_id)` loads only the new
//! segment's own events (bounded replay), not the parent's full history.
//!
//! ## Continue-as-new caveats
//!
//! F-2g-timers: a timer that was registered against the OLD execution_id will not
//! automatically fire into the new segment. The timer table references
//! execution_id; after rollover, the old execution is in a terminal completed
//! state and the scheduler drops its ExecProgress. Any pending timer for the old
//! execution_id simply fires against a closed log and has no effect on the new
//! segment. Carry-over of pending timers across a rollover boundary is future work
//! (F-2g-timers).
//!
//! In-flight / parked items at rollover: the rollover fires at the strategy-limit
//! point (same semantics as the existing limit-terminate path). A work item that
//! was parked (retry_after = future) when the limit is hit will eventually be
//! claimed by a worker, execute, and attempt commit_turn against the OLD
//! (completed) execution. That commit lands on the closed event log and is inert:
//! it does not affect the new segment's state. This is safe but should be logged
//! by the worker's commit path. The test `inert_old_segment_does_not_leak` proves
//! that a late append to the old terminal execution leaves the new segment's
//! materialize output unchanged.

use crate::backend::{BackendResult, StateBackend, WorkItem};
use crate::event::{Event, EventKind};
use crate::materializer::MaterializedState;
use crate::snapshot::Snapshot;
use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use uuid::Uuid;

/// Stable UUID namespace for deterministic segment child IDs.
///
/// This constant is fixed forever — changing it would make existing segment
/// chains unresolvable after a crash-recovery cycle.  The value was chosen
/// as a random v4 UUID and then frozen.
const SEGMENT_NS: Uuid = Uuid::from_u128(0x6a8e_f5c3_d3a4_4b9c_8b2e_4f5d_6e7a_8c9bu128);

/// Create segment N+1: a fresh execution linked to `parent_id`, seeded with the
/// carried materialized state, ready to resume the workflow from its start node.
///
/// Mirrors `routes.rs` execution-creation (WorkflowStarted seq 1 + NodeScheduled
/// seq 2 + work-item enqueue). The seed snapshot written here ensures that
/// `materialize(new_id)` returns the carried state, not an empty base.
///
/// # Child ID stability (crash safety)
/// The child `ExecutionId` is derived deterministically via UUID v5 from the
/// parent id and segment number, so a re-run after a crash between
/// `start_next_segment` and the `SegmentBoundary` append always produces the
/// SAME child id.  Combined with the idempotency guard at the top of this
/// function (returns early if the child already exists), a double invocation
/// cannot fork the segment chain into two concurrent children.
///
/// # Seed snapshot sequence space (SQLite correctness)
/// The seed snapshot is anchored at `at_sequence = 0` (`Snapshot::seed_for_segment`),
/// NOT at the parent's `last_sequence`.  SQLite `get_events_since` filters with
/// `sequence > at_sequence`; seeding from the parent sequence would drop every
/// child event with a sequence <= that value (child events start at 1).
///
/// # State carry invariant
/// `new_exec.initial_input == new_exec.current_state == carried_state.current_state`
/// (byte-identical canonical JSON).
///
/// # Returns
/// The `ExecutionId` of the newly created (or pre-existing idempotent) segment.
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
    // Derive a DETERMINISTIC child id from the parent id + segment number.
    // This guarantees that a re-run after a crash always targets the same
    // child row, preventing a second random child from forking the chain.
    let child_uuid = Uuid::new_v5(
        &SEGMENT_NS,
        format!("{}:{}", parent_id.0, next_segment_number).as_bytes(),
    );
    let new_id = ExecutionId(child_uuid);

    // Idempotency guard: if a prior attempt already created the child (crash
    // between this function and the caller's SegmentBoundary append), return
    // the existing id without re-seeding or re-enqueuing.
    if backend.get_execution(&new_id).await?.is_some() {
        return Ok(new_id);
    }

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
    // IMPORTANT: seed at at_sequence=0 (child's own sequence space), NOT at
    // the parent's last_sequence.  See `Snapshot::seed_for_segment` for the
    // full rationale.
    let seed = Snapshot::seed_for_segment(new_id.clone(), &carried_state.current_state);
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

/// Maximum number of hops allowed when walking the segment chain. A well-formed
/// chain is acyclic; exceeding this limit indicates data corruption or a cycle.
const SEGMENT_CHAIN_MAX_DEPTH: usize = 10_000;

/// Walk the segment chain backward from any segment to the root, returning the
/// `ExecutionId`s from root..=given in ascending segment order.
///
/// The result is ordered root-first (lowest segment_number first). For a
/// 3-segment chain `root -> seg1 -> seg2`, `segment_chain(backend, &seg2_id)`
/// returns `[root_id, seg1_id, seg2_id]`.
///
/// # Errors
/// Returns an error if `get_execution` fails for any node in the chain, if any
/// node in the chain does not exist, or if the chain depth exceeds
/// [`SEGMENT_CHAIN_MAX_DEPTH`] (cycle guard).
pub async fn segment_chain(
    backend: &dyn StateBackend,
    id: &ExecutionId,
) -> BackendResult<Vec<ExecutionId>> {
    let mut chain: Vec<ExecutionId> = Vec::new();
    let mut current = id.clone();

    loop {
        if chain.len() >= SEGMENT_CHAIN_MAX_DEPTH {
            return Err(crate::backend::StateBackendError::Database(format!(
                "segment_chain exceeded max depth ({SEGMENT_CHAIN_MAX_DEPTH}): \
                 possible cycle starting near {current}"
            )));
        }

        let exec = backend.get_execution(&current).await?.ok_or_else(|| {
            crate::backend::StateBackendError::NotFound(format!(
                "segment_chain: execution {current} not found"
            ))
        })?;

        chain.push(current.clone());

        match exec.parent_execution_id {
            None => break,
            Some(parent) => current = parent,
        }
    }

    // We walked tail-to-root; reverse so root is first.
    chain.reverse();
    Ok(chain)
}
