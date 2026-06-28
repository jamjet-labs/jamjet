//! Integration tests for InMemoryBackend.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, WorkflowDefinition},
    event::{Event, EventKind},
    snapshot::Snapshot,
    InMemoryBackend, WorkItem,
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

#[tokio::test]
async fn test_workflow_crud() {
    let backend = InMemoryBackend::new();
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

#[tokio::test]
async fn test_execution_lifecycle() {
    let backend = InMemoryBackend::new();
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

#[tokio::test]
async fn test_list_executions_with_filter() {
    let backend = InMemoryBackend::new();
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

#[tokio::test]
async fn test_event_log() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();

    let e1 = Event::new(
        exec_id.clone(),
        0,
        EventKind::WorkflowStarted {
            workflow_id: "wf-test".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({ "x": 1 }),
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

#[tokio::test]
async fn test_snapshots() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    assert!(backend.latest_snapshot(&exec_id).await.unwrap().is_none());
    let snap = Snapshot::new(exec_id.clone(), 5, json!({"state": "checkpoint"}));
    backend.write_snapshot(snap).await.unwrap();
    let latest = backend.latest_snapshot(&exec_id).await.unwrap().unwrap();
    assert_eq!(latest.at_sequence, 5);
}

#[tokio::test]
async fn test_work_queue() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
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
    backend.complete_work_item(item_id).await.unwrap();
}

#[tokio::test]
async fn test_complete_work_item_fenced() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
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
#[tokio::test]
async fn fenced_complete_rejects_stale_fence_after_reclaim() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
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
    backend.force_lease_expired_for_test(item_id);
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
    assert!(
        backend
            .claim_work_item("w2", &["default"])
            .await
            .unwrap()
            .is_some(),
        "the item must still be claimable after the stale-fence settle was rejected"
    );
}

/// Forged-fence-0 fail-open: a never-claimed pending item carries
/// `lease_fence == 0`. A forged `lease_fence: 0` (the value an attacker can put
/// on the HTTP complete request) must NOT settle it.
#[tokio::test]
async fn fenced_complete_rejects_forged_fence_zero_on_pending_item() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
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

/// Happy path: a currently-claimed item settled with its matching fence returns
/// true (the worker_id-is-some guard must not block the legitimate holder).
#[tokio::test]
async fn fenced_complete_settles_claimed_item_with_matching_fence() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
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
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fenced_complete_is_atomic_under_concurrency() {
    use std::sync::Arc;
    use tokio::sync::Barrier;

    let backend = Arc::new(InMemoryBackend::new());
    let exec_id = ExecutionId::new();
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

#[tokio::test]
async fn test_patch_append_array() {
    let backend = InMemoryBackend::new();
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

#[tokio::test]
async fn test_api_tokens() {
    let backend = InMemoryBackend::new();
    let (plaintext, token) = backend.create_token("test-token", "admin").await.unwrap();
    assert!(plaintext.starts_with("jj_"));
    assert_eq!(token.name, "test-token");
    let validated = backend.validate_token(&plaintext).await.unwrap().unwrap();
    assert_eq!(validated.role, "admin");
    assert!(backend.validate_token("invalid").await.unwrap().is_none());
}
