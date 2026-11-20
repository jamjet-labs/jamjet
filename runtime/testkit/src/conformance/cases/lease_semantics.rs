//! Lease semantics found wanting by the PAS orchestrator spike (2026-09-06).
//!
//! - G9: the lease TTL was a 30 s constant. A worker now says how long a
//!   lease it needs when it claims, and renewals honour it.
//! - G6: the claim path's fast reclaim beat the scheduler sweep on every poll
//!   and wrote nothing, so a retry left no trace in the history.
//! - G8: an engine restart longer than the TTL reclaimed a live worker's
//!   lease and forced a full re-run. A heartbeat that arrives while the item
//!   is only pending because of expiry re-attaches the lease instead.
//!
//! Every case runs on every backend, the scoped stores in a tenant other
//! than the default, so they cannot drift apart: the re-attach rule once
//! held on memory and not on SQLite (PR #140 review, finding C1).
use crate::conformance::cases::support::in_tenant;
use crate::conformance::Fixture;
use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, StateBackendError, WorkItem},
    EventKind, DEFAULT_TENANT,
};
use serde_json::json;
use uuid::Uuid;

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-lease".into(),
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

fn sample_item(execution_id: &ExecutionId, queue: &str) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: "gpu_step".into(),
        queue_type: queue.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

async fn seed(backend: &dyn StateBackend, queue: &str) -> (ExecutionId, Uuid) {
    let execution_id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&execution_id))
        .await
        .unwrap();
    let item = sample_item(&execution_id, queue);
    let id = item.id;
    backend.enqueue_work_item(item).await.unwrap();
    (execution_id, id)
}

/// The backend under test. The scoped stores run in a tenant other than
/// the default, so the tenant clauses are exercised.
async fn backend(fx: &Fixture) -> (String, std::sync::Arc<dyn StateBackend>) {
    (format!("{:?}", fx.kind), in_tenant(fx, "acme").await)
}

pub async fn a_claim_names_its_lease_length_and_renewals_keep_it(fx: &Fixture) {
    let (name, backend) = backend(fx).await;
    let (_exec, id) = seed(backend.as_ref(), "gpu").await;
    let claimed = backend
        .claim_work_item_with_lease("gpu-worker", &["gpu"], Some(120))
        .await
        .unwrap()
        .expect("claimable");
    let expiry = claimed.lease_expires_at.expect("lease set");
    let secs = (expiry - Utc::now()).num_seconds();
    assert!(
        (110..=121).contains(&secs),
        "{name}: claim lease should be ~120 s, got {secs}"
    );

    backend
        .renew_lease(id, "gpu-worker", claimed.lease_fence)
        .await
        .unwrap();
    let after = backend.get_work_item(id).await.unwrap().expect("item");
    let secs = (after.lease_expires_at.unwrap() - Utc::now()).num_seconds();
    assert!(
        (110..=121).contains(&secs),
        "{name}: renewal should keep the 120 s lease, got {secs}"
    );
}

pub async fn the_fast_path_reclaim_records_the_retry_in_the_history(fx: &Fixture) {
    let (name, backend) = backend(fx).await;
    let (exec, id) = seed(backend.as_ref(), "gpu").await;
    let first = backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");
    fx.hooks.force_lease_expired(id).await;

    let second = backend
        .claim_work_item("w2", &["gpu"])
        .await
        .unwrap()
        .expect("{name}: an expired lease must be reclaimable by the next poll");
    assert_eq!(second.id, first.id);
    assert_eq!(
        second.attempt, 1,
        "{name}: the reclaim counts as an attempt"
    );

    let events = backend.get_events(&exec).await.unwrap();
    let failed = events.iter().any(|e| {
        matches!(&e.kind,
            EventKind::NodeFailed { node_id, retryable: true, .. } if node_id == "gpu_step")
    });
    let retry = events.iter().any(|e| {
        matches!(&e.kind,
            EventKind::RetryScheduled { node_id, attempt: 1, .. } if node_id == "gpu_step")
    });
    assert!(
        failed,
        "{name}: node_failed must be recorded for the lost lease, events: {events:?}"
    );
    assert!(
        retry,
        "{name}: retry_scheduled must be recorded, events: {events:?}"
    );
}

pub async fn a_heartbeat_reattaches_a_lease_lost_to_expiry_when_nobody_else_claimed(fx: &Fixture) {
    let (name, backend) = backend(fx).await;
    let (_exec, id) = seed(backend.as_ref(), "gpu").await;
    let first = backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");
    fx.hooks.force_lease_expired(id).await;
    // Another worker polls a different queue: the fast path re-queues our
    // item, but nobody takes it.
    assert!(backend
        .claim_work_item("other", &["cpu"])
        .await
        .unwrap()
        .is_none());

    let renewal = backend
        .heartbeat_lease(id, "w1", first.lease_fence)
        .await
        .unwrap_or_else(|e| panic!("{name}: the original worker must get its lease back, got {e}"));
    assert!(
        renewal.reattached,
        "{name}: renewal must report the re-attach"
    );
    assert!(
        renewal.lease_fence > first.lease_fence,
        "{name}: a re-attach mints a newer fence"
    );

    let item = backend.get_work_item(id).await.unwrap().expect("item");
    assert_eq!(item.worker_id.as_deref(), Some("w1"), "{name}");
    assert_eq!(
        item.attempt, 0,
        "{name}: a re-attached lease is the same attempt, not a retry"
    );

    let settled = backend
        .complete_work_item_fenced(id, renewal.lease_fence)
        .await
        .unwrap();
    assert!(
        settled,
        "{name}: completion with the re-attached fence must succeed"
    );
}

pub async fn a_heartbeat_does_not_reattach_once_another_worker_holds_the_item(fx: &Fixture) {
    let (name, backend) = backend(fx).await;
    let (_exec, id) = seed(backend.as_ref(), "gpu").await;
    let first = backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");
    fx.hooks.force_lease_expired(id).await;
    let taken = backend
        .claim_work_item("w2", &["gpu"])
        .await
        .unwrap()
        .expect("reclaimable");
    assert_eq!(taken.worker_id.as_deref(), Some("w2"));

    let err = backend
        .heartbeat_lease(id, "w1", first.lease_fence)
        .await
        .expect_err(&format!(
            "{name}: a stale worker must not steal the item back"
        ));
    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "{name}: got {err}"
    );
}

/// How the worker that took the item over gives it back without settling it.
#[derive(Debug, Clone, Copy)]
enum Release {
    /// A retryable failure report: back to pending with a backoff.
    Fail,
    /// A rate-limit park: back to pending with a not-before time.
    Park,
}

/// C1 (PR #140 review): w1 loses its lease to expiry, w2 claims the item and
/// then hands it back (a retryable failure, or a park). The item is pending
/// and unclaimed again, but somebody DID claim it since w1's expiry, so w1's
/// late heartbeat must not take it back. If it could, it would skip w2's
/// backoff or park window and get the attempt w2 spent refunded.
pub async fn a_heartbeat_does_not_reattach_after_another_worker_claimed_and_released(fx: &Fixture) {
    for release in [Release::Fail, Release::Park] {
        // One store for every round: each round gets a queue of its own.
        let queue = format!("gpu-{release:?}");
        let (name, backend) = backend(fx).await;
        let (_exec, id) = seed(backend.as_ref(), &queue).await;
        let first = backend
            .claim_work_item("w1", &[queue.as_str()])
            .await
            .unwrap()
            .expect("claimable");
        fx.hooks.force_lease_expired(id).await;
        let taken = backend
            .claim_work_item("w2", &[queue.as_str()])
            .await
            .unwrap()
            .expect("reclaimable");
        assert_eq!(taken.worker_id.as_deref(), Some("w2"), "{name}");
        match release {
            Release::Fail => {
                let out = backend
                    .fail_work_item_fenced(id, taken.lease_fence, "boom")
                    .await
                    .unwrap();
                assert!(out.is_some(), "{name}: w2's fenced fail must land");
            }
            Release::Park => {
                let parked = backend
                    .park_work_item(id, taken.lease_fence, "2020-01-01T00:00:00+00:00", 2)
                    .await
                    .unwrap();
                assert!(parked, "{name}: w2's park must land");
            }
        }
        let before = backend.get_work_item(id).await.unwrap().expect("item");

        let res = backend.heartbeat_lease(id, "w1", first.lease_fence).await;

        let after = backend.get_work_item(id).await.unwrap().expect("item");
        assert!(
            matches!(res, Err(StateBackendError::FenceLost(_))),
            "{name} ({release:?}): w1 re-attached after w2 claimed and released: {res:?}"
        );
        assert_eq!(
            after.attempt, before.attempt,
            "{name} ({release:?}): the attempt w2 spent must not be refunded"
        );
        assert!(after.worker_id.is_none(), "{name} ({release:?})");
    }
}

/// The worker that lost the lease, presenting a fence it never held, must not
/// re-attach. One wrong fence differs in the epoch; the other carries the
/// right epoch under another store term, so only the fence match refuses it.
pub async fn a_heartbeat_with_the_wrong_fence_does_not_reattach(fx: &Fixture) {
    for offset in [12_345_i64, 1_i64 << 32] {
        // One store for every round: each round gets a queue of its own.
        let queue = format!("gpu-{offset}");
        let (name, backend) = backend(fx).await;
        let (_exec, id) = seed(backend.as_ref(), &queue).await;
        let first = backend
            .claim_work_item("w1", &[queue.as_str()])
            .await
            .unwrap()
            .expect("claimable");
        fx.hooks.force_lease_expired(id).await;
        assert!(backend
            .claim_work_item("other", &["cpu"])
            .await
            .unwrap()
            .is_none());

        let res = backend
            .heartbeat_lease(id, "w1", first.lease_fence + offset)
            .await;
        assert!(
            matches!(res, Err(StateBackendError::FenceLost(_))),
            "{name}: a wrong fence (+{offset}) re-attached: {res:?}"
        );
    }
}

/// Another worker presenting the right fence must not re-attach either.
pub async fn a_heartbeat_from_another_worker_does_not_reattach(fx: &Fixture) {
    let (name, backend) = backend(fx).await;
    let (_exec, id) = seed(backend.as_ref(), "gpu").await;
    let first = backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");
    fx.hooks.force_lease_expired(id).await;
    assert!(backend
        .claim_work_item("other", &["cpu"])
        .await
        .unwrap()
        .is_none());

    let res = backend.heartbeat_lease(id, "w2", first.lease_fence).await;
    assert!(
        matches!(res, Err(StateBackendError::FenceLost(_))),
        "{name}: another worker re-attached: {res:?}"
    );
}

/// M1 (PR #140 review): the backend bounds a requested lease itself instead of
/// trusting every caller to. `u64::MAX` used to wrap to -1 s, an already
/// expired lease, and a large value could panic inside chrono.
pub async fn a_claimed_lease_is_bounded_by_the_backend(fx: &Fixture) {
    use jamjet_state::{MAX_LEASE_SECS, MIN_LEASE_SECS};
    for (asked, lo, hi) in [
        (0_u64, MIN_LEASE_SECS - 1, MIN_LEASE_SECS),
        (1, MIN_LEASE_SECS - 1, MIN_LEASE_SECS),
        (99_999, MAX_LEASE_SECS - 2, MAX_LEASE_SECS),
        (u64::MAX, MAX_LEASE_SECS - 2, MAX_LEASE_SECS),
    ] {
        // One store for every round: each round gets a queue of its own.
        let queue = format!("gpu-{asked}");
        let (name, backend) = backend(fx).await;
        let (_exec, _id) = seed(backend.as_ref(), &queue).await;
        let claimed = backend
            .claim_work_item_with_lease("w", &[queue.as_str()], Some(asked))
            .await
            .unwrap()
            .expect("claimable");
        let secs = (claimed.lease_expires_at.expect("lease set") - Utc::now()).num_seconds();
        assert!(
            (lo as i64..=hi as i64).contains(&secs),
            "{name}: asked for {asked} s, got a {secs} s lease, expected {lo}..={hi}"
        );
    }
}

fn node_completed() -> EventKind {
    EventKind::NodeCompleted {
        node_id: "gpu_step".into(),
        output: json!({"ok": true}),
        state_patch: json!({"gpu_result": "late"}),
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

/// I1 and I3 (PR #140 review) at the backend: a commit into a terminal run
/// records nothing and withdraws the holder's item, and a fence that is not
/// the holder's touches nothing, terminal run or not.
pub async fn a_commit_into_a_terminal_run_records_nothing_and_honours_the_fence(fx: &Fixture) {
    use jamjet_state::{Event, TurnCommit};
    let (name, backend) = backend(fx).await;
    let (exec, id) = seed(backend.as_ref(), "gpu").await;
    let held = backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");
    backend
        .update_execution_status(&exec, WorkflowStatus::Cancelled)
        .await
        .unwrap();

    let forged = backend
        .commit_turn_unless_terminal(
            id,
            held.lease_fence + 999,
            Event::new(exec.clone(), 0, node_completed()),
            true,
        )
        .await;
    assert!(
        matches!(forged, Err(StateBackendError::FenceLost(_))),
        "{name}: a forged fence must be refused before anything else, got {forged:?}"
    );
    let still = backend.get_work_item(id).await.unwrap().expect("item");
    assert_eq!(
        still.worker_id.as_deref(),
        Some("w1"),
        "{name}: a forged fence withdrew the holder's item"
    );

    let outcome = backend
        .commit_turn_unless_terminal(
            id,
            held.lease_fence,
            Event::new(exec.clone(), 0, node_completed()),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        TurnCommit::ExecutionTerminal(WorkflowStatus::Cancelled),
        "{name}"
    );
    let events = backend.get_events(&exec).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. })),
        "{name}: a completion was recorded into a cancelled run"
    );
    assert!(
        !backend
            .withdraw_work_item_fenced(id, held.lease_fence)
            .await
            .unwrap(),
        "{name}: the item must already be withdrawn"
    );
    assert!(
        backend
            .claim_work_item("w2", &["gpu"])
            .await
            .unwrap()
            .is_none(),
        "{name}: a withdrawn item must not be claimable"
    );
}

/// I4 (PR #140 review): a lease that lapses on a run that is already terminal
/// is not a retry. The fast reclaim used to re-queue the item and append
/// `NodeFailed` + `RetryScheduled` to the cancelled run, against "nothing is
/// recorded against a terminal run". It now settles the item and writes
/// nothing.
pub async fn a_lease_lapsing_on_a_terminal_run_records_no_retry_and_settles_the_item(fx: &Fixture) {
    let (name, backend) = backend(fx).await;
    let (exec, id) = seed(backend.as_ref(), "gpu").await;
    backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");
    backend
        .update_execution_status(&exec, WorkflowStatus::Cancelled)
        .await
        .unwrap();
    fx.hooks.force_lease_expired(id).await;

    let next = backend.claim_work_item("w2", &["gpu"]).await.unwrap();

    assert!(
        next.is_none(),
        "{name}: the cancelled run's item was handed out again: {next:?}"
    );
    let events = backend.get_events(&exec).await.unwrap();
    assert!(
        !events.iter().any(|e| matches!(
            e.kind,
            EventKind::NodeFailed { .. } | EventKind::RetryScheduled { .. }
        )),
        "{name}: a retry was recorded against a cancelled run: {events:?}"
    );
}

/// I-A (PR #140 re-review) at the backend: ending a run sets the status and
/// appends the terminal event in one step, a fenced commit after it is
/// refused, and a second termination records nothing.
pub async fn finishing_a_run_writes_status_and_event_together_and_only_once(fx: &Fixture) {
    use jamjet_state::{Event, ExecutionFinish, TurnCommit};
    let (name, backend) = backend(fx).await;
    let (exec, id) = seed(backend.as_ref(), "gpu").await;
    let held = backend
        .claim_work_item("w1", &["gpu"])
        .await
        .unwrap()
        .expect("claimable");

    let not_terminal = backend
        .finish_execution(
            Event::new(
                exec.clone(),
                0,
                EventKind::WorkflowCancelled { reason: None },
            ),
            WorkflowStatus::Running,
        )
        .await;
    assert!(
        not_terminal.is_err(),
        "{name}: a non-terminal status must be refused, got {not_terminal:?}"
    );
    assert!(
        backend.get_events(&exec).await.unwrap().is_empty(),
        "{name}: a refused finish wrote an event"
    );

    let finished = backend
        .finish_execution(
            Event::new(
                exec.clone(),
                0,
                EventKind::WorkflowCancelled { reason: None },
            ),
            WorkflowStatus::Cancelled,
        )
        .await
        .unwrap();
    assert!(
        matches!(finished, ExecutionFinish::Finished(_)),
        "{name}: {finished:?}"
    );
    let status = backend.get_execution(&exec).await.unwrap().unwrap().status;
    assert_eq!(status, WorkflowStatus::Cancelled, "{name}");

    let again = backend
        .finish_execution(
            Event::new(
                exec.clone(),
                0,
                EventKind::WorkflowCompleted {
                    final_state: json!({}),
                },
            ),
            WorkflowStatus::Completed,
        )
        .await
        .unwrap();
    assert_eq!(
        again,
        ExecutionFinish::AlreadyTerminal(WorkflowStatus::Cancelled),
        "{name}: a second termination must not overwrite the first"
    );

    let late = backend
        .commit_turn_unless_terminal(
            id,
            held.lease_fence,
            Event::new(exec.clone(), 0, node_completed()),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        late,
        TurnCommit::ExecutionTerminal(WorkflowStatus::Cancelled),
        "{name}"
    );

    let kinds: Vec<String> = backend
        .get_events(&exec)
        .await
        .unwrap()
        .iter()
        .map(|e| serde_json::to_value(&e.kind).unwrap()["type"].to_string())
        .collect();
    assert_eq!(kinds, vec!["\"workflow_cancelled\""], "{name}");
    let status = backend.get_execution(&exec).await.unwrap().unwrap().status;
    assert_eq!(status, WorkflowStatus::Cancelled, "{name}");

    let missing = backend
        .finish_execution(
            Event::new(
                ExecutionId::new(),
                0,
                EventKind::WorkflowCancelled { reason: None },
            ),
            WorkflowStatus::Cancelled,
        )
        .await;
    assert!(
        matches!(missing, Err(StateBackendError::NotFound(_))),
        "{name}: {missing:?}"
    );
}
