//! The fixture's own contract: every handle and hook reaches the same
//! store, and the hooks read and write what the backend does.

use crate::conformance::{Fixture, StoreKind};
use chrono::{Duration, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{Tenant, TenantId, TenantStatus, WorkItem, WorkItemId, DEFAULT_TENANT};
use serde_json::json;

const QUEUE: &str = "general";

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-fixture".into(),
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

fn tenant(id: &str) -> Tenant {
    let now = Utc::now();
    Tenant {
        id: TenantId::from(id),
        name: id.to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    }
}

/// A running run with one pending item on [`QUEUE`].
async fn run_with_an_item(fx: &Fixture) -> (ExecutionId, WorkItemId) {
    let id = ExecutionId::new();
    fx.backend.create_execution(run(&id)).await.unwrap();
    let item = WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: "step".into(),
        queue_type: QUEUE.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    };
    let item_id = fx.backend.enqueue_work_item(item).await.unwrap();
    (id, item_id)
}

async fn claim(fx: &Fixture, item: WorkItemId) -> i64 {
    let claimed = fx
        .backend
        .claim_work_item("worker-a", &[QUEUE])
        .await
        .unwrap()
        .expect("the item is claimable");
    assert_eq!(claimed.id, item);
    claimed.lease_fence
}

pub async fn the_second_handle_sees_what_the_first_wrote(fx: &Fixture) {
    let second = fx
        .second
        .as_ref()
        .expect("every factory here has a second handle");
    let id = ExecutionId::new();
    fx.backend.create_execution(run(&id)).await.unwrap();
    assert!(
        second.get_execution(&id).await.unwrap().is_some(),
        "{:?}: the second handle does not see the first one's run",
        fx.kind
    );
}

pub async fn item_row_follows_a_claim_and_a_park(fx: &Fixture) {
    let (_, item) = run_with_an_item(fx).await;
    let row = fx.hooks.item_row(item).await.expect("a row for the item");
    assert_eq!(row.status, "pending");
    assert_eq!((row.attempt, row.lease_epoch, row.lease_fence), (0, 0, 0));
    assert_eq!(row.worker_id, None);

    let fence = claim(fx, item).await;
    let row = fx.hooks.item_row(item).await.unwrap();
    assert_eq!(row.status, "claimed");
    assert_eq!(row.worker_id.as_deref(), Some("worker-a"));
    assert_eq!((row.lease_epoch, row.lease_fence), (1, fence));

    let retry_after = Utc::now() + Duration::hours(1);
    let parked = fx
        .backend
        .park_work_item(item, fence, &retry_after.to_rfc3339(), 2)
        .await
        .unwrap();
    assert!(parked);
    let row = fx.hooks.item_row(item).await.unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!((row.attempt, row.lease_epoch), (2, 2));
    assert_eq!(row.worker_id, None);
    let kept = match fx.kind {
        StoreKind::Memory => None,
        _ => Some(retry_after),
    };
    assert_eq!(row.retry_after, kept);
}

pub async fn a_forced_expiry_is_reclaimed_and_a_future_one_is_not(fx: &Fixture) {
    let (_, item) = run_with_an_item(fx).await;
    claim(fx, item).await;

    fx.hooks
        .set_lease_expiry(item, Utc::now() + Duration::hours(1))
        .await;
    let swept = fx.backend.reclaim_expired_leases().await.unwrap();
    assert!(swept.retryable.is_empty() && swept.exhausted.is_empty());
    assert_eq!(fx.hooks.item_row(item).await.unwrap().status, "claimed");

    fx.hooks.force_lease_expired(item).await;
    let swept = fx.backend.reclaim_expired_leases().await.unwrap();
    let requeued: Vec<WorkItemId> = swept.retryable.iter().map(|i| i.id).collect();
    assert_eq!(requeued, vec![item]);
    assert_eq!(fx.hooks.item_row(item).await.unwrap().status, "pending");
}

pub async fn the_store_term_only_rises_and_reaches_the_fence(fx: &Fixture) {
    assert_eq!(fx.hooks.set_store_term_at_least(5).await, 5);
    assert_eq!(fx.hooks.set_store_term_at_least(3).await, 5);
    let (_, item) = run_with_an_item(fx).await;
    let fence = claim(fx, item).await;
    assert_eq!(fence >> 32, 5, "the fence carries the term above the epoch");
    assert_eq!(fence & 0xffff_ffff, 1);
}

pub async fn a_dead_lettered_item_names_its_run(fx: &Fixture) {
    let (run_id, item) = run_with_an_item(fx).await;
    assert_eq!(fx.hooks.dead_letter_execution(item).await, None);
    fx.backend
        .move_to_dead_letter(item, "gave up")
        .await
        .unwrap();
    assert_eq!(fx.hooks.dead_letter_execution(item).await, Some(run_id));
    assert_eq!(
        fx.hooks.item_row(item).await.map(|r| r.status),
        Some("dead_lettered".to_string())
    );
}

pub async fn artifact_rows_count_one_copy_per_tenant(fx: &Fixture) {
    let bytes = b"a fixture artifact";
    let stored = fx
        .backend
        .put_artifact(bytes, Some("text/plain"))
        .await
        .unwrap();
    fx.backend.put_artifact(bytes, None).await.unwrap();
    assert_eq!(fx.hooks.artifact_rows(&stored.hash, None).await, 1);
    assert_eq!(
        fx.hooks
            .artifact_rows(&stored.hash, Some(DEFAULT_TENANT))
            .await,
        1
    );
    assert_eq!(
        fx.hooks.artifact_rows(&stored.hash, Some("nobody")).await,
        0
    );
    let never_stored = jamjet_state::sha256_hex(b"never stored");
    assert_eq!(fx.hooks.artifact_rows(&never_stored, None).await, 0);
}

pub async fn a_token_moved_to_another_tenant_validates_there(fx: &Fixture) {
    let other = "fixture-other";
    match (&fx.scoped, fx.kind) {
        (Some(scoper), _) => scoper(&TenantId::from(other))
            .create_tenant(tenant(other))
            .await
            .unwrap(),
        // Memory has no tenant rows to make.
        (None, StoreKind::Memory) => {}
        // An unscoped SQL store makes tenants only through a scoped handle.
        (None, _) => return,
    }
    let (plaintext, token) = fx
        .backend
        .create_token("fixture-token", "operator")
        .await
        .unwrap();
    assert_eq!(token.tenant_id, DEFAULT_TENANT);
    fx.hooks.set_token_tenant("fixture-token", other).await;
    let seen = fx
        .backend
        .validate_token(&plaintext)
        .await
        .unwrap()
        .expect("the token is still valid");
    assert_eq!(seen.tenant_id, other);
}

pub async fn a_reopened_store_keeps_what_was_written(fx: &Fixture) {
    let id = ExecutionId::new();
    fx.backend.create_execution(run(&id)).await.unwrap();
    match fx.hooks.reopen().await {
        None => assert_eq!(fx.kind, StoreKind::Memory, "only memory cannot reopen"),
        Some(again) => assert!(
            again.get_execution(&id).await.unwrap().is_some(),
            "{:?}: the reopened store lost the run",
            fx.kind
        ),
    }
}

pub async fn a_scoped_handle_sees_only_its_tenant(fx: &Fixture) {
    let Some(scoper) = &fx.scoped else {
        assert!(!fx.kind.is_scoped(), "{:?} has no scoper", fx.kind);
        return;
    };
    for t in ["fixture-a", "fixture-b"] {
        scoper(&TenantId::from(t))
            .create_tenant(tenant(t))
            .await
            .unwrap();
    }
    let a = scoper(&TenantId::from("fixture-a"));
    let b = scoper(&TenantId::from("fixture-b"));
    let id = ExecutionId::new();
    a.create_execution(run(&id)).await.unwrap();
    assert!(a.get_execution(&id).await.unwrap().is_some());
    assert!(b.get_execution(&id).await.unwrap().is_none());
    // `backend`, `second` and a reopened handle are the default tenant's views.
    assert!(fx.backend.get_execution(&id).await.unwrap().is_none());
    let second = fx.second.as_ref().unwrap();
    assert!(second.get_execution(&id).await.unwrap().is_none());
    if let Some(again) = fx.hooks.reopen().await {
        assert!(again.get_execution(&id).await.unwrap().is_none());
    }
}
