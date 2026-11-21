//! The trait-level tests first written for `InMemoryBackend`: workflow and
//! run records, the event log, snapshots, the queue and fenced settles,
//! array patches and tokens.

use crate::conformance::{Fixture, StoreKind};
use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::WorkflowDefinition,
    event::{Event, EventKind},
    snapshot::Snapshot,
    WorkItem,
};
use serde_json::json;
use uuid::Uuid;

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-test".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "x": 1 }),
        current_state: json!({ "x": 1 }),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

pub async fn test_workflow_crud(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let def = WorkflowDefinition {
        workflow_id: "wf-1".into(),
        version: "1.0.0".into(),
        ir: json!({"nodes": []}),
        created_at: Utc::now(),
        tenant_id: "default".into(),
    };
    backend.store_workflow(def).await.unwrap();
    let loaded = backend.get_workflow("wf-1", "1.0.0").await.unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().workflow_id, "wf-1");
    assert!(backend
        .get_workflow("wf-1", "2.0.0")
        .await
        .unwrap()
        .is_none());
}

pub async fn test_execution_lifecycle(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&id))
        .await
        .unwrap();
    let exec = backend.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Running);
    backend
        .update_execution_status(&id, WorkflowStatus::Completed)
        .await
        .unwrap();
    let exec = backend.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Completed);
}

pub async fn test_list_executions_with_filter(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    for _ in 0..3 {
        backend
            .create_execution(sample_execution(&ExecutionId::new()))
            .await
            .unwrap();
    }
    let all = backend.list_executions(None, 100, 0).await.unwrap();
    assert_eq!(all.len(), 3);
    let running = backend
        .list_executions(Some(WorkflowStatus::Running), 100, 0)
        .await
        .unwrap();
    assert_eq!(running.len(), 3);
    let completed = backend
        .list_executions(Some(WorkflowStatus::Completed), 100, 0)
        .await
        .unwrap();
    assert_eq!(completed.len(), 0);
    let page = backend.list_executions(None, 2, 0).await.unwrap();
    assert_eq!(page.len(), 2);
    let page2 = backend.list_executions(None, 2, 2).await.unwrap();
    assert_eq!(page2.len(), 1);
}

pub async fn test_event_log(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let e1 = Event::new(
        exec_id.clone(),
        0,
        EventKind::WorkflowStarted {
            workflow_id: "wf-test".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({ "x": 1 }),
            initiated_by: None,
            run_key: None,
        },
    );
    let seq1 = backend.append_event(e1).await.unwrap();
    assert!(seq1 > 0);

    let e2 = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeScheduled {
            node_id: "n1".into(),
            queue_type: "default".into(),
        },
    );
    let seq2 = backend.append_event(e2).await.unwrap();
    assert!(seq2 > seq1);

    let events = backend.get_events(&exec_id).await.unwrap();
    assert_eq!(events.len(), 2);

    let since = backend.get_events_since(&exec_id, seq1).await.unwrap();
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].sequence, seq2);

    let latest = backend.latest_sequence(&exec_id).await.unwrap();
    assert_eq!(latest, seq2);
}

pub async fn test_snapshots(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    assert!(backend.latest_snapshot(&exec_id).await.unwrap().is_none());
    let snap = Snapshot::new(exec_id.clone(), 5, json!({"state": "checkpoint"}));
    backend.write_snapshot(snap).await.unwrap();
    let latest = backend.latest_snapshot(&exec_id).await.unwrap().unwrap();
    assert_eq!(latest.at_sequence, 5);
}

pub async fn test_work_queue(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
        node_id: "n1".into(),
        queue_type: "default".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    };
    let item_id = backend.enqueue_work_item(item).await.unwrap();
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, item_id);
    assert_eq!(claimed.worker_id.as_deref(), Some("w1"));
    // Second claim should find nothing (item is already claimed)
    assert!(backend
        .claim_work_item("w2", &["default"])
        .await
        .unwrap()
        .is_none());
    assert!(backend.complete_work_item(item_id).await.unwrap());
}

pub async fn test_complete_work_item_fenced(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id,
        node_id: "n1".into(),
        queue_type: "default".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    };
    let item_id = backend.enqueue_work_item(item).await.unwrap();
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .unwrap();
    let fence = claimed.lease_fence;
    assert!(fence > 0);

    // Mismatched fence settles nothing; matching fence settles exactly once; a
    // repeat settle is a no-op (item already removed) — exactly-once-COMMIT.
    assert!(!backend
        .complete_work_item_fenced(item_id, fence + 1)
        .await
        .unwrap());
    assert!(backend
        .complete_work_item_fenced(item_id, fence)
        .await
        .unwrap());
    assert!(!backend
        .complete_work_item_fenced(item_id, fence)
        .await
        .unwrap());
}

/// The unfenced settle takes only an item still claimed, as the fenced one
/// does: a pending item, one already settled and an unknown id are `false`,
/// and a pending item stays claimable. A completion sent twice without its
/// fence used to settle the row again on SQLite, and the route recorded the
/// step twice.
pub async fn unfenced_complete_settles_only_a_claimed_item(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item_id = backend
        .enqueue_work_item(pending_item(&exec_id))
        .await
        .unwrap();

    assert!(
        !backend.complete_work_item(item_id).await.unwrap(),
        "a pending item is not settled"
    );
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .expect("the pending item is still claimable");
    assert_eq!(claimed.id, item_id);
    assert!(backend.complete_work_item(item_id).await.unwrap());
    assert!(
        !backend.complete_work_item(item_id).await.unwrap(),
        "a settled item is not settled again"
    );
    assert!(!backend.complete_work_item(Uuid::new_v4()).await.unwrap());
    assert!(backend
        .claim_work_item("w2", &["default"])
        .await
        .unwrap()
        .is_none());
}

/// Build a never-claimed pending work item (worker_id = None, lease_fence = 0).
fn pending_item(exec_id: &ExecutionId) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
        node_id: "n1".into(),
        queue_type: "default".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    }
}

/// Reclaim fail-open: after `reclaim_expired_leases` hands a claimed item back
/// to the queue it clears `worker_id` but LEAVES `lease_fence` set. A zombie
/// worker presenting that now-stale fence must NOT settle the item.
pub async fn fenced_complete_rejects_stale_fence_after_reclaim(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item_id = backend
        .enqueue_work_item(pending_item(&exec_id))
        .await
        .unwrap();
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .unwrap();
    let fence = claimed.lease_fence;
    assert!(fence > 0, "a claimed item must carry a non-zero fence");

    // Force the lease to expire and run the REAL reclaim sweep (the same path
    // the scheduler drives). It clears worker_id but leaves lease_fence = F1.
    fx.hooks.force_lease_expired(item_id).await;
    let reclaimed = backend.reclaim_expired_leases().await.unwrap();
    assert_eq!(
        reclaimed.retryable.len(),
        1,
        "the expired item must be reclaimed as retryable"
    );

    // The zombie presents its stale fence F1. Pre-fix this settled (fence-only
    // match: 0 != F1 but worker_id was never checked); post-fix the
    // worker_id-is-None (reclaimed) item is rejected.
    assert!(
        !backend
            .complete_work_item_fenced(item_id, fence)
            .await
            .unwrap(),
        "a reclaimed item (worker_id cleared) must not settle via the stale fence"
    );

    // The item must survive the rejected settle and remain re-claimable.
    // Asserted on the row: the SQL stores hold a reclaimed item back for its
    // retry backoff, so a claim now would test the backoff. Memory has no
    // backoff filter, so there the claim takes it at once.
    let row = fx.hooks.item_row(item_id).await;
    assert_eq!(
        row.map(|r| r.status).as_deref(),
        Some("pending"),
        "the item must survive the stale-fence settle"
    );
    if fx.kind == StoreKind::Memory {
        assert!(
            backend
                .claim_work_item("w2", &["default"])
                .await
                .unwrap()
                .is_some(),
            "the item must still be claimable after the stale-fence settle was rejected"
        );
    }
}

/// Forged-fence-0 fail-open: a never-claimed pending item carries
/// `lease_fence == 0`. A forged `lease_fence: 0` (the value an attacker can put
/// on the HTTP complete request) must NOT settle it.
pub async fn fenced_complete_rejects_forged_fence_zero_on_pending_item(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item_id = backend
        .enqueue_work_item(pending_item(&exec_id))
        .await
        .unwrap();

    // Pre-fix: 0 == 0 matched the pending item's fence and removed it.
    assert!(
        !backend.complete_work_item_fenced(item_id, 0).await.unwrap(),
        "a forged fence 0 must not settle a never-claimed pending item"
    );

    // The pending item must survive and remain claimable.
    assert!(
        backend
            .claim_work_item("w1", &["default"])
            .await
            .unwrap()
            .is_some(),
        "the pending item must still be claimable after the forged-fence settle was rejected"
    );
}

/// A rollover finalizes only a still-claimed item of its own run. A forged
/// fence 0 matched a pending item's fence and settled it, and a run could
/// settle another run's item and end itself on it.
pub async fn finalize_rollover_settles_only_a_claimed_item_of_its_run(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let run = ExecutionId::new();
    let other = ExecutionId::new();
    for id in [&run, &other] {
        backend
            .create_execution(sample_execution(id))
            .await
            .unwrap();
    }
    let item_id = backend.enqueue_work_item(pending_item(&run)).await.unwrap();
    let status =
        |id: ExecutionId| async move { backend.get_execution(&id).await.unwrap().unwrap().status };

    assert!(
        !backend
            .finalize_rollover_fenced(&run, item_id, 0)
            .await
            .unwrap(),
        "a forged fence 0 must not finalize a pending item"
    );
    assert_eq!(status(run.clone()).await, WorkflowStatus::Running);
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .expect("the pending item is still claimable");
    assert_eq!(claimed.id, item_id);

    assert!(
        !backend
            .finalize_rollover_fenced(&other, item_id, claimed.lease_fence)
            .await
            .unwrap(),
        "another run's item is not finalized"
    );
    assert_eq!(status(other.clone()).await, WorkflowStatus::Running);

    assert!(backend
        .finalize_rollover_fenced(&run, item_id, claimed.lease_fence)
        .await
        .unwrap());
    assert_eq!(status(run.clone()).await, WorkflowStatus::LimitExceeded);
    assert!(
        !backend
            .finalize_rollover_fenced(&run, item_id, claimed.lease_fence)
            .await
            .unwrap(),
        "a settled item is not finalized again"
    );
}

/// Happy path: a currently-claimed item settled with its matching fence returns
/// true (the worker_id-is-some guard must not block the legitimate holder).
pub async fn fenced_complete_settles_claimed_item_with_matching_fence(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item_id = backend
        .enqueue_work_item(pending_item(&exec_id))
        .await
        .unwrap();
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        backend
            .complete_work_item_fenced(item_id, claimed.lease_fence)
            .await
            .unwrap(),
        "the current holder with the matching fence must settle the item"
    );
}

/// Atomicity: many concurrent fenced completes of the SAME claimed item with the
/// SAME matching fence must yield EXACTLY ONE `true`. `complete_work_item_fenced`
/// uses DashMap's `remove_if`, so the claimed+fence-match check and the delete are
/// one atomic step — the get-then-remove TOCTOU where two racers both observe the
/// item and both remove it (double-settle) cannot happen.
pub async fn fenced_complete_is_atomic_under_concurrency(fx: &Fixture) {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    let backend = fx.backend.clone();
    let exec_id = ExecutionId::new();
    // The SQL stores refuse rows of a run they do not have.
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let item_id = backend
        .enqueue_work_item(pending_item(&exec_id))
        .await
        .unwrap();
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .unwrap();
    let fence = claimed.lease_fence;
    assert!(fence > 0);

    // Release all racers simultaneously (the barrier maximizes the overlap on the
    // single contended item) and have each present the matching fence.
    const RACERS: usize = 16;
    let barrier = Arc::new(Barrier::new(RACERS));
    let mut handles = Vec::with_capacity(RACERS);
    for _ in 0..RACERS {
        let backend = Arc::clone(&backend);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            backend
                .complete_work_item_fenced(item_id, fence)
                .await
                .unwrap()
        }));
    }

    let mut settled = 0usize;
    for h in handles {
        if h.await.unwrap() {
            settled += 1;
        }
    }
    assert_eq!(
        settled, 1,
        "exactly one concurrent fenced complete may settle the item (atomic check-and-delete)"
    );
}

pub async fn test_patch_append_array(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&id))
        .await
        .unwrap();
    backend
        .patch_append_array(&id, "results", json!("first"))
        .await
        .unwrap();
    backend
        .patch_append_array(&id, "results", json!("second"))
        .await
        .unwrap();
    let exec = backend.get_execution(&id).await.unwrap().unwrap();
    let results = exec.current_state["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], json!("first"));
    assert_eq!(results[1], json!("second"));
}

pub async fn test_api_tokens(fx: &Fixture) {
    let backend = fx.backend.as_ref();
    let (plaintext, token) = backend.create_token("test-token", "admin").await.unwrap();
    assert!(plaintext.starts_with("jj_"));
    assert_eq!(token.name, "test-token");
    let validated = backend.validate_token(&plaintext).await.unwrap().unwrap();
    assert_eq!(validated.role, "admin");
    assert!(backend.validate_token("invalid").await.unwrap().is_none());
}
