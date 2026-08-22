//! `POST /work-items/:id/fail` — the external worker tier's failure path.
//!
//! The endpoint used to be a single unfenced `fail_work_item` call. That wrote
//! `status = 'failed'`, a status no sweep ever selects, and appended NOTHING —
//! so the scheduler fold kept the node in `scheduled`, the execution stayed
//! `Running` forever, and the item could never be reclaimed. Every legitimate
//! Python/Java tool failure stranded its workflow permanently.
//!
//! These tests pin the two properties that fix requires: a worker-reported
//! failure produces the SAME events as a lease that expired (so the fold cannot
//! tell them apart), and a worker that no longer holds the lease cannot settle
//! or narrate the item at all.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend, DEFAULT_TENANT};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

fn make_state(backend: Arc<dyn StateBackend>) -> AppState {
    let backend_for_fn = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| backend_for_fn.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
    }
}

/// Enqueue one claimable item, then claim it so the test holds a real fence.
async fn seed_and_claim(
    backend: &Arc<dyn StateBackend>,
    max_attempts: u32,
    attempt: u32,
) -> (ExecutionId, Uuid, i64) {
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "wf".into(),
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
        })
        .await
        .expect("create_execution");
    let id = Uuid::new_v4();
    backend
        .enqueue_work_item(WorkItem {
            id,
            execution_id: execution_id.clone(),
            node_id: "n1".into(),
            queue_type: "python_tool".into(),
            payload: json!({"node_id": "n1"}),
            attempt,
            max_attempts,
            created_at: now,
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .expect("enqueue_work_item");

    let claimed = backend
        .claim_work_item("external-python-worker-0", &["python_tool"])
        .await
        .expect("claim")
        .expect("an item must be claimable");
    assert_eq!(claimed.id, id, "the seeded item must be the claimed one");
    (execution_id, id, claimed.lease_fence)
}

async fn post_fail(state: &AppState, id: Uuid, body: Value) -> (StatusCode, Value) {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post(format!("/work-items/{id}/fail"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn events(backend: &Arc<dyn StateBackend>, execution_id: &ExecutionId) -> Vec<Event> {
    backend.get_events(execution_id).await.expect("get_events")
}

fn node_failed(events: &[Event]) -> Option<(String, bool, u32)> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::NodeFailed {
            error,
            retryable,
            attempt,
            ..
        } => Some((error.clone(), *retryable, *attempt)),
        _ => None,
    })
}

fn retry_scheduled(events: &[Event]) -> Option<(u32, u64)> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::RetryScheduled {
            attempt, delay_ms, ..
        } => Some((*attempt, *delay_ms)),
        _ => None,
    })
}

// ── The wedge itself ─────────────────────────────────────────────────────────

/// The regression that matters: a failure must leave a terminal event behind.
///
/// Without one the node stays in the fold's `scheduled` set and the execution
/// never completes, which is what stranded every external tool failure.
#[tokio::test]
async fn a_fenced_failure_emits_node_failed_and_reschedules() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (status, body) = post_fail(
        &state,
        id,
        json!({"error": "tool raised ValueError", "lease_fence": fence}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["failed"], json!(true));
    assert_eq!(body["retryable"], json!(true), "attempts remain, so retry");

    let evs = events(&backend, &execution_id).await;
    let (error, retryable, attempt) =
        node_failed(&evs).expect("a failure MUST append NodeFailed — without it the node strands");
    assert_eq!(
        error, "tool raised ValueError",
        "the worker's reason survives"
    );
    assert!(retryable);
    assert_eq!(attempt, 0, "the attempt that just failed");

    let (retry_attempt, delay_ms) =
        retry_scheduled(&evs).expect("a retryable failure MUST also schedule the retry");
    assert_eq!(retry_attempt, 1);
    assert!(delay_ms > 0, "the retry must back off, got {delay_ms}ms");
}

/// The last attempt is terminal: NodeFailed{retryable:false} and NO retry.
#[tokio::test]
async fn the_final_attempt_dead_letters_without_scheduling_a_retry() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    // attempt 2 of max 3 — failing it makes 3, which exhausts the budget.
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 2).await;
    let state = make_state(backend.clone());

    let (status, body) = post_fail(
        &state,
        id,
        json!({"error": "still broken", "lease_fence": fence}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["retryable"], json!(false));

    let evs = events(&backend, &execution_id).await;
    let (_, retryable, attempt) = node_failed(&evs).expect("NodeFailed");
    assert!(!retryable, "an exhausted node is permanently dead");
    assert_eq!(attempt, 3, "the final attempt count");
    assert!(
        retry_scheduled(&evs).is_none(),
        "a dead-lettered node must NOT be rescheduled"
    );
}

// ── The fence ────────────────────────────────────────────────────────────────

/// A worker that lost its lease cannot fail the item, and cannot narrate it
/// either: emitting NodeFailed for an item another worker now holds would let a
/// zombie kill a live node.
#[tokio::test]
async fn a_stale_fence_is_refused_and_emits_nothing() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (status, body) = post_fail(
        &state,
        id,
        json!({"error": "zombie worker", "lease_fence": fence + 1}),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["failed"], json!(false));
    assert!(
        events(&backend, &execution_id).await.is_empty(),
        "a refused failure must leave no trace in the log"
    );
}

/// `lease_fence: 0` is the shape a forged or defaulted request takes. A pending
/// item carries fence 0, so a fence-only check would match it; the still-claimed
/// guard is what refuses this.
#[tokio::test]
async fn a_zero_fence_does_not_match_an_unclaimed_item() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, _fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (status, _) = post_fail(&state, id, json!({"error": "forged", "lease_fence": 0})).await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(events(&backend, &execution_id).await.is_empty());
}

/// Failing twice with the same fence must not double-emit: the first failure
/// releases the lease, so the replay finds nothing of its own to settle.
#[tokio::test]
async fn replaying_a_failure_with_the_same_fence_is_refused() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (first, _) = post_fail(&state, id, json!({"error": "boom", "lease_fence": fence})).await;
    assert_eq!(first, StatusCode::OK);

    let (second, _) = post_fail(&state, id, json!({"error": "boom", "lease_fence": fence})).await;
    assert_eq!(
        second,
        StatusCode::CONFLICT,
        "the second report no longer holds the lease"
    );

    let failures = events(&backend, &execution_id)
        .await
        .iter()
        .filter(|e| matches!(e.kind, EventKind::NodeFailed { .. }))
        .count();
    assert_eq!(failures, 1, "exactly one NodeFailed, not one per replay");
}

// ── The durable backend ──────────────────────────────────────────────────────
//
// Everything above runs on the in-memory backend, which is exactly how the
// SQLite-vs-memory divergence class hides (review finding 2: claim-side lease
// expiry exists only in SQLite, so memory-only tests are blind to it). The
// status transitions below can ONLY be observed in the table, so they are
// asserted against the real backend.

/// A durable backend plus the raw pool, since `StateBackend` exposes no
/// work-item read and `'pending'` vs `'dead_lettered'` is the whole point.
struct Durable {
    backend: Arc<dyn StateBackend>,
    pool: sqlx::SqlitePool,
    db_path: std::path::PathBuf,
}

impl Durable {
    async fn new() -> Self {
        let db_path = std::env::temp_dir().join(format!("jjtest-fail-{}.db", Uuid::new_v4()));
        let url = format!("sqlite://{}", db_path.display());
        let backend: Arc<dyn StateBackend> = Arc::new(
            jamjet_state::SqliteBackend::open(&url)
                .await
                .expect("open sqlite"),
        );
        let pool = sqlx::SqlitePool::connect(&url).await.expect("open pool");
        Self {
            backend,
            pool,
            db_path,
        }
    }

    async fn status(&self, item_id: Uuid) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM work_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&self.pool)
            .await
            .expect("the work item row must exist")
    }

    async fn attempt(&self, item_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT attempt FROM work_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&self.pool)
            .await
            .expect("the work item row must exist")
    }

    async fn dead_letter_rows(&self, item_id: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM dead_letter_items WHERE id = ?")
            .bind(item_id.to_string())
            .fetch_one(&self.pool)
            .await
            .expect("count")
    }

    async fn cleanup(self) {
        self.pool.close().await;
        let _ = std::fs::remove_file(&self.db_path);
    }
}

/// On the durable backend a retryable failure must return the item to the queue,
/// not leave it in the `'failed'` dead end nothing selects.
#[tokio::test]
async fn durable_retryable_failure_returns_the_item_to_pending() {
    let d = Durable::new().await;
    let (_execution_id, id, fence) = seed_and_claim(&d.backend, 3, 0).await;
    let state = make_state(d.backend.clone());

    let (status, _) = post_fail(&state, id, json!({"error": "boom", "lease_fence": fence})).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        d.status(id).await,
        "pending",
        "a retryable failure must be claimable again — 'failed' is the dead end this fixes"
    );
    assert_eq!(d.attempt(id).await, 1, "the attempt must be consumed");
    assert_eq!(
        d.dead_letter_rows(id).await,
        0,
        "a retryable failure is not dead-lettered"
    );
    d.cleanup().await;
}

/// The exhausting failure must dead-letter durably, in both tables.
#[tokio::test]
async fn durable_exhausted_failure_dead_letters() {
    let d = Durable::new().await;
    let (_execution_id, id, fence) = seed_and_claim(&d.backend, 3, 2).await;
    let state = make_state(d.backend.clone());

    let (status, body) =
        post_fail(&state, id, json!({"error": "final", "lease_fence": fence})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["retryable"], json!(false));

    assert_eq!(d.status(id).await, "dead_lettered");
    assert_eq!(
        d.dead_letter_rows(id).await,
        1,
        "the dead-letter queue is what makes an exhausted node recoverable by an operator"
    );
    d.cleanup().await;
}

/// A refused failure must not leave a half-written dead-letter row behind — the
/// settle is guarded first precisely so the insert cannot outlive it.
#[tokio::test]
async fn durable_stale_fence_writes_nothing_at_all() {
    let d = Durable::new().await;
    let (execution_id, id, fence) = seed_and_claim(&d.backend, 3, 2).await;
    let state = make_state(d.backend.clone());

    let (status, _) = post_fail(
        &state,
        id,
        json!({"error": "zombie", "lease_fence": fence + 7}),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        d.status(id).await,
        "claimed",
        "the real holder still owns the item"
    );
    assert_eq!(d.dead_letter_rows(id).await, 0, "no orphan dead-letter row");
    assert!(events(&d.backend, &execution_id).await.is_empty());
    d.cleanup().await;
}

// ── Legacy path ──────────────────────────────────────────────────────────────

/// The unfenced path still works for callers that predate the fence, but it is
/// honest about what it does not do. Pinning this keeps the deprecation
/// deliberate rather than accidental.
#[tokio::test]
async fn the_unfenced_legacy_path_still_settles_but_warns() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, _fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (status, body) = post_fail(&state, id, json!({"error": "old client"})).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["failed"], json!(true));
    assert_eq!(body["retryable"], json!(false));
    assert!(
        body["warning"]
            .as_str()
            .is_some_and(|w| w.contains("lease_fence")),
        "the response must tell the caller how to stop stranding nodes; got {body}"
    );
    assert!(
        events(&backend, &execution_id).await.is_empty(),
        "the legacy path emits nothing — that is exactly why it is deprecated"
    );
}

// ── /complete: the settle and the terminal event are ONE transaction ─────────

async fn post_complete(state: &AppState, id: Uuid, body: Value) -> (StatusCode, Value) {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post(format!("/work-items/{id}/complete"))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn node_completed_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::NodeCompleted { .. }))
        .count()
}

/// A fenced completion settles the item AND emits `NodeCompleted` together.
///
/// These used to be separate statements. A crash in the window left the item
/// settled with no terminal event, so the scheduler fold kept the node in
/// `scheduled` and the execution never finished — permanently, because the fold
/// replays the log and reproduces the same gap every time.
#[tokio::test]
async fn a_fenced_completion_settles_and_emits_together() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (status, body) = post_complete(
        &state,
        id,
        json!({
            "execution_id": execution_id.to_string(),
            "node_id": "n1",
            "output": {"ok": true},
            "state_patch": {"ok": true},
            "duration_ms": 5,
            "lease_fence": fence,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["completed"], json!(true));
    assert_eq!(
        node_completed_count(&events(&backend, &execution_id).await),
        1,
        "a completion must leave exactly one NodeCompleted — without it the node \
         stays scheduled and the execution never reaches a terminal state"
    );
}

/// A stale fence completes nothing and emits nothing.
#[tokio::test]
async fn a_stale_fence_completion_is_refused_and_emits_nothing() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let (status, _) = post_complete(
        &state,
        id,
        json!({
            "execution_id": execution_id.to_string(),
            "node_id": "n1",
            "output": {},
            "state_patch": {},
            "lease_fence": fence + 9,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        node_completed_count(&events(&backend, &execution_id).await),
        0,
        "a rejected completion must not emit a terminal event for work it did not settle"
    );
}

/// Replaying a completion must not emit a second `NodeCompleted`.
///
/// The fence is consumed by the first settle, so the replay finds nothing of its
/// own to commit — a duplicate terminal event would corrupt the fold.
#[tokio::test]
async fn replaying_a_completion_does_not_double_emit() {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let (execution_id, id, fence) = seed_and_claim(&backend, 3, 0).await;
    let state = make_state(backend.clone());

    let payload = json!({
        "execution_id": execution_id.to_string(),
        "node_id": "n1",
        "output": {},
        "state_patch": {},
        "lease_fence": fence,
    });

    let (first, _) = post_complete(&state, id, payload.clone()).await;
    assert_eq!(first, StatusCode::OK);
    let (second, _) = post_complete(&state, id, payload).await;
    assert_eq!(
        second,
        StatusCode::CONFLICT,
        "the replay no longer holds the lease"
    );

    assert_eq!(
        node_completed_count(&events(&backend, &execution_id).await),
        1,
        "exactly one NodeCompleted, not one per delivery"
    );
}
