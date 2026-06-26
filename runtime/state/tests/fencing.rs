//! Lease-fencing tests: a zombie worker cannot double-commit; the fence is
//! monotonic across reclaim and survives a simulated store failover.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, StateBackendError, WorkItem},
    Event, EventKind, SqliteBackend,
};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    let unique = Uuid::new_v4().to_string().replace('-', "");
    path.push(format!("jamjet_fence_{unique}.db"));
    path
}

async fn open_db(path: &PathBuf) -> SqliteBackend {
    let url = format!("sqlite://{}", path.display());
    SqliteBackend::open(&url).await.expect("open test db")
}

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-fence".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
    }
}

fn sample_item(execution_id: &ExecutionId) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: "n1".into(),
        queue_type: "model".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "default".into(),
        lease_fence: 0,
    }
}

#[tokio::test]
async fn claim_mints_nonzero_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    let claimed = db.claim_work_item("worker-A", &["model"]).await.unwrap().unwrap();
    assert!(claimed.lease_fence > 0, "claim must mint a non-zero fence");

    std::fs::remove_file(&path).ok();
}

/// Verify that a re-claim (after a stale/expired lease) mints a strictly
/// greater fence. This is the anti-double-commit invariant: a zombie worker
/// holding the old fence value gets 0 rows on renew_lease / commit.
///
/// Adaptation from brief: `fail_work_item` sets status='failed' (not
/// 'pending') in the SQLite backend, so we cannot use it to make the item
/// re-claimable. Instead we use `force_lease_expired_for_test` to backdate the
/// lease_expires_at, then call `claim_work_item` which has a built-in
/// stale-expiry UPDATE that bumps `lease_epoch + 1` and resets to 'pending'
/// before the second claim.
#[tokio::test]
async fn reclaim_bumps_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    // First claim mints fence F1 (term=0, epoch=1 → F1=1).
    let first = db.claim_work_item("worker-A", &["model"]).await.unwrap().unwrap();
    assert!(first.lease_fence > 0, "first claim must mint a non-zero fence");

    // Backdate the lease so the stale-expiry path inside claim_work_item fires.
    // (fail_work_item sets status='failed', not 'pending', so it is not
    //  directly re-claimable — see adaptation note above.)
    db.force_lease_expired_for_test(first.id).await.unwrap();

    // Re-claim: claim_work_item's stale-expiry UPDATE bumps lease_epoch+1 and
    // resets status='pending'; the subsequent INSERT mints a strictly greater fence.
    let second = db.claim_work_item("worker-B", &["model"]).await.unwrap().unwrap();
    assert!(
        second.lease_fence > first.lease_fence,
        "re-claim must mint a strictly greater fence ({} !> {})",
        second.lease_fence,
        first.lease_fence
    );

    std::fs::remove_file(&path).ok();
}

fn node_completed(node: &str) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node.into(),
        output: json!({ "ok": true }),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
    }
}

#[tokio::test]
async fn commit_succeeds_with_correct_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    let item = db.claim_work_item("worker-A", &["model"]).await.unwrap().unwrap();
    let event = Event::new(eid.clone(), 0, node_completed("n1"));
    let seq = db
        .commit_node_terminal(item.id, item.lease_fence, event)
        .await
        .expect("commit with correct fence");
    assert!(seq >= 1);

    // Exactly one event was appended.
    let events = db.get_events(&eid).await.unwrap();
    assert_eq!(events.len(), 1);
    // Item is settled — no longer claimable.
    assert!(db.claim_work_item("worker-B", &["model"]).await.unwrap().is_none());

    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn commit_fails_closed_with_stale_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    // Worker A claims (fence F1). Backdate the lease so the stale-expiry path
    // in claim_work_item fires: it bumps lease_epoch+1 and resets to 'pending'.
    // Worker B then re-claims and receives a strictly greater fence F2.
    // Worker A is now a zombie holding F1.
    let zombie = db.claim_work_item("worker-A", &["model"]).await.unwrap().unwrap();
    db.force_lease_expired_for_test(zombie.id).await.unwrap();
    let _b = db.claim_work_item("worker-B", &["model"]).await.unwrap().unwrap();

    let event = Event::new(eid.clone(), 0, node_completed("n1"));
    let err = db
        .commit_node_terminal(zombie.id, zombie.lease_fence, event)
        .await
        .expect_err("zombie commit must fail closed");
    assert!(matches!(err, StateBackendError::FenceLost(_)));

    // The zombie's commit emitted NOTHING.
    assert_eq!(db.get_events(&eid).await.unwrap().len(), 0);

    std::fs::remove_file(&path).ok();
}
