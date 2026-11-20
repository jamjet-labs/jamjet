//! `reclaim_expired_leases_recorded` and `fail_work_item_fenced_recorded`:
//! the scheduler's lease sweep and a worker's failure report, each with the
//! events that record it written in the same step as the settle.
//!
//! The trails are the ones the scheduler and `POST /work-items/:id/fail`
//! appended before, spelled out here as literals. An item whose run is
//! already over is settled with nothing recorded. A scoped store touches
//! only its own tenant's items.

use crate::conformance::cases::support::{finding, in_tenant, tenant_view, without_tenant_views};
use crate::conformance::{Fixture, StoreKind};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{EventKind, FailOutcome, StateBackend, WorkItem, DEFAULT_TENANT};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const QUEUE: &str = "general";

/// The backend under test. The scoped stores run in a tenant other than the
/// default.
async fn backend(fx: &Fixture) -> (String, Arc<dyn StateBackend>) {
    (format!("{:?}", fx.kind), in_tenant(fx, "acme").await)
}

/// `"pending"` or `"claimed"` while the item is live, then its stored
/// status. Memory drops a settled item, which reads as `"settled"`.
async fn item_status(fx: &Fixture, id: Uuid) -> String {
    match fx.hooks.item_row(id).await {
        Some(row) => row.status,
        None => "settled".into(),
    }
}

/// The item's dead-letter rows.
async fn dead_letters(fx: &Fixture, id: Uuid) -> i64 {
    i64::from(fx.hooks.dead_letter_execution(id).await.is_some())
}

/// What an item of a finished run reads as once it is settled: `failed`,
/// or gone on memory.
fn settled_failed(fx: &Fixture) -> &'static str {
    if fx.kind == StoreKind::Memory {
        "settled"
    } else {
        "failed"
    }
}

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
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
    }
}

/// A started run whose node `a` is claimed at `attempt` of `max_attempts`.
/// Returns the run, the claimed item and the log's latest sequence.
async fn start_claimed(
    b: &dyn StateBackend,
    attempt: u32,
    max_attempts: u32,
) -> (ExecutionId, WorkItem, i64) {
    let id = ExecutionId::new();
    // A queue of the run's own: a case may start several runs in one store,
    // and each claim must take its own run's item.
    let queue = format!("{QUEUE}-{}", id.0.simple());
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: "a".into(),
        queue_type: queue.clone(),
        payload: json!({ "node_id": "a" }),
        attempt,
        max_attempts,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    };
    b.start_execution_atomic(
        run(&id),
        EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({}),
            initiated_by: None,
            run_key: None,
        },
        EventKind::NodeScheduled {
            node_id: "a".into(),
            queue_type: queue.clone(),
        },
        item,
    )
    .await
    .unwrap();
    let held = b
        .claim_work_item("w1", &[queue.as_str()])
        .await
        .unwrap()
        .expect("the run's item is claimable");
    let seen = b.latest_sequence(&id).await.unwrap();
    (id, held, seen)
}

/// The events after `seen`, as JSON, with their sequences.
async fn trail(b: &dyn StateBackend, id: &ExecutionId, seen: i64) -> (Vec<i64>, serde_json::Value) {
    let tail = b.get_events_since(id, seen).await.unwrap();
    let sequences = tail.iter().map(|e| e.sequence).collect();
    let kinds: Vec<&EventKind> = tail.iter().map(|e| &e.kind).collect();
    (sequences, serde_json::to_value(kinds).unwrap())
}

fn as_json(kinds: Vec<EventKind>) -> serde_json::Value {
    serde_json::to_value(kinds).unwrap()
}

/// (id, attempt) of each item.
fn attempts(items: &[WorkItem]) -> Vec<(Uuid, u32)> {
    items.iter().map(|i| (i.id, i.attempt)).collect()
}

/// What the scheduler's sweep appended for a lapsed lease before the
/// backend recorded it, written out by hand.
pub async fn reclaim_events_match_the_old_trail(fx: &Fixture) {
    // Requeued: attempt 1 of 3 lapses, attempt 2 follows after 2^2 s.
    {
        let (name, b) = backend(fx).await;
        let (id, held, seen) = start_claimed(&*b, 1, 3).await;
        fx.hooks.force_lease_expired(held.id).await;

        let out = b.reclaim_expired_leases_recorded().await.unwrap();
        assert_eq!(attempts(&out.retryable), vec![(held.id, 2)], "{name}");
        assert!(out.exhausted.is_empty(), "{name}: {:?}", out.exhausted);
        let (sequences, kinds) = trail(&*b, &id, seen).await;
        assert_eq!(
            kinds,
            as_json(vec![
                EventKind::NodeFailed {
                    node_id: "a".into(),
                    error: "lease expired: worker presumed dead".into(),
                    attempt: 1,
                    retryable: true,
                },
                EventKind::RetryScheduled {
                    node_id: "a".into(),
                    attempt: 2,
                    delay_ms: 4000,
                },
            ]),
            "{name}"
        );
        assert_eq!(sequences, vec![seen + 1, seen + 2], "{name}");
        assert_eq!(item_status(fx, held.id).await, "pending", "{name}");
        assert_eq!(dead_letters(fx, held.id).await, 0, "{name}");

        // Nothing is left to reclaim, and nothing is recorded twice.
        let again = b.reclaim_expired_leases_recorded().await.unwrap();
        assert!(
            again.retryable.is_empty() && again.exhausted.is_empty(),
            "{name}"
        );
        assert_eq!(b.latest_sequence(&id).await.unwrap(), seen + 2, "{name}");
    }

    // Dead-lettered: the last of 3 attempts lapses.
    {
        let (name, b) = backend(fx).await;
        let (id, held, seen) = start_claimed(&*b, 2, 3).await;
        fx.hooks.force_lease_expired(held.id).await;

        let out = b.reclaim_expired_leases_recorded().await.unwrap();
        assert_eq!(attempts(&out.exhausted), vec![(held.id, 3)], "{name}");
        assert!(out.retryable.is_empty(), "{name}: {:?}", out.retryable);
        let (sequences, kinds) = trail(&*b, &id, seen).await;
        assert_eq!(
            kinds,
            as_json(vec![EventKind::NodeFailed {
                node_id: "a".into(),
                error: "exhausted 3 attempts: lease expired".into(),
                attempt: 3,
                retryable: false,
            }]),
            "{name}"
        );
        assert_eq!(sequences, vec![seen + 1], "{name}");
        assert_eq!(item_status(fx, held.id).await, "dead_lettered", "{name}");
        assert_eq!(
            dead_letters(fx, held.id).await,
            1,
            "{name}: one dead-letter row"
        );
        assert_eq!(
            fx.hooks.dead_letter_tenant(held.id).await,
            Some(held.tenant_id.clone()),
            "{name}: the row carries the item's tenant"
        );

        // Nothing is left to reclaim, and nothing is recorded twice.
        let again = b.reclaim_expired_leases_recorded().await.unwrap();
        assert!(
            again.retryable.is_empty() && again.exhausted.is_empty(),
            "{name}"
        );
        assert_eq!(b.latest_sequence(&id).await.unwrap(), seen + 1, "{name}");
    }
}

/// The sweep used to append its failure trail to a run that had already
/// finished. Now the item of a finished run is settled failed, nothing is
/// recorded, and the item is not in the result.
pub async fn reclaim_on_a_finished_run_records_nothing(fx: &Fixture) {
    // A retryable item and an item with no attempt left.
    for (attempt, max_attempts) in [(0, 3), (0, 1)] {
        {
            let (name, b) = backend(fx).await;
            let (id, held, seen) = start_claimed(&*b, attempt, max_attempts).await;
            b.update_execution_status(&id, WorkflowStatus::Cancelled)
                .await
                .unwrap();
            fx.hooks.force_lease_expired(held.id).await;

            let out = b.reclaim_expired_leases_recorded().await.unwrap();
            assert!(
                out.retryable.is_empty() && out.exhausted.is_empty(),
                "{name} ({attempt} of {max_attempts}): nothing reclaimed; {out:?}"
            );
            let (_, kinds) = trail(&*b, &id, seen).await;
            assert_eq!(
                kinds,
                json!([]),
                "{name} ({attempt} of {max_attempts}): nothing recorded"
            );
            let settled = settled_failed(fx);
            assert_eq!(
                item_status(fx, held.id).await,
                settled,
                "{name} ({attempt} of {max_attempts})"
            );
            assert_eq!(
                dead_letters(fx, held.id).await,
                0,
                "{name} ({attempt} of {max_attempts}): no dead-letter row"
            );
        }
    }
}

/// What the fail route appended for a worker's failure report before the
/// backend recorded it, written out by hand.
pub async fn a_recorded_failure_writes_the_fail_routes_trail(fx: &Fixture) {
    {
        let (name, b) = backend(fx).await;
        let (id, held, seen) = start_claimed(&*b, 0, 3).await;

        let out = b
            .fail_work_item_fenced_recorded(held.id, held.lease_fence, "tool raised ValueError")
            .await
            .unwrap();
        assert!(
            matches!(&out, Some(FailOutcome::Retryable { item, delay_ms: 2000 }) if item.attempt == 1),
            "{name}: {out:?}"
        );
        let (sequences, kinds) = trail(&*b, &id, seen).await;
        assert_eq!(
            kinds,
            as_json(vec![
                EventKind::NodeFailed {
                    node_id: "a".into(),
                    error: "tool raised ValueError".into(),
                    attempt: 0,
                    retryable: true,
                },
                EventKind::RetryScheduled {
                    node_id: "a".into(),
                    attempt: 1,
                    delay_ms: 2000,
                },
            ]),
            "{name}"
        );
        assert_eq!(sequences, vec![seen + 1, seen + 2], "{name}");
        assert_eq!(item_status(fx, held.id).await, "pending", "{name}");
    }

    {
        let (name, b) = backend(fx).await;
        let (id, held, seen) = start_claimed(&*b, 2, 3).await;

        let out = b
            .fail_work_item_fenced_recorded(held.id, held.lease_fence, "still broken")
            .await
            .unwrap();
        assert!(
            matches!(&out, Some(FailOutcome::Exhausted { item }) if item.attempt == 3),
            "{name}: {out:?}"
        );
        let (_, kinds) = trail(&*b, &id, seen).await;
        assert_eq!(
            kinds,
            as_json(vec![EventKind::NodeFailed {
                node_id: "a".into(),
                error: "still broken".into(),
                attempt: 3,
                retryable: false,
            }]),
            "{name}"
        );
        if !finding(
            fx,
            &[StoreKind::Memory],
            "a fenced failure on the last attempt keeps no dead-letter copy",
        ) {
            assert_eq!(
                dead_letters(fx, held.id).await,
                1,
                "{name}: one dead-letter row"
            );
        }

        // A replay under the same fence finds nothing of its own.
        let replay = b
            .fail_work_item_fenced_recorded(held.id, held.lease_fence, "still broken")
            .await
            .unwrap();
        assert!(replay.is_none(), "{name}: {replay:?}");
        assert_eq!(b.latest_sequence(&id).await.unwrap(), seen + 1, "{name}");
    }
}

/// A failure report for an item whose run was cancelled after the caller
/// read it: the item is settled, nothing is recorded, and the answer is
/// `None`, which the fail route turns into a 409.
pub async fn a_recorded_failure_on_a_finished_run_records_nothing(fx: &Fixture) {
    for (attempt, max_attempts) in [(0, 3), (2, 3)] {
        {
            let (name, b) = backend(fx).await;
            let (id, held, seen) = start_claimed(&*b, attempt, max_attempts).await;
            b.update_execution_status(&id, WorkflowStatus::Cancelled)
                .await
                .unwrap();

            let out = b
                .fail_work_item_fenced_recorded(held.id, held.lease_fence, "boom")
                .await
                .unwrap();
            assert!(
                out.is_none(),
                "{name} ({attempt} of {max_attempts}): {out:?}"
            );
            let (_, kinds) = trail(&*b, &id, seen).await;
            assert_eq!(kinds, json!([]), "{name} ({attempt} of {max_attempts})");
            let settled = settled_failed(fx);
            assert_eq!(
                item_status(fx, held.id).await,
                settled,
                "{name} ({attempt} of {max_attempts})"
            );
            assert_eq!(
                dead_letters(fx, held.id).await,
                0,
                "{name} ({attempt} of {max_attempts})"
            );
        }
    }
}

/// A fence that is not the item's writes nothing, the item's run going on.
pub async fn a_recorded_failure_with_a_stale_fence_writes_nothing(fx: &Fixture) {
    {
        let (name, b) = backend(fx).await;
        let (id, held, seen) = start_claimed(&*b, 2, 3).await;

        let out = b
            .fail_work_item_fenced_recorded(held.id, held.lease_fence + 1, "zombie")
            .await
            .unwrap();
        assert!(out.is_none(), "{name}: {out:?}");
        assert_eq!(b.latest_sequence(&id).await.unwrap(), seen, "{name}");
        assert_eq!(item_status(fx, held.id).await, "claimed", "{name}");
        assert_eq!(dead_letters(fx, held.id).await, 0, "{name}");
    }
}

/// A store scoped to one tenant neither reclaims nor fails another
/// tenant's item, and records nothing in its run.
pub async fn a_scoped_store_reclaims_and_fails_only_its_own_tenants_items(fx: &Fixture) {
    let (Some(acme), Some(other)) = (
        tenant_view(fx, "acme").await,
        tenant_view(fx, "other").await,
    ) else {
        return without_tenant_views(fx);
    };
    let base = fx.hooks.unscoped();
    let (id, held, seen) = start_claimed(&*other, 0, 3).await;
    fx.hooks.force_lease_expired(held.id).await;

    let out = acme.reclaim_expired_leases_recorded().await.unwrap();
    assert!(
        out.retryable.is_empty() && out.exhausted.is_empty(),
        "{out:?}"
    );
    let failed = acme
        .fail_work_item_fenced_recorded(held.id, held.lease_fence, "not yours")
        .await
        .unwrap();
    assert!(failed.is_none(), "{failed:?}");
    assert_eq!(base.latest_sequence(&id).await.unwrap(), seen);
    assert_eq!(
        item_status(fx, held.id).await,
        "claimed",
        "the other tenant's item is untouched"
    );

    // Its own tenant's sweep reclaims it.
    let out = other.reclaim_expired_leases_recorded().await.unwrap();
    assert_eq!(attempts(&out.retryable), vec![(held.id, 1)]);
}
