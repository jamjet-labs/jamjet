//! `park_work_item`: fence-guarded rate-limit parking.
//!
//! A parked work item is reset to `pending` with a not-before `retry_after`
//! timestamp (now + provider backoff). The fence guard ensures a stale worker
//! cannot overwrite a newer attempt's state. Bumping `lease_epoch` means the
//! next claim mints a strictly-greater fence.
//!
//! Memory does not keep `retry_after` and does not hold a parked item back
//! from a claim; the cases say where that changes what they check.

use crate::conformance::{Fixture, ItemRow, StoreKind};
use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem};
use serde_json::json;
use uuid::Uuid;

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
/// not-before window has elapsed. No real sleep needed: the SQL filter is
/// `retry_after <= now`, so a past timestamp is always visible on the next claim.
fn past_retry_after() -> String {
    (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339()
}

fn parsed(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&Utc)
}

/// The item's row, which must exist.
async fn row(fx: &Fixture, item_id: Uuid) -> ItemRow {
    fx.hooks
        .item_row(item_id)
        .await
        .expect("the item has a row")
}

/// A run with one pending item on `model`, the item's id.
async fn seed(db: &dyn StateBackend, max_attempts: u32) -> Uuid {
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    let mut item = sample_item(&exec_id);
    item.max_attempts = max_attempts;
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();
    item_id
}

/// Correct fence: park returns Ok(true) and the row is reset to pending
/// with the expected attempt, no worker, the retry_after given, and a
/// bumped lease_epoch. Until then the item is not claimable.
pub async fn park_correct_fence_returns_true_and_resets_row(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let item_id = seed(db, 5).await;

    // Claim the item to get a valid lease_fence.
    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let original_fence = claimed.lease_fence;
    assert!(original_fence > 0, "claim must mint a non-zero fence");

    // Read the lease_epoch before parking to verify it is bumped.
    let epoch_before = row(fx, item_id).await.lease_epoch;

    let retry_after = future_retry_after();
    let parked = db
        .park_work_item(item_id, original_fence, &retry_after, 1)
        .await
        .expect("park_work_item must not error");
    assert!(parked, "correct fence must return true");

    let after = row(fx, item_id).await;
    assert_eq!(after.status, "pending", "parked item must be 'pending'");
    assert_eq!(after.attempt, 1, "attempt must be set to next_attempt (1)");
    assert!(
        after.worker_id.is_none(),
        "worker_id must be NULL after park"
    );
    if fx.kind != StoreKind::Memory {
        assert_eq!(
            after.retry_after,
            Some(parsed(&retry_after)),
            "retry_after must match what was passed"
        );
    }
    assert_eq!(
        after.lease_epoch,
        epoch_before + 1,
        "lease_epoch must be bumped by 1"
    );

    let next = db.claim_work_item("worker-2", &["model"]).await.unwrap();
    if fx.kind == StoreKind::Memory {
        // Memory has no retry_after filter: the item is claimable at once,
        // at the parked attempt and under a fresh fence.
        let reclaimed = next.expect("in-memory item must be re-claimable after park");
        assert_eq!(
            reclaimed.attempt, 1,
            "attempt must be set to next_attempt after park"
        );
        assert!(
            reclaimed.lease_fence > 0,
            "re-claim must mint a fresh non-zero fence"
        );
        assert_ne!(
            reclaimed.lease_fence, original_fence,
            "re-claim fence must differ from the parked fence"
        );
    } else {
        assert!(
            next.is_none(),
            "parked item with future retry_after must not be claimable"
        );
    }
}

/// Stale fence: park returns Ok(false) and the row is unchanged.
pub async fn park_stale_fence_returns_false_row_unchanged(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let item_id = seed(db, 5).await;

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let real_fence = claimed.lease_fence;

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
    let after = row(fx, item_id).await;
    assert_eq!(after.status, "pending", "status must still be 'pending'");
    assert_eq!(
        after.attempt, 1,
        "attempt must still be 1 (not 2 from stale park)"
    );
    if fx.kind != StoreKind::Memory {
        assert_eq!(
            after.retry_after,
            Some(parsed(&retry_after)),
            "retry_after must be the one set by the first (valid) park"
        );
    }
}

/// A completely wrong (fabricated) fence on a claimed item returns false
/// and leaves the item in 'claimed' state.
pub async fn park_wrong_fence_on_claimed_item_is_noop(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let item_id = seed(db, 5).await;

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
    assert_eq!(
        row(fx, item_id).await.status,
        "claimed",
        "item must remain 'claimed' after stale-fence park"
    );
}

/// A stale fence from a different attempt is rejected, and the lease_epoch
/// is bumped for re-claim.
pub async fn park_fence_guard_and_epoch_bump(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let item_id = seed(db, 5).await;

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let fence = claimed.lease_fence;

    // Read epoch before parking.
    let epoch_before = row(fx, item_id).await.lease_epoch;

    // Correct fence → parked.
    let retry_after = future_retry_after();
    let ok = db
        .park_work_item(item_id, fence, &retry_after, 1)
        .await
        .unwrap();
    assert!(ok, "park with correct fence must return true");

    // Verify epoch bumped.
    assert_eq!(
        row(fx, item_id).await.lease_epoch,
        epoch_before + 1,
        "park must bump lease_epoch"
    );

    // Stale fence → no-op.
    let noop = db
        .park_work_item(item_id, fence, &future_retry_after(), 2)
        .await
        .unwrap();
    assert!(!noop, "stale fence must be a no-op");

    // Attempt must still be 1.
    assert_eq!(
        row(fx, item_id).await.attempt,
        1,
        "attempt must be unchanged after stale-fence park"
    );
}

/// Park with a PAST retry_after makes the item immediately claimable.
///
/// This is the complement of the "future = not claimable" assertion in
/// `park_correct_fence_returns_true_and_resets_row`. Together, the two
/// cases prove the retry_after filter in both directions without any real
/// sleep. The reclaimed item must carry the bumped attempt and a fresh
/// non-zero fence that differs from the fence the parking worker held.
pub async fn park_past_retry_after_is_immediately_claimable(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let item_id = seed(db, 5).await;

    let claimed = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimable");
    let fence = claimed.lease_fence;

    // Park with a timestamp in the PAST — item should be visible on the next claim.
    let parked = db
        .park_work_item(item_id, fence, &past_retry_after(), 1)
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

/// The full fence-safety sequence:
///   claim(F1) → park(F1, past_ts, 1) → reclaim(F2) → stale-park(F1, .., 2) = false.
///
/// `park_stale_fence_returns_false_row_unchanged` exercises
/// claim→park→stale-park. This adds the RECLAIM step in between, proving
/// that F1 is truly stale after the item has been reclaimed by a new worker
/// holding F2, and that the newer attempt's state (attempt=1) survives the
/// rejected stale park.
pub async fn fence_safety_claim_park_reclaim_stale_park(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let item_id = seed(db, 5).await;

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
    let after = row(fx, item_id).await;
    assert_eq!(
        after.status, "claimed",
        "item must still be 'claimed' by worker-2"
    );
    assert_eq!(
        after.attempt, 1,
        "attempt must be 1; stale park must not advance it"
    );
}

/// Each successful park increments attempt by exactly 1 and bumps
/// lease_epoch, so the next claim mints a strictly different fence.
///
/// Sequence: claim → park(1) → reclaim(F2) → park(2) → reclaim(F3).
/// Past timestamps are used so both reclaims succeed without sleeping.
///
/// Note: `claim_work_item` also bumps `lease_epoch` (so the fence is strictly
/// increasing). The epoch is read AFTER each claim so the assertion only checks
/// the delta contributed by `park_work_item` itself (+1 per park).
pub async fn attempt_monotonicity_and_epoch_freshness(fx: &Fixture) {
    let db = fx.backend.as_ref();
    // Headroom for two parks.
    let item_id = seed(db, 10).await;

    // ── Park 1: attempt 0 → 1 ────────────────────────────────────────────────
    let c1 = db
        .claim_work_item("w1", &["model"])
        .await
        .unwrap()
        .expect("first claim must succeed");
    let f1 = c1.lease_fence;
    assert!(f1 > 0, "fence must be non-zero after claim");

    // Read epoch AFTER claim so we measure the park delta (+1) cleanly.
    let epoch_after_claim1 = row(fx, item_id).await.lease_epoch;

    db.park_work_item(item_id, f1, &past_retry_after(), 1)
        .await
        .unwrap();

    assert_eq!(
        row(fx, item_id).await.lease_epoch,
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
    assert_ne!(f2, 0, "fence must be non-zero after re-claim");
    assert_eq!(
        c2.attempt, 1,
        "item must be at attempt=1 before second park"
    );

    // Read epoch after second claim.
    let epoch_after_claim2 = row(fx, item_id).await.lease_epoch;

    db.park_work_item(item_id, f2, &past_retry_after(), 2)
        .await
        .unwrap();

    let after = row(fx, item_id).await;
    assert_eq!(
        after.lease_epoch,
        epoch_after_claim2 + 1,
        "second park must bump lease_epoch by exactly 1"
    );
    assert_eq!(after.attempt, 2, "attempt must be 2 after second park");

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
