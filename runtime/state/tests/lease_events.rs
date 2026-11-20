//! `reclaim_expired_leases_recorded` and `fail_work_item_fenced_recorded`:
//! the scheduler's lease sweep and a worker's failure report, each with the
//! events that record it written in the same step as the settle.
//!
//! The trails are the ones the scheduler and `POST /work-items/:id/fail`
//! appended before, spelled out here as literals. An item whose run is
//! already over is settled with nothing recorded. A scoped store touches
//! only its own tenant's items.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    EventKind, FailOutcome, InMemoryBackend, SqliteBackend, StateBackend, Tenant, TenantId,
    TenantScopedSqliteBackend, TenantStatus, WorkItem, DEFAULT_TENANT,
};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const QUEUE: &str = "general";

fn sqlite_url() -> String {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "jamjet_lease_events_{}.db",
        Uuid::new_v4().simple()
    ));
    format!("sqlite://{}", p.display())
}

/// A view of `base` scoped to `tenant`, with the tenant row created (SQLite
/// refuses an unknown tenant on every tenant column).
async fn scoped_to(base: &SqliteBackend, tenant: &str) -> TenantScopedSqliteBackend {
    let id = TenantId::from(tenant);
    let scoped = base.for_tenant(id.clone());
    if scoped.get_tenant(&id).await.unwrap().is_none() {
        let now = chrono::Utc::now();
        scoped
            .create_tenant(Tenant {
                id,
                name: tenant.into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }
    scoped
}

/// A backend under test, with what the trait does not have: backdating a
/// lease and reading where an item stands.
enum Store {
    Memory(Arc<InMemoryBackend>),
    Sqlite(Arc<SqliteBackend>),
    /// Scoped to `acme`; `base` is the unscoped backend on the same file.
    Scoped {
        base: Arc<SqliteBackend>,
        scoped: Arc<TenantScopedSqliteBackend>,
    },
}

impl Store {
    fn name(&self) -> &'static str {
        match self {
            Store::Memory(_) => "memory",
            Store::Sqlite(_) => "sqlite",
            Store::Scoped { .. } => "tenant-scoped sqlite",
        }
    }

    fn backend(&self) -> Arc<dyn StateBackend> {
        match self {
            Store::Memory(b) => b.clone(),
            Store::Sqlite(b) => b.clone(),
            Store::Scoped { scoped, .. } => scoped.clone(),
        }
    }

    fn sqlite(&self) -> Option<&SqliteBackend> {
        match self {
            Store::Memory(_) => None,
            Store::Sqlite(b) => Some(b),
            Store::Scoped { base, .. } => Some(base),
        }
    }

    async fn expire(&self, id: Uuid) {
        match self {
            Store::Memory(b) => b.force_lease_expired_for_test(id),
            _ => self
                .sqlite()
                .unwrap()
                .force_lease_expired_for_test(id)
                .await
                .unwrap(),
        }
    }

    /// `"pending"` or `"claimed"` while the item is live, then its stored
    /// status. Memory drops a settled item, which reads as `"settled"`.
    async fn item_status(&self, id: Uuid) -> String {
        match self.sqlite() {
            None => match self.backend().get_work_item(id).await.unwrap() {
                Some(item) if item.worker_id.is_some() => "claimed".into(),
                Some(_) => "pending".into(),
                None => "settled".into(),
            },
            Some(b) => sqlx::query_scalar("SELECT status FROM work_items WHERE id = ?")
                .bind(id.to_string())
                .fetch_one(&b.pool())
                .await
                .unwrap(),
        }
    }

    /// The item's dead-letter rows. Memory keeps no readable dead-letter
    /// queue: `None`.
    async fn dead_letters(&self, id: Uuid) -> Option<i64> {
        let b = self.sqlite()?;
        Some(
            sqlx::query_scalar("SELECT COUNT(*) FROM dead_letter_items WHERE id = ?")
                .bind(id.to_string())
                .fetch_one(&b.pool())
                .await
                .unwrap(),
        )
    }

    /// The tenant the item's dead-letter row carries, on SQLite.
    async fn dead_letter_tenant(&self, id: Uuid) -> Option<String> {
        let b = self.sqlite()?;
        sqlx::query_scalar("SELECT tenant_id FROM dead_letter_items WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&b.pool())
            .await
            .unwrap()
    }
}

/// One fresh store per backend. Each case starts one run per store, so the
/// in-memory backend's sequences (one counter for every run) are the run's
/// own, and a claim takes that run's item.
async fn stores() -> Vec<Store> {
    let base = Arc::new(SqliteBackend::open(&sqlite_url()).await.unwrap());
    let scoped = Arc::new(scoped_to(&base, "acme").await);
    vec![
        Store::Memory(Arc::new(InMemoryBackend::new())),
        Store::Sqlite(Arc::new(SqliteBackend::open(&sqlite_url()).await.unwrap())),
        Store::Scoped { base, scoped },
    ]
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
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: "a".into(),
        queue_type: QUEUE.into(),
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
            queue_type: QUEUE.into(),
        },
        item,
    )
    .await
    .unwrap();
    let held = b
        .claim_work_item("w1", &[QUEUE])
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
#[tokio::test]
async fn reclaim_events_match_the_old_trail() {
    // Requeued: attempt 1 of 3 lapses, attempt 2 follows after 2^2 s.
    for store in stores().await {
        let name = store.name();
        let b = store.backend();
        let (id, held, seen) = start_claimed(&*b, 1, 3).await;
        store.expire(held.id).await;

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
        assert_eq!(store.item_status(held.id).await, "pending", "{name}");
        assert_eq!(store.dead_letters(held.id).await.unwrap_or(0), 0, "{name}");

        // Nothing is left to reclaim, and nothing is recorded twice.
        let again = b.reclaim_expired_leases_recorded().await.unwrap();
        assert!(
            again.retryable.is_empty() && again.exhausted.is_empty(),
            "{name}"
        );
        assert_eq!(b.latest_sequence(&id).await.unwrap(), seen + 2, "{name}");
    }

    // Dead-lettered: the last of 3 attempts lapses.
    for store in stores().await {
        let name = store.name();
        let b = store.backend();
        let (id, held, seen) = start_claimed(&*b, 2, 3).await;
        store.expire(held.id).await;

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
        let settled = if store.sqlite().is_some() {
            "dead_lettered"
        } else {
            "settled"
        };
        assert_eq!(store.item_status(held.id).await, settled, "{name}");
        if let Some(rows) = store.dead_letters(held.id).await {
            assert_eq!(rows, 1, "{name}: one dead-letter row");
            assert_eq!(
                store.dead_letter_tenant(held.id).await,
                Some(held.tenant_id.clone()),
                "{name}: the row carries the item's tenant"
            );
        }

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
#[tokio::test]
async fn reclaim_on_a_finished_run_records_nothing() {
    // A retryable item and an item with no attempt left.
    for (attempt, max_attempts) in [(0, 3), (0, 1)] {
        for store in stores().await {
            let name = store.name();
            let b = store.backend();
            let (id, held, seen) = start_claimed(&*b, attempt, max_attempts).await;
            b.update_execution_status(&id, WorkflowStatus::Cancelled)
                .await
                .unwrap();
            store.expire(held.id).await;

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
            let settled = if store.sqlite().is_some() {
                "failed"
            } else {
                "settled"
            };
            assert_eq!(
                store.item_status(held.id).await,
                settled,
                "{name} ({attempt} of {max_attempts})"
            );
            assert_eq!(
                store.dead_letters(held.id).await.unwrap_or(0),
                0,
                "{name} ({attempt} of {max_attempts}): no dead-letter row"
            );
        }
    }
}

/// What the fail route appended for a worker's failure report before the
/// backend recorded it, written out by hand.
#[tokio::test]
async fn a_recorded_failure_writes_the_fail_routes_trail() {
    for store in stores().await {
        let name = store.name();
        let b = store.backend();
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
        assert_eq!(store.item_status(held.id).await, "pending", "{name}");
    }

    for store in stores().await {
        let name = store.name();
        let b = store.backend();
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
        if let Some(rows) = store.dead_letters(held.id).await {
            assert_eq!(rows, 1, "{name}: one dead-letter row");
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
#[tokio::test]
async fn a_recorded_failure_on_a_finished_run_records_nothing() {
    for (attempt, max_attempts) in [(0, 3), (2, 3)] {
        for store in stores().await {
            let name = store.name();
            let b = store.backend();
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
            let settled = if store.sqlite().is_some() {
                "failed"
            } else {
                "settled"
            };
            assert_eq!(
                store.item_status(held.id).await,
                settled,
                "{name} ({attempt} of {max_attempts})"
            );
            assert_eq!(
                store.dead_letters(held.id).await.unwrap_or(0),
                0,
                "{name} ({attempt} of {max_attempts})"
            );
        }
    }
}

/// A fence that is not the item's writes nothing, the item's run going on.
#[tokio::test]
async fn a_recorded_failure_with_a_stale_fence_writes_nothing() {
    for store in stores().await {
        let name = store.name();
        let b = store.backend();
        let (id, held, seen) = start_claimed(&*b, 2, 3).await;

        let out = b
            .fail_work_item_fenced_recorded(held.id, held.lease_fence + 1, "zombie")
            .await
            .unwrap();
        assert!(out.is_none(), "{name}: {out:?}");
        assert_eq!(b.latest_sequence(&id).await.unwrap(), seen, "{name}");
        assert_eq!(store.item_status(held.id).await, "claimed", "{name}");
        assert_eq!(store.dead_letters(held.id).await.unwrap_or(0), 0, "{name}");
    }
}

/// A store scoped to one tenant neither reclaims nor fails another
/// tenant's item, and records nothing in its run.
#[tokio::test]
async fn a_scoped_store_reclaims_and_fails_only_its_own_tenants_items() {
    let base = SqliteBackend::open(&sqlite_url()).await.unwrap();
    let acme = scoped_to(&base, "acme").await;
    let other = scoped_to(&base, "other").await;
    let (id, held, seen) = start_claimed(&other, 0, 3).await;
    base.force_lease_expired_for_test(held.id).await.unwrap();

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
    let status: String = sqlx::query_scalar("SELECT status FROM work_items WHERE id = ?")
        .bind(held.id.to_string())
        .fetch_one(&base.pool())
        .await
        .unwrap();
    assert_eq!(status, "claimed", "the other tenant's item is untouched");

    // Its own tenant's sweep reclaims it.
    let out = other.reclaim_expired_leases_recorded().await.unwrap();
    assert_eq!(attempts(&out.retryable), vec![(held.id, 1)]);
}
