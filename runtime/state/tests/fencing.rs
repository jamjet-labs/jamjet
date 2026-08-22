//! Lease-fencing tests: a zombie worker cannot double-commit; the fence is
//! monotonic across reclaim and survives a simulated store failover.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, StateBackendError, WorkItem},
    Event, EventKind, InMemoryBackend, SqliteBackend, TenantId,
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
        parent_execution_id: None,
        segment_number: 0,
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

    let claimed = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
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
    let first = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    assert!(
        first.lease_fence > 0,
        "first claim must mint a non-zero fence"
    );

    // Backdate the lease so the stale-expiry path inside claim_work_item fires.
    // (fail_work_item sets status='failed', not 'pending', so it is not
    //  directly re-claimable — see adaptation note above.)
    db.force_lease_expired_for_test(first.id).await.unwrap();

    // Re-claim: claim_work_item's stale-expiry UPDATE bumps lease_epoch+1 and
    // resets status='pending'; the subsequent INSERT mints a strictly greater fence.
    let second = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();
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
        idempotency_key: None,
    }
}

fn node_failed(node: &str) -> EventKind {
    EventKind::NodeFailed {
        node_id: node.into(),
        error: "boom".into(),
        attempt: 0,
        retryable: false,
    }
}

#[tokio::test]
async fn commit_succeeds_with_correct_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    let item = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let event = Event::new(eid.clone(), 0, node_completed("n1"));
    let seq = db
        .commit_turn(item.id, item.lease_fence, event, false)
        .await
        .expect("commit with correct fence");
    assert!(seq >= 1);

    // Exactly one event was appended.
    let events = db.get_events(&eid).await.unwrap();
    assert_eq!(events.len(), 1);
    // Item is settled — no longer claimable.
    assert!(db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .is_none());

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
    let zombie = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    db.force_lease_expired_for_test(zombie.id).await.unwrap();
    let _b = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();

    let event = Event::new(eid.clone(), 0, node_completed("n1"));
    let err = db
        .commit_turn(zombie.id, zombie.lease_fence, event, false)
        .await
        .expect_err("zombie commit must fail closed");
    assert!(matches!(err, StateBackendError::FenceLost(_)));

    // The zombie's commit emitted NOTHING.
    assert_eq!(db.get_events(&eid).await.unwrap().len(), 0);

    std::fs::remove_file(&path).ok();
}

/// Simulates a lost-tail failover: the primary store drops while a worker
/// holds a lease. A promoted store (same DB file, bumped term) is opened,
/// the stale lease is expired and re-claimed under the new term. The original
/// zombie fence (minted under term 0) is rejected on the promoted store.
///
/// Central invariant: a fence minted under term N is rejected after promotion
/// to term N+1. The fence packs the store term in the high 32 bits, so any
/// term-0 fence is numerically less than any term-1 fence, and the
/// `AND lease_fence = ?` WHERE clause in commit_node_terminal will find zero
/// rows -> FenceLost, zero events written.
///
/// Mechanics note: rather than copying a DB file (the brief's two-file sketch),
/// we drop the `SqliteBackend` handle (simulating the primary going away) and
/// re-open the same on-disk file as the "promoted" store. The data persists
/// because SQLite WAL files survive the connection close. This is deterministic
/// without filesystem copies.
#[tokio::test]
async fn fence_survives_lost_tail_failover() {
    let path = temp_db_path();
    let eid = ExecutionId::new();
    let zombie_fence: i64;
    let zombie_item_id: Uuid;

    // --- Primary store: claim under term 0 ---
    {
        let primary = open_db(&path).await;
        primary
            .create_execution(sample_execution(&eid))
            .await
            .unwrap();
        zombie_item_id = primary.enqueue_work_item(sample_item(&eid)).await.unwrap();
        let z = primary
            .claim_work_item("worker-A", &["model"])
            .await
            .unwrap()
            .unwrap();
        zombie_fence = z.lease_fence; // term=0, epoch=1 -> value=1
        assert!(zombie_fence > 0, "zombie fence must be nonzero");
        // Primary "crashes" here: SqliteBackend dropped, item still in 'claimed'
        // state on disk (no commit was issued).
    }

    // --- Promoted store: same file, bump failover generation to term=1 ---
    let promoted = open_db(&path).await;
    let new_term = promoted.bump_store_term().await.unwrap();
    assert_eq!(new_term, 1, "term must be 1 after first promotion");

    // Expire the stale lease (worker-A is gone). The backdated lease_expires_at
    // causes claim_work_item's built-in stale-expiry UPDATE to reset the item
    // to 'pending' with a bumped epoch before the fresh claim.
    promoted
        .force_lease_expired_for_test(zombie_item_id)
        .await
        .unwrap();
    let fresh = promoted
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();
    // Fresh fence: term=1 * 4_294_967_296 + epoch=3 >> zombie_fence (term-0).
    assert!(
        fresh.lease_fence > zombie_fence,
        "term-1 fence {} must be > term-0 zombie fence {}",
        fresh.lease_fence,
        zombie_fence
    );

    // Central assertion: the zombie's term-0 fence is rejected on the promoted
    // store. commit_node_terminal WHERE clause `AND lease_fence = zombie_fence`
    // finds zero rows (current fence is the term-1 value) -> FenceLost.
    let ev = Event::new(eid.clone(), 0, node_completed("n1"));
    let err = promoted
        .commit_turn(zombie_item_id, zombie_fence, ev, false)
        .await
        .expect_err("term-0 zombie fence must be rejected after promotion to term 1");
    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );
    // The zombie commit must have written NOTHING.
    assert_eq!(
        promoted.get_events(&eid).await.unwrap().len(),
        0,
        "zombie must emit zero events"
    );

    std::fs::remove_file(&path).ok();
}

/// Negative control: proves the fence check catches a bad fence value even
/// when the store term has NOT changed. A fabricated fence (item.lease_fence + 1)
/// is one higher than the real fence; commit_node_terminal must reject it with
/// FenceLost and emit zero events. This demonstrates the test would catch a
/// regression that removed the fence check.
#[tokio::test]
async fn term_pin_reopens_window_negative_control() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    let item = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();

    // Fabricate a fence that is one higher than the real value.
    let wrong_fence = item.lease_fence + 1;
    let ev = Event::new(eid.clone(), 0, node_completed("n1"));
    let err = db
        .commit_turn(item.id, wrong_fence, ev, false)
        .await
        .expect_err("fabricated wrong fence must be rejected");
    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );
    assert_eq!(
        db.get_events(&eid).await.unwrap().len(),
        0,
        "wrong-fence commit must emit zero events"
    );

    std::fs::remove_file(&path).ok();
}

/// Crash-injection exactly-once-commit: worker A claims but crashes (the
/// SqliteBackend is dropped) before committing. The lease is force-expired and
/// worker B re-claims the same item. B's commit must succeed, and exactly one
/// NodeCompleted event must exist in the log — no double-write, no ghost event.
///
/// Mechanics: the item id is captured at enqueue so it can be threaded into
/// force_lease_expired_for_test after the re-open (no db_first_item_id helper
/// needed). The stale-expiry path inside claim_work_item resets the item to
/// 'pending' with a bumped epoch before the re-claim.
#[tokio::test]
async fn crash_before_commit_then_reclaim_yields_exactly_one_terminal() {
    let path = temp_db_path();
    let eid = ExecutionId::new();
    let item_id: Uuid;

    // Worker A claims but "crashes" before commit (SqliteBackend dropped).
    {
        let db = open_db(&path).await;
        db.create_execution(sample_execution(&eid)).await.unwrap();
        item_id = db.enqueue_work_item(sample_item(&eid)).await.unwrap();
        let _a = db
            .claim_work_item("worker-A", &["model"])
            .await
            .unwrap()
            .unwrap();
        // db dropped here; item remains in 'claimed' state, no events written.
    }

    // New "process": re-open the same DB, expire worker-A's stale lease,
    // re-claim as worker B, commit.
    let db = open_db(&path).await;
    db.force_lease_expired_for_test(item_id).await.unwrap();
    let b = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();
    let ev = Event::new(eid.clone(), 0, node_completed("n1"));
    db.commit_turn(b.id, b.lease_fence, ev, false)
        .await
        .expect("worker-B commit must succeed after re-claim");

    // Exactly one terminal event must exist — the zombie A never committed.
    let completions = db
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::NodeCompleted { .. }))
        .count();
    assert_eq!(
        completions, 1,
        "exactly one NodeCompleted must exist after crash-then-reclaim; got {completions}"
    );

    std::fs::remove_file(&path).ok();
}

/// `commit_node_terminal` with a `NodeFailed` event and the correct fence must
/// succeed, settle the item as failed (lease_expires_at=NULL, worker_id=NULL,
/// completed_at=NULL), and append exactly one NodeFailed event to the log.
#[tokio::test]
async fn commit_node_terminal_node_failed_settles_failed() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    let item = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let event = Event::new(eid.clone(), 0, node_failed("n1"));
    let seq = db
        .commit_turn(item.id, item.lease_fence, event, false)
        .await
        .expect("commit NodeFailed with correct fence must succeed");
    assert!(seq >= 1, "sequence must be at least 1");

    // Exactly one event in the log, and it is a NodeFailed.
    let events = db.get_events(&eid).await.unwrap();
    assert_eq!(
        events.len(),
        1,
        "exactly one event must exist after NodeFailed commit"
    );
    assert!(
        matches!(events[0].kind, EventKind::NodeFailed { .. }),
        "the committed event must be NodeFailed, got {:?}",
        events[0].kind
    );

    std::fs::remove_file(&path).ok();
}

/// A zombie (stale fence) attempting to commit a `NodeFailed` event must be
/// rejected with `FenceLost` and must write zero events to the log.
#[tokio::test]
async fn commit_node_terminal_node_failed_stale_fence_fails_closed() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    // Worker A claims (fence F1), then has its lease expired and re-claimed by B.
    let zombie = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    db.force_lease_expired_for_test(zombie.id).await.unwrap();
    let _b = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();

    // Zombie tries to commit a NodeFailed with stale fence F1.
    let event = Event::new(eid.clone(), 0, node_failed("n1"));
    let err = db
        .commit_turn(zombie.id, zombie.lease_fence, event, false)
        .await
        .expect_err("zombie NodeFailed commit must fail closed");
    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );

    // Zero events written — the zombie's NodeFailed must not appear.
    assert_eq!(
        db.get_events(&eid).await.unwrap().len(),
        0,
        "zombie NodeFailed commit must emit zero events"
    );

    std::fs::remove_file(&path).ok();
}

// ── Cross-backend helpers ─────────────────────────────────────────────────────

/// Generic helper: asserts that `commit_node_terminal` with the CORRECT fence
/// succeeds and appends exactly one event. Backend-agnostic.
async fn assert_fence_commit_succeeds(backend: &dyn StateBackend) {
    let eid = ExecutionId::new();
    backend
        .create_execution(sample_execution(&eid))
        .await
        .unwrap();
    backend.enqueue_work_item(sample_item(&eid)).await.unwrap();
    let item = backend
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let ev = Event::new(eid.clone(), 0, node_completed("n1"));
    let seq = backend
        .commit_turn(item.id, item.lease_fence, ev, false)
        .await
        .expect("commit with correct fence must succeed");
    assert!(seq >= 1, "sequence must be at least 1");
    assert_eq!(
        backend.get_events(&eid).await.unwrap().len(),
        1,
        "exactly one event must exist after successful commit"
    );
}

/// Generic helper: asserts that `commit_node_terminal` with a FABRICATED wrong
/// fence returns FenceLost and writes zero events. Backend-agnostic.
async fn assert_fence_commit_fails_stale(backend: &dyn StateBackend) {
    let eid = ExecutionId::new();
    backend
        .create_execution(sample_execution(&eid))
        .await
        .unwrap();
    backend.enqueue_work_item(sample_item(&eid)).await.unwrap();
    let item = backend
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let wrong_fence = item.lease_fence + 1; // fabricated — one higher than real
    let ev = Event::new(eid.clone(), 0, node_completed("n1"));
    let err = backend
        .commit_turn(item.id, wrong_fence, ev, false)
        .await
        .expect_err("fabricated wrong fence must be rejected");
    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );
    assert_eq!(
        backend.get_events(&eid).await.unwrap().len(),
        0,
        "stale-fence commit must emit zero events"
    );
}

// ── InMemoryBackend cross-backend tests ───────────────────────────────────────

#[tokio::test]
async fn commit_succeeds_with_correct_fence_inmemory() {
    let backend = InMemoryBackend::new();
    assert_fence_commit_succeeds(&backend).await;
}

#[tokio::test]
async fn commit_fails_closed_with_stale_fence_inmemory() {
    let backend = InMemoryBackend::new();
    assert_fence_commit_fails_stale(&backend).await;
}

// ── TenantScopedSqliteBackend cross-backend tests ─────────────────────────────

#[tokio::test]
async fn commit_succeeds_with_correct_fence_tenant_scoped() {
    let path = temp_db_path();
    // open_db runs migrations; for_tenant creates a scoped view over the same pool.
    let base = open_db(&path).await;
    let backend = base.for_tenant(TenantId::default_tenant());
    assert_fence_commit_succeeds(&backend).await;
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn commit_fails_closed_with_stale_fence_tenant_scoped() {
    let path = temp_db_path();
    let base = open_db(&path).await;
    let backend = base.for_tenant(TenantId::default_tenant());
    assert_fence_commit_fails_stale(&backend).await;
    std::fs::remove_file(&path).ok();
}

// ── NodeFailed cross-backend helpers ─────────────────────────────────────────

/// Generic helper: asserts that `commit_node_terminal` with a `NodeFailed` event
/// and the CORRECT fence succeeds and appends exactly one NodeFailed event.
async fn assert_fence_node_failed_succeeds(backend: &dyn StateBackend) {
    let eid = ExecutionId::new();
    backend
        .create_execution(sample_execution(&eid))
        .await
        .unwrap();
    backend.enqueue_work_item(sample_item(&eid)).await.unwrap();
    let item = backend
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let ev = Event::new(eid.clone(), 0, node_failed("n1"));
    let seq = backend
        .commit_turn(item.id, item.lease_fence, ev, false)
        .await
        .expect("NodeFailed commit with correct fence must succeed");
    assert!(seq >= 1, "sequence must be at least 1");
    let events = backend.get_events(&eid).await.unwrap();
    assert_eq!(
        events.len(),
        1,
        "exactly one event must exist after NodeFailed commit"
    );
    assert!(
        matches!(events[0].kind, EventKind::NodeFailed { .. }),
        "the committed event must be NodeFailed"
    );
}

/// Generic helper: asserts that `commit_node_terminal` with a `NodeFailed` event
/// and a FABRICATED wrong fence returns FenceLost and writes zero events.
async fn assert_fence_node_failed_stale_fails(backend: &dyn StateBackend) {
    let eid = ExecutionId::new();
    backend
        .create_execution(sample_execution(&eid))
        .await
        .unwrap();
    backend.enqueue_work_item(sample_item(&eid)).await.unwrap();
    let item = backend
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let wrong_fence = item.lease_fence + 1;
    let ev = Event::new(eid.clone(), 0, node_failed("n1"));
    let err = backend
        .commit_turn(item.id, wrong_fence, ev, false)
        .await
        .expect_err("stale-fence NodeFailed must be rejected");
    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );
    assert_eq!(
        backend.get_events(&eid).await.unwrap().len(),
        0,
        "stale-fence NodeFailed commit must emit zero events"
    );
}

// ── NodeFailed — InMemoryBackend ──────────────────────────────────────────────

#[tokio::test]
async fn commit_node_failed_succeeds_with_correct_fence_inmemory() {
    let backend = InMemoryBackend::new();
    assert_fence_node_failed_succeeds(&backend).await;
}

#[tokio::test]
async fn commit_node_failed_stale_fence_fails_closed_inmemory() {
    let backend = InMemoryBackend::new();
    assert_fence_node_failed_stale_fails(&backend).await;
}

// ── NodeFailed — TenantScopedSqliteBackend ────────────────────────────────────

#[tokio::test]
async fn commit_node_failed_succeeds_with_correct_fence_tenant_scoped() {
    let path = temp_db_path();
    let base = open_db(&path).await;
    let backend = base.for_tenant(TenantId::default_tenant());
    assert_fence_node_failed_succeeds(&backend).await;
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn commit_node_failed_stale_fence_fails_closed_tenant_scoped() {
    let path = temp_db_path();
    let base = open_db(&path).await;
    let backend = base.for_tenant(TenantId::default_tenant());
    assert_fence_node_failed_stale_fails(&backend).await;
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn set_store_term_at_least_is_monotonic_and_lifts_the_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();

    // Term starts at 0; lift to 5 (e.g. the promotion generation at startup).
    assert_eq!(db.set_store_term_at_least(5).await.unwrap(), 5);
    // Monotonic: a lower value is a no-op.
    assert_eq!(db.set_store_term_at_least(3).await.unwrap(), 5);
    // Equal is a no-op.
    assert_eq!(db.set_store_term_at_least(5).await.unwrap(), 5);

    // A fence minted now carries term 5: fence == 5 * 2^32 + epoch (epoch >= 1).
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();
    let claimed = db.claim_work_item("w", &["model"]).await.unwrap().unwrap();
    const BAND: i64 = 4_294_967_296; // 2^32
    assert!(
        claimed.lease_fence >= 5 * BAND && claimed.lease_fence < 6 * BAND,
        "fence {} must be in the term-5 band [{}, {})",
        claimed.lease_fence,
        5 * BAND,
        6 * BAND
    );
    std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn set_store_term_at_least_monotonic_in_memory() {
    use jamjet_state::InMemoryBackend;
    let db = InMemoryBackend::new();
    assert_eq!(db.set_store_term_at_least(7), 7);
    assert_eq!(db.set_store_term_at_least(4), 7); // lower is a no-op
    assert_eq!(db.set_store_term_at_least(9), 9);
}

// ── Work-item lifecycle: attempts and the reclaim race ───────────────────────

/// A node that reliably kills its worker must EXHAUST its attempts, not loop.
///
/// `claim_work_item` expires stale leases itself, and that fast path races the
/// scheduler's reclaim sweep — the only other place attempts increment — and
/// normally wins, because it runs on every poll while the sweep runs on an
/// interval. When it reset the item without incrementing, the item came back at
/// attempt 0 forever: max_attempts unreachable, dead-letter never entered, loop
/// unbounded. This walks the exact loop and asserts it terminates.
#[tokio::test]
async fn claim_side_lease_expiry_consumes_attempts() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    let mut item = sample_item(&eid);
    item.max_attempts = 3;
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    // Attempt 0: claim, then die without settling.
    let first = db
        .claim_work_item("worker-dies", &["model"])
        .await
        .unwrap()
        .expect("first claim");
    assert_eq!(first.attempt, 0);
    db.force_lease_expired_for_test(item_id).await.unwrap();

    // Attempt 1: the claim-side expiry must have consumed an attempt.
    let second = db
        .claim_work_item("worker-dies", &["model"])
        .await
        .unwrap()
        .expect("second claim");
    assert_eq!(
        second.attempt, 1,
        "the claim-side lease expiry must consume an attempt, or the node loops forever"
    );
    db.force_lease_expired_for_test(item_id).await.unwrap();

    // Attempt 2 is the last one under max_attempts = 3.
    let third = db
        .claim_work_item("worker-dies", &["model"])
        .await
        .unwrap()
        .expect("third claim");
    assert_eq!(third.attempt, 2);
    db.force_lease_expired_for_test(item_id).await.unwrap();

    // Budget spent: the fast path must NOT resurrect it. The item stays
    // 'claimed' with an expired lease, which is exactly what the reclaim sweep
    // selects — so the sweep dead-letters it, WITH the NodeFailed only the
    // sweep's caller can emit.
    let fourth = db.claim_work_item("worker-dies", &["model"]).await.unwrap();
    assert!(
        fourth.is_none(),
        "an item that has spent its attempts must not be handed out again"
    );

    let reclaimed = db.reclaim_expired_leases().await.unwrap();
    assert_eq!(
        reclaimed.exhausted.len(),
        1,
        "the sweep must dead-letter it so a NodeFailed is emitted"
    );
    assert!(reclaimed.retryable.is_empty());

    let _ = std::fs::remove_file(&path);
}

/// The reclaim sweep must never resurrect an item settled under it.
///
/// The sweep does ONE SELECT for every expired item, then UPDATEs them one at a
/// time. A worker that commits inside that loop used to have its COMPLETED item
/// flipped back to pending by an unguarded `WHERE id = ?`, then re-claimed and
/// re-run at a shifted step ordinal — a different idempotency key, so the replay
/// guard did not catch it and the side effect fired twice.
///
/// Opening the window deterministically takes volume: a settle done BEFORE the
/// call also changes `status`, which the sweep\'s own SELECT then filters out, so
/// nothing races. With many expired items the update loop is long enough for a
/// concurrent settle to land inside it, against a row the SELECT already
/// captured. Verified to fail against the unguarded UPDATE before being kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reclaim_cannot_resurrect_an_item_settled_under_it() {
    const ITEMS: usize = 400;

    let path = temp_db_path();
    let db = std::sync::Arc::new(open_db(&path).await);
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();

    // Every item is expired and claimed, so the sweep captures all of them.
    let mut ids = Vec::with_capacity(ITEMS);
    for _ in 0..ITEMS {
        let item = sample_item(&eid);
        let id = item.id;
        db.enqueue_work_item(item).await.unwrap();
        ids.push(id);
    }
    let mut fences = Vec::with_capacity(ITEMS);
    for _ in 0..ITEMS {
        let c = db
            .claim_work_item("worker-slow", &["model"])
            .await
            .unwrap()
            .expect("claim");
        fences.push((c.id, c.lease_fence));
        db.force_lease_expired_for_test(c.id).await.unwrap();
    }

    // Settle the LAST items the sweep will reach, while it is still working
    // through the earlier ones.
    let victims: Vec<(uuid::Uuid, i64)> = fences.iter().rev().take(40).copied().collect();
    let settler = {
        let db = db.clone();
        let victims = victims.clone();
        tokio::spawn(async move {
            let mut settled = Vec::new();
            for (id, fence) in victims {
                if db.complete_work_item_fenced(id, fence).await.unwrap() {
                    settled.push(id);
                }
            }
            settled
        })
    };

    let reclaimed = db.reclaim_expired_leases().await.unwrap();
    let settled = settler.await.unwrap();

    // Whatever the interleaving, an item whose completion won its fence is
    // finished: it must not be back in the queue, and must not be reported as
    // reclaimed, because the caller emits NodeFailed from those lists.
    let reported: std::collections::HashSet<uuid::Uuid> = reclaimed
        .retryable
        .iter()
        .chain(reclaimed.exhausted.iter())
        .map(|i| i.id)
        .collect();
    for id in &settled {
        assert!(
            !reported.contains(id),
            "a COMPLETED item was reported as reclaimed ({id}) — its node \
             succeeded, and the caller will now emit NodeFailed for it"
        );
        let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap();
        assert_eq!(
            status, "completed",
            "a COMPLETED item was resurrected to {status:?} ({id}) — it will be \
             re-claimed and fire its side effect a second time"
        );
    }
    assert!(
        !settled.is_empty(),
        "the race never opened; the test proved nothing"
    );

    let _ = std::fs::remove_file(&path);
}

// ── Stale fences after a requeue ─────────────────────────────────────────────

/// A worker that just failed its item must not be able to park the requeue.
///
/// `fail_work_item_fenced`'s retryable branch puts the item back to `pending`.
/// It used to leave `lease_fence` at the failing worker's value, and
/// `park_work_item` matched on `id` + `lease_fence` with no status guard — so
/// the worker that had just lost the item could park it, overwriting the
/// `attempt` and `retry_after` of an attempt that is no longer its own.
///
/// Two independent guards now close it: the requeue clears the fence (as
/// `park_work_item` itself always did), and park requires the item to still be
/// claimed.
#[tokio::test]
async fn a_failed_worker_cannot_park_its_requeued_item() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    let item = sample_item(&eid);
    let item_id = item.id;
    db.enqueue_work_item(item).await.unwrap();

    let claimed = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .expect("claim");

    // The worker reports a failure; attempts remain, so the item is requeued.
    let outcome = db
        .fail_work_item_fenced(item_id, claimed.lease_fence, "boom")
        .await
        .unwrap()
        .expect("the fence matched, so it settles");
    assert!(matches!(
        outcome,
        jamjet_state::backend::FailOutcome::Retryable { .. }
    ));

    // Same worker, same (now stale) fence, tries to park what it no longer owns.
    let parked = db
        .park_work_item(
            item_id,
            claimed.lease_fence,
            "2030-01-01T00:00:00+00:00",
            99,
        )
        .await
        .unwrap();
    assert!(
        !parked,
        "a worker that already failed this item must not park the requeue — it \
         would overwrite the attempt and retry_after of an attempt that is not its own"
    );

    // And the requeued attempt is intact. Asserted on the row rather than by
    // re-claiming: the retryable branch sets a backoff, so the item is
    // deliberately NOT claimable yet, and a claim here would test the backoff
    // instead of the clobber.
    let (status, attempt, fence): (String, i64, i64) =
        sqlx::query_as("SELECT status, attempt, lease_fence FROM work_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&db.pool())
            .await
            .unwrap();
    assert_eq!(status, "pending", "the item is requeued");
    assert_eq!(
        attempt, 1,
        "the failed attempt was consumed once — a successful park would have \
         overwritten this with its own next_attempt"
    );
    assert_eq!(
        fence, 0,
        "the requeue must clear the stale fence, exactly as park_work_item does"
    );

    let _ = std::fs::remove_file(&path);
}
