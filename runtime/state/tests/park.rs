//! TDD tests for `park_work_item` — fence-guarded rate-limit parking.
//!
//! A parked work item is reset to `pending` with a not-before `retry_after`
//! timestamp (now + provider backoff). The fence guard ensures a stale worker
//! cannot overwrite a newer attempt's state. Bumping `lease_epoch` means the
//! next claim mints a strictly-greater fence.
//!
//! Written RED before `park_work_item` existed; turned GREEN after implementing
//! the method in all three backends (sqlite.rs, tenant_scoped.rs, memory.rs).

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, WorkItem},
    InMemoryBackend, SqliteBackend, TenantId,
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

// ── Shared fixtures ──────────────────────────────────────────────────────────

async fn open_test_db() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite for park tests")
}

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "park-wf".into(),
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

fn sample_item(execution_id: &ExecutionId) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: "llm-node".into(),
        queue_type: "model".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 5,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "default".into(),
        lease_fence: 0,
    }
}

/// Returns a retry_after timestamp 60 seconds in the future (RFC3339).
fn future_retry_after() -> String {
    (Utc::now() + chrono::Duration::seconds(60)).to_rfc3339()
}

/// Returns a retry_after timestamp 5 seconds in the past (RFC3339).
/// Used to verify that a parked item becomes immediately claimable once its
/// not-before window has elapsed.  No real sleep needed: the SQL filter is
/// `retry_after <= now`, so a past timestamp is always visible on the next claim.
fn past_retry_after() -> String {
    (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339()
}

// ── SQLite backend tests ─────────────────────────────────────────────────────

/// Correct fence: park returns Ok(true) and the row is reset to pending
/// with the expected attempt, NULL worker_id, future retry_after, and a
/// bumped lease_epoch.
#[tokio::test]
async fn sqlite_park_correct_fence_returns_true_and_resets_row() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    // Claim the item to get a valid lease_fence.
    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");

    let original_fence = claimed.lease_fence;
    assert!(original_fence > 0, "claim must mint a non-zero fence");

    // Read the lease_epoch before parking to verify it is bumped.
    let epoch_before: i64 = sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap()
        .try_get("lease_epoch")
        .unwrap();

    let retry_after = future_retry_after();
    let parked = db
        .park_work_item(item_id, original_fence, &retry_after, 1)
        .await
        .expect("park_work_item must not error");
    assert!(parked, "correct fence must return true");

    // Read the row back directly to verify all SET columns.
    let row = sqlx::query(
        "SELECT status, attempt, worker_id, retry_after, lease_epoch FROM work_items WHERE id = ?",
    )
    .bind(item_id.to_string())
    .fetch_one(&db.pool())
    .await
    .unwrap();

    let status: &str = row.try_get("status").unwrap();
    assert_eq!(status, "pending", "parked item must be 'pending'");

    let attempt: i64 = row.try_get("attempt").unwrap();
    assert_eq!(attempt, 1, "attempt must be set to next_attempt (1)");

    let worker_id: Option<String> = row.try_get("worker_id").unwrap();
    assert!(worker_id.is_none(), "worker_id must be NULL after park");

    let stored_retry_after: &str = row.try_get("retry_after").unwrap();
    assert_eq!(
        stored_retry_after, retry_after,
        "retry_after must match what was passed"
    );

    let epoch_after: i64 = row.try_get("lease_epoch").unwrap();
    assert_eq!(
        epoch_after,
        epoch_before + 1,
        "lease_epoch must be bumped by 1"
    );

    // A parked item with future retry_after is NOT claimable yet.
    let not_claimable = db.claim_work_item("worker-2", &["model"]).await.unwrap();
    assert!(
        not_claimable.is_none(),
        "parked item with future retry_after must not be claimable"
    );
}

/// Stale fence: park returns Ok(false) and the row is unchanged.
#[tokio::test]
async fn sqlite_park_stale_fence_returns_false_row_unchanged() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let real_fence = claimed.lease_fence;
    let _stale_fence = real_fence + 1; // fabricated — documented for clarity, not used directly

    // First: park with the correct fence so the item is back in 'pending'.
    let retry_after = future_retry_after();
    let first = db
        .park_work_item(item_id, real_fence, &retry_after, 1)
        .await
        .unwrap();
    assert!(first, "first park with correct fence must return true");

    // Now the item is pending with lease_fence effectively gone (bumped).
    // A second park with the original (now stale) fence must be a no-op.
    let later_retry = future_retry_after();
    let second = db
        .park_work_item(item_id, real_fence, &later_retry, 2)
        .await
        .unwrap();
    assert!(
        !second,
        "stale fence park must return false (fence was bumped by first park)"
    );

    // Row must be unchanged from the FIRST park (attempt=1, retry_after from first).
    let row =
        sqlx::query("SELECT status, attempt, worker_id, retry_after FROM work_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap();

    let status: &str = row.try_get("status").unwrap();
    assert_eq!(status, "pending", "status must still be 'pending'");

    let attempt: i64 = row.try_get("attempt").unwrap();
    assert_eq!(
        attempt, 1,
        "attempt must still be 1 (not 2 from stale park)"
    );

    let stored_retry_after: &str = row.try_get("retry_after").unwrap();
    assert_eq!(
        stored_retry_after, retry_after,
        "retry_after must be the one set by the first (valid) park"
    );
}

/// A completely wrong (fabricated) fence on a claimed item returns false
/// and leaves the item in 'claimed' state.
#[tokio::test]
async fn sqlite_park_wrong_fence_on_claimed_item_is_noop() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .unwrap();
    let wrong_fence = claimed.lease_fence + 999;

    let result = db
        .park_work_item(item_id, wrong_fence, &future_retry_after(), 1)
        .await
        .unwrap();
    assert!(!result, "wrong fence must return false");

    // Item is still 'claimed', not 'pending'.
    let status: String = sqlx::query("SELECT status FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap()
        .try_get("status")
        .unwrap();
    assert_eq!(
        status, "claimed",
        "item must remain 'claimed' after stale-fence park"
    );
}

// ── In-memory backend tests ──────────────────────────────────────────────────

/// Correct fence on in-memory backend returns Ok(true); attempt and fence are updated.
/// Note: InMemoryBackend does not enforce the retry_after not-before window in
/// claim_work_item (no SQL filter). The fence guard and state mutation are tested here.
#[tokio::test]
async fn memory_park_correct_fence_returns_true_and_resets_item() {
    let db = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let fence = claimed.lease_fence;
    assert!(fence > 0);

    let parked = db
        .park_work_item(item_id, fence, &future_retry_after(), 1)
        .await
        .unwrap();
    assert!(parked, "correct fence must return true");

    // After park: item is back in pending (worker_id=None, fence reset).
    // In-memory claim picks it up immediately (no retry_after filter).
    let reclaimed = db
        .claim_work_item("worker-2", &["model"])
        .await
        .unwrap()
        .expect("in-memory item must be re-claimable after park");

    assert_eq!(
        reclaimed.attempt, 1,
        "attempt must be set to next_attempt after park"
    );
    assert!(
        reclaimed.lease_fence > 0,
        "re-claim must mint a fresh non-zero fence"
    );
    // The new fence must be different from (and ideally greater than) the old one.
    assert_ne!(
        reclaimed.lease_fence, fence,
        "re-claim fence must differ from the parked fence"
    );
}

/// Stale fence on in-memory backend returns Ok(false) and item is unchanged.
#[tokio::test]
async fn memory_park_stale_fence_returns_false_item_unchanged() {
    let db = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let real_fence = claimed.lease_fence;

    // Park with correct fence first.
    let first = db
        .park_work_item(item_id, real_fence, &future_retry_after(), 1)
        .await
        .unwrap();
    assert!(first, "first park must succeed");

    // Park with the now-stale fence → no-op.
    let second = db
        .park_work_item(item_id, real_fence, &future_retry_after(), 2)
        .await
        .unwrap();
    assert!(!second, "stale fence must return false");

    // Item's attempt must still be 1 (from the first valid park), not 2.
    let reclaimed = db
        .claim_work_item("worker-2", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    assert_eq!(
        reclaimed.attempt, 1,
        "attempt must be 1 from first park; stale park must not update it"
    );
}

// ── Tenant-scoped backend tests ──────────────────────────────────────────────

/// TenantScopedSqliteBackend: park is scoped to tenant — a stale fence from a
/// different worker is rejected; the lease_epoch is bumped for re-claim.
#[tokio::test]
async fn tenant_scoped_park_fence_guard_and_epoch_bump() {
    let db = open_test_db().await;
    let tenant = db.for_tenant(TenantId::default_tenant());

    let exec_id = ExecutionId::new();
    tenant
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    tenant.enqueue_work_item(item).await.unwrap();

    let claimed = tenant
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("tenant item must be claimable");
    let fence = claimed.lease_fence;

    // Read epoch before parking.
    let epoch_before: i64 =
        sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ? AND tenant_id = 'default'")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap()
            .try_get("lease_epoch")
            .unwrap();

    // Correct fence → parked.
    let retry_after = future_retry_after();
    let ok = tenant
        .park_work_item(item_id, fence, &retry_after, 1)
        .await
        .unwrap();
    assert!(ok, "tenant-scoped park with correct fence must return true");

    // Verify epoch bumped.
    let epoch_after: i64 =
        sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ? AND tenant_id = 'default'")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap()
            .try_get("lease_epoch")
            .unwrap();
    assert_eq!(
        epoch_after,
        epoch_before + 1,
        "tenant-scoped park must bump lease_epoch"
    );

    // Stale fence → no-op.
    let noop = tenant
        .park_work_item(item_id, fence, &future_retry_after(), 2)
        .await
        .unwrap();
    assert!(!noop, "stale fence must be no-op for tenant-scoped backend");

    // Attempt must still be 1.
    let attempt: i64 = sqlx::query("SELECT attempt FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap()
        .try_get("attempt")
        .unwrap();
    assert_eq!(
        attempt, 1,
        "attempt must be unchanged after stale-fence park"
    );
}

// ── Task 2f-4: park/reclaim visibility (not-before filter) ───────────────────

/// SQLite: park with a PAST retry_after makes the item immediately claimable.
///
/// This is the complement of the "future = not claimable" assertion that already
/// lives in `sqlite_park_correct_fence_returns_true_and_resets_row` (2f-1).
/// Together, the two tests prove the retry_after filter in both directions without
/// any real sleep.  The SQLite claim SQL is:
///   AND (retry_after IS NULL OR retry_after <= ?)
/// so a past timestamp passes immediately on the next claim call.
///
/// The reclaimed item must carry the bumped attempt and a fresh non-zero fence
/// that differs from the fence the parking worker held.
#[tokio::test]
async fn sqlite_park_past_retry_after_is_immediately_claimable() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let fence = claimed.lease_fence;

    // Park with a timestamp in the PAST — item should be visible on the next claim.
    let past_ts = past_retry_after();
    let parked = db
        .park_work_item(item_id, fence, &past_ts, 1)
        .await
        .unwrap();
    assert!(parked, "park with correct fence must return true");

    // Immediately claimable because retry_after <= now.
    let reclaimed = db
        .claim_work_item("worker-2", &["model"])
        .await
        .unwrap()
        .expect("item parked with past retry_after must be immediately claimable");

    assert_eq!(
        reclaimed.attempt, 1,
        "re-claimed item must have attempt=1 (set by park)"
    );
    assert!(
        reclaimed.lease_fence > 0,
        "re-claim must mint a fresh non-zero fence"
    );
    assert_ne!(
        reclaimed.lease_fence, fence,
        "re-claim fence must differ from the park-time fence"
    );
}

/// Tenant-scoped: park with a PAST retry_after is immediately claimable.
///
/// Verifies the same not-before filter (`retry_after <= ?`) on the
/// tenant-scoped claim SQL which adds `AND tenant_id = ?`.
#[tokio::test]
async fn tenant_scoped_park_past_retry_after_is_immediately_claimable() {
    let db = open_test_db().await;
    let tenant = db.for_tenant(TenantId::default_tenant());

    let exec_id = ExecutionId::new();
    tenant
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    tenant.enqueue_work_item(item).await.unwrap();

    let claimed = tenant
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let fence = claimed.lease_fence;

    // Park with a past timestamp.
    let parked = tenant
        .park_work_item(item_id, fence, &past_retry_after(), 1)
        .await
        .unwrap();
    assert!(
        parked,
        "tenant-scoped park with correct fence must return true"
    );

    // Immediately claimable (past retry_after passes the <= now filter).
    let reclaimed = tenant
        .claim_work_item("worker-2", &["model"])
        .await
        .unwrap()
        .expect("tenant item parked with past retry_after must be immediately claimable");

    assert_eq!(reclaimed.attempt, 1, "reclaimed attempt must be 1");
    assert_ne!(
        reclaimed.lease_fence, fence,
        "reclaimed fence must differ from park-time fence"
    );
}

// ── Task 2f-4: fence-safety end-to-end ───────────────────────────────────────

/// SQLite: full end-to-end fence-safety sequence —
///   claim(F1) → park(F1, past_ts, 1) → reclaim(F2) → stale-park(F1, .., 2) = false.
///
/// The 2f-1 unit test (`sqlite_park_stale_fence_returns_false_row_unchanged`)
/// exercises claim→park→stale-park. This test adds the RECLAIM step in between,
/// proving that F1 is truly stale after the item has been reclaimed by a new
/// worker holding F2, and that the newer attempt's state (attempt=1) survives the
/// rejected stale park.
#[tokio::test]
async fn sqlite_fence_safety_claim_park_reclaim_stale_park() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let item = sample_item(&exec_id);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    // Step 1: claim → get fence F1.
    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let f1 = claimed.lease_fence;
    assert!(f1 > 0);

    // Step 2: park with F1 (past ts so re-claimable immediately).
    let parked = db
        .park_work_item(item_id, f1, &past_retry_after(), 1)
        .await
        .unwrap();
    assert!(parked, "park with F1 must succeed");

    // Step 3: a second worker reclaims → gets fence F2 > F1.
    let reclaimed = db
        .claim_work_item("worker-2", &["model"])
        .await
        .unwrap()
        .expect("re-claim after past-ts park must succeed");
    let f2 = reclaimed.lease_fence;
    assert_ne!(f2, f1, "F2 must differ from F1");
    assert_eq!(reclaimed.attempt, 1, "reclaimed item must be at attempt=1");

    // Step 4: stale worker (holding F1) tries to park again → must be no-op.
    let stale = db
        .park_work_item(item_id, f1, &future_retry_after(), 2)
        .await
        .unwrap();
    assert!(!stale, "stale-fence park after reclaim must return false");

    // The row must still be at attempt=1 (not 2 from the stale park).
    // Status is 'claimed' because worker-2 holds it.
    let row = sqlx::query("SELECT status, attempt FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap();
    let status: &str = row.try_get("status").unwrap();
    assert_eq!(
        status, "claimed",
        "item must still be 'claimed' by worker-2"
    );
    let attempt: i64 = row.try_get("attempt").unwrap();
    assert_eq!(
        attempt, 1,
        "attempt must be 1; stale park must not advance it"
    );
}

// ── Task 2f-4: attempt monotonicity + epoch freshness (cross-backend) ────────

/// SQLite: each successful park increments attempt by exactly 1 and bumps
/// lease_epoch, so the next claim mints a strictly different fence.
///
/// Sequence: claim → park(1) → reclaim(F2) → park(2) → reclaim(F3).
/// After each park, attempt increases by 1 and lease_epoch increases by 1.
/// Past timestamps are used so both reclaims succeed without sleeping.
///
/// Note: `claim_work_item` also bumps `lease_epoch` (so the fence is strictly
/// increasing). The epoch is read AFTER each claim so the assertion only checks
/// the delta contributed by `park_work_item` itself (+1 per park).
#[tokio::test]
async fn sqlite_attempt_monotonicity_and_epoch_freshness() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let mut item = sample_item(&exec_id);
    item.max_attempts = 10; // headroom for two parks
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    // ── Park 1: attempt 0 → 1 ────────────────────────────────────────────────
    let c1 = db
        .claim_work_item("w1", &["model"])
        .await
        .unwrap()
        .expect("first claim must succeed");
    let f1 = c1.lease_fence;
    assert!(f1 > 0, "fence must be non-zero after claim");

    // Read epoch AFTER claim so we measure the park delta (+1) cleanly.
    let epoch_after_claim1: i64 = sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap()
        .try_get("lease_epoch")
        .unwrap();

    db.park_work_item(item_id, f1, &past_retry_after(), 1)
        .await
        .unwrap();

    let epoch_after_park1: i64 = sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap()
        .try_get("lease_epoch")
        .unwrap();
    assert_eq!(
        epoch_after_park1,
        epoch_after_claim1 + 1,
        "first park must bump lease_epoch by exactly 1"
    );

    // ── Park 2: attempt 1 → 2 ────────────────────────────────────────────────
    let c2 = db
        .claim_work_item("w2", &["model"])
        .await
        .unwrap()
        .expect("second claim must succeed after past-ts park");
    let f2 = c2.lease_fence;
    assert_ne!(f2, f1, "second claim must mint a different fence");
    assert_eq!(
        c2.attempt, 1,
        "item must be at attempt=1 before second park"
    );

    // Read epoch after second claim.
    let epoch_after_claim2: i64 = sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap()
        .try_get("lease_epoch")
        .unwrap();

    db.park_work_item(item_id, f2, &past_retry_after(), 2)
        .await
        .unwrap();

    let row = sqlx::query("SELECT lease_epoch, attempt FROM work_items WHERE id = ?")
        .bind(item_id.to_string())
        .fetch_one(&db.pool())
        .await
        .unwrap();
    let epoch_after_park2: i64 = row.try_get("lease_epoch").unwrap();
    let attempt2: i64 = row.try_get("attempt").unwrap();
    assert_eq!(
        epoch_after_park2,
        epoch_after_claim2 + 1,
        "second park must bump lease_epoch by exactly 1"
    );
    assert_eq!(attempt2, 2, "attempt must be 2 after second park");

    // ── Third claim: fence differs from both F1 and F2 ───────────────────────
    let c3 = db
        .claim_work_item("w3", &["model"])
        .await
        .unwrap()
        .expect("third claim must succeed");
    let f3 = c3.lease_fence;
    assert_ne!(f3, f1, "third fence must differ from F1");
    assert_ne!(f3, f2, "third fence must differ from F2");
    assert_eq!(c3.attempt, 2, "item must be at attempt=2 on third claim");
}

/// In-memory: each successful park increments attempt by exactly 1 and the next
/// claim mints a fresh fence.
///
/// The in-memory backend does not enforce the retry_after not-before window, so
/// future timestamps are used (no real sleep needed — claim ignores retry_after).
/// Epoch is not readable directly; monotonicity is asserted via the fence values
/// from successive claims.
#[tokio::test]
async fn memory_attempt_monotonicity_and_epoch_freshness() {
    let db = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let mut item = sample_item(&exec_id);
    item.max_attempts = 10;
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    // ── Park 1: attempt 0 → 1 ────────────────────────────────────────────────
    let c1 = db
        .claim_work_item("w1", &["model"])
        .await
        .unwrap()
        .expect("first claim must succeed");
    let f1 = c1.lease_fence;

    db.park_work_item(item_id, f1, &future_retry_after(), 1)
        .await
        .unwrap();

    let c2 = db
        .claim_work_item("w2", &["model"])
        .await
        .unwrap()
        .expect("in-memory re-claim must succeed immediately after park");
    let f2 = c2.lease_fence;
    assert_eq!(c2.attempt, 1, "attempt must be 1 after first park");
    assert_ne!(f2, f1, "second fence must differ from first");
    assert_ne!(f2, 0, "fence must be non-zero after re-claim");

    // ── Park 2: attempt 1 → 2 ────────────────────────────────────────────────
    db.park_work_item(item_id, f2, &future_retry_after(), 2)
        .await
        .unwrap();

    let c3 = db
        .claim_work_item("w3", &["model"])
        .await
        .unwrap()
        .expect("third claim must succeed");
    let f3 = c3.lease_fence;
    assert_eq!(c3.attempt, 2, "attempt must be 2 after second park");
    assert_ne!(f3, f2, "third fence must differ from second");
    assert_ne!(f3, f1, "third fence must differ from first");
}

/// Tenant-scoped: each successful park increments attempt by exactly 1 and
/// bumps lease_epoch.
///
/// Uses past timestamps so re-claims succeed without sleeping. Epoch is
/// verified via direct SQL (pool from the parent SqliteBackend).
/// Note: `claim_work_item` also bumps `lease_epoch`, so epochs are read
/// AFTER each claim to isolate the park-only delta (+1 per park).
#[tokio::test]
async fn tenant_scoped_attempt_monotonicity_and_epoch_freshness() {
    let db = open_test_db().await;
    let tenant = db.for_tenant(TenantId::default_tenant());

    let exec_id = ExecutionId::new();
    tenant
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let mut item = sample_item(&exec_id);
    item.max_attempts = 10;
    let item_id = item.id;
    tenant.enqueue_work_item(item).await.unwrap();

    // ── Park 1: attempt 0 → 1 ────────────────────────────────────────────────
    let c1 = tenant
        .claim_work_item("w1", &["model"])
        .await
        .unwrap()
        .expect("first claim");
    let f1 = c1.lease_fence;

    // Read epoch AFTER claim to measure only the park delta (+1).
    let epoch_after_claim1: i64 =
        sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ? AND tenant_id = 'default'")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap()
            .try_get("lease_epoch")
            .unwrap();

    tenant
        .park_work_item(item_id, f1, &past_retry_after(), 1)
        .await
        .unwrap();

    let epoch_after_park1: i64 =
        sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ? AND tenant_id = 'default'")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap()
            .try_get("lease_epoch")
            .unwrap();
    assert_eq!(
        epoch_after_park1,
        epoch_after_claim1 + 1,
        "first park must bump epoch by exactly 1"
    );

    // ── Park 2: attempt 1 → 2 ────────────────────────────────────────────────
    let c2 = tenant
        .claim_work_item("w2", &["model"])
        .await
        .unwrap()
        .expect("second claim");
    let f2 = c2.lease_fence;
    assert_ne!(f2, f1, "second fence must differ");
    assert_eq!(c2.attempt, 1, "attempt must be 1 before second park");

    // Read epoch after second claim.
    let epoch_after_claim2: i64 =
        sqlx::query("SELECT lease_epoch FROM work_items WHERE id = ? AND tenant_id = 'default'")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap()
            .try_get("lease_epoch")
            .unwrap();

    tenant
        .park_work_item(item_id, f2, &past_retry_after(), 2)
        .await
        .unwrap();

    let row = sqlx::query(
        "SELECT lease_epoch, attempt FROM work_items WHERE id = ? AND tenant_id = 'default'",
    )
    .bind(item_id.to_string())
    .fetch_one(&db.pool())
    .await
    .unwrap();
    let epoch_after_park2: i64 = row.try_get("lease_epoch").unwrap();
    let attempt2: i64 = row.try_get("attempt").unwrap();
    assert_eq!(
        epoch_after_park2,
        epoch_after_claim2 + 1,
        "second park must bump epoch by exactly 1"
    );
    assert_eq!(attempt2, 2, "attempt must be 2 after second park");
}
