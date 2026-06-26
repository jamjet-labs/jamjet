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
        .commit_node_terminal(item.id, item.lease_fence, event)
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
        .commit_node_terminal(zombie.id, zombie.lease_fence, event)
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
        .commit_node_terminal(zombie_item_id, zombie_fence, ev)
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
        .commit_node_terminal(item.id, wrong_fence, ev)
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
    db.commit_node_terminal(b.id, b.lease_fence, ev)
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
        .commit_node_terminal(item.id, item.lease_fence, event)
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
        .commit_node_terminal(zombie.id, zombie.lease_fence, event)
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
        .commit_node_terminal(item.id, item.lease_fence, ev)
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
        .commit_node_terminal(item.id, wrong_fence, ev)
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
        .commit_node_terminal(item.id, item.lease_fence, ev)
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
        .commit_node_terminal(item.id, wrong_fence, ev)
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
