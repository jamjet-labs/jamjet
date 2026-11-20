//! A finished run stays finished. A plain append still lands on it, as it
//! always has on every backend, but the run's status does not move and the
//! scheduler cannot write into it; a worker's commit into it is withdrawn
//! and records nothing.

use crate::conformance::cases::support::in_tenant;
use crate::conformance::Fixture;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    Event, EventKind, ExecutionFinish, PassCommit, SchedulerPass, StateBackend, TurnCommit,
    WorkItem, DEFAULT_TENANT,
};
use serde_json::json;
use std::sync::Arc;

/// The backend under test, the scoped stores in a tenant other than the
/// default.
async fn backend(fx: &Fixture) -> (String, Arc<dyn StateBackend>) {
    (format!("{:?}", fx.kind), in_tenant(fx, "acme").await)
}

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-terminal".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn item(execution_id: &ExecutionId, node: &str) -> WorkItem {
    WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: node.into(),
        queue_type: "general".into(),
        payload: json!({ "node_id": node }),
        attempt: 0,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

/// End the run with `status` and its terminal event. Returns the event's
/// sequence.
async fn finish(b: &dyn StateBackend, id: &ExecutionId, status: WorkflowStatus) -> i64 {
    let kind = match status {
        WorkflowStatus::Completed => EventKind::WorkflowCompleted {
            final_state: json!({}),
        },
        _ => EventKind::WorkflowCancelled { reason: None },
    };
    match b
        .finish_execution(Event::new(id.clone(), 0, kind), status)
        .await
        .unwrap()
    {
        ExecutionFinish::Finished(seq) => seq,
        other => panic!("the run was live: {other:?}"),
    }
}

/// A plain `append_event` on a completed run lands (it always has: the
/// method checks no status), the run stays completed, and a scheduler pass
/// that read the whole log is refused with `ExecutionTerminal` and writes
/// nothing.
pub async fn plain_append_on_a_finished_run_changes_nothing(fx: &Fixture) {
    let (name, b) = backend(fx).await;
    let id = ExecutionId::new();
    b.create_execution(run(&id)).await.unwrap();
    let ended = finish(&*b, &id, WorkflowStatus::Completed).await;

    let late = b
        .append_event(Event::new(
            id.clone(),
            0,
            EventKind::NodeStarted {
                node_id: "a".into(),
                worker_id: "w1".into(),
                attempt: 0,
            },
        ))
        .await
        .unwrap();
    assert_eq!(
        late,
        ended + 1,
        "{name}: the late append lands after the end"
    );
    let status = b.get_execution(&id).await.unwrap().unwrap().status;
    assert_eq!(status, WorkflowStatus::Completed, "{name}: still completed");

    let b_item = item(&id, "b");
    let pass = SchedulerPass {
        execution_id: id.clone(),
        seen_sequence: late,
        events: vec![EventKind::NodeScheduled {
            node_id: "b".into(),
            queue_type: "general".into(),
        }],
        work_items: vec![b_item.clone()],
        finish: None,
    };
    let out = b.commit_scheduler_pass(pass).await.unwrap();
    assert_eq!(
        out,
        PassCommit::ExecutionTerminal(WorkflowStatus::Completed),
        "{name}"
    );
    assert_eq!(
        b.latest_sequence(&id).await.unwrap(),
        late,
        "{name}: the refused pass wrote no event"
    );
    assert!(
        b.get_work_item(b_item.id).await.unwrap().is_none(),
        "{name}: the refused pass wrote no item"
    );
    let status = b.get_execution(&id).await.unwrap().unwrap().status;
    assert_eq!(status, WorkflowStatus::Completed, "{name}");
}

/// A worker holds an item of a run that is then cancelled. Its
/// `commit_turn_unless_terminal` answers `ExecutionTerminal`, records no
/// event, and withdraws the item: the lease is gone, so the same fence
/// settles nothing and no claim takes the item again.
pub async fn commit_turn_on_a_finished_run_withdraws(fx: &Fixture) {
    let (name, b) = backend(fx).await;
    let id = ExecutionId::new();
    b.create_execution(run(&id)).await.unwrap();
    let it = item(&id, "a");
    b.enqueue_work_item(it.clone()).await.unwrap();
    let held = b
        .claim_work_item("w1", &["general"])
        .await
        .unwrap()
        .expect("the item is claimable");
    assert_eq!(held.id, it.id, "{name}");
    let ended = finish(&*b, &id, WorkflowStatus::Cancelled).await;

    let completed = Event::new(
        id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "a".into(),
            output: json!({}),
            state_patch: json!({}),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: None,
        },
    );
    let out = b
        .commit_turn_unless_terminal(held.id, held.lease_fence, completed, true)
        .await
        .unwrap();
    assert_eq!(
        out,
        TurnCommit::ExecutionTerminal(WorkflowStatus::Cancelled),
        "{name}"
    );
    assert_eq!(
        b.latest_sequence(&id).await.unwrap(),
        ended,
        "{name}: no event after the end"
    );
    assert!(
        b.latest_snapshot(&id).await.unwrap().is_none(),
        "{name}: no snapshot either"
    );
    let row = fx.hooks.item_row(held.id).await;
    assert!(
        row.as_ref().is_none_or(|r| r.status == "failed"),
        "{name}: the item is withdrawn: {row:?}"
    );
    assert!(
        !b.complete_work_item_fenced(held.id, held.lease_fence)
            .await
            .unwrap(),
        "{name}: the withdrawn lease settles nothing"
    );
    assert!(
        b.claim_work_item("w2", &["general"])
            .await
            .unwrap()
            .is_none(),
        "{name}: no claim takes the withdrawn item"
    );
}
