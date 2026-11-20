//! A tenant's view of the store reaches only that tenant's rows: its claims
//! never take, reclaim or report another tenant's items, its appends never
//! land in another tenant's run, and its reads of another tenant's rows come
//! back empty. Each case runs `acme` beside `globex` on the stores that have
//! tenant views, and checks the same call on `globex`'s own view, so an
//! empty answer comes from the scope and not from a missing row.

use crate::conformance::cases::support::{finding, tenant_view, without_tenant_views};
use crate::conformance::{Fixture, StoreKind};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    ApprovalProjectionRow, ClaimFilter, Event, EventKind, ExecutionClaim, Snapshot, StateBackend,
    StateBackendError, WorkItem, WorkItemId, DEFAULT_TENANT,
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

/// `acme`'s and `globex`'s views, or `None` (said on stderr) where the
/// store has no tenant views.
async fn two_tenants(fx: &Fixture) -> Option<(Arc<dyn StateBackend>, Arc<dyn StateBackend>)> {
    match (
        tenant_view(fx, "acme").await,
        tenant_view(fx, "globex").await,
    ) {
        (Some(acme), Some(globex)) => Some((acme, globex)),
        _ => {
            without_tenant_views(fx);
            None
        }
    }
}

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-tenancy".into(),
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

async fn new_run(b: &dyn StateBackend) -> ExecutionId {
    let id = ExecutionId::new();
    b.create_execution(run(&id)).await.unwrap();
    id
}

/// A pending item of the run on `queue`, through the run's tenant's view.
async fn enqueue(b: &dyn StateBackend, id: &ExecutionId, node: &str, queue: &str) -> WorkItemId {
    b.enqueue_work_item(WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: node.into(),
        queue_type: queue.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    })
    .await
    .unwrap()
}

/// Claim on `queue` until nothing is left. More claims than items fails.
async fn claim_all(b: &dyn StateBackend, worker: &str, queue: &str) -> BTreeSet<WorkItemId> {
    let mut claimed = BTreeSet::new();
    for _ in 0..10 {
        match b.claim_work_item(worker, &[queue]).await.unwrap() {
            Some(item) => assert!(claimed.insert(item.id), "{} claimed twice", item.id),
            None => return claimed,
        }
    }
    panic!("the claims did not run dry: {claimed:?}");
}

/// `globex`'s items are older, so a claim that ignored the tenant would take
/// them first. `acme`'s claims, repeated until they get nothing, take
/// exactly `acme`'s items, and `globex`'s stay pending for `globex`.
pub async fn scoped_claim_never_takes_another_tenants_item(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let mut theirs = BTreeSet::new();
    let mut ours = BTreeSet::new();
    let (globex_run, acme_run) = (new_run(&*globex).await, new_run(&*acme).await);
    for n in 0..3 {
        theirs.insert(enqueue(&*globex, &globex_run, &format!("g{n}"), "shared").await);
    }
    for n in 0..3 {
        ours.insert(enqueue(&*acme, &acme_run, &format!("a{n}"), "shared").await);
    }

    assert_eq!(claim_all(&*acme, "acme-worker", "shared").await, ours);
    for item in &theirs {
        let row = fx.hooks.item_row(*item).await.expect("globex's item");
        assert_eq!(row.status, "pending", "globex's item {item}");
    }
    assert_eq!(claim_all(&*globex, "globex-worker", "shared").await, theirs);
}

/// `acme` claims for `globex`'s run, by execution: `Nothing`, while the
/// item is pending and again once `globex`'s worker holds it (not
/// `LeasedToOther`, which would say the run has an item).
pub async fn scoped_claim_for_execution_of_another_tenant_is_nothing(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let item = enqueue(&*globex, &globex_run, "step", "shared").await;
    let filter = ClaimFilter {
        execution_id: Some(globex_run.clone()),
        node_id: None,
    };

    let out = acme
        .claim_for_execution("acme-worker", &["shared"], None, &filter)
        .await
        .unwrap();
    assert!(
        matches!(out, ExecutionClaim::Nothing),
        "acme's claim, pending item: {out:?}"
    );
    let row = fx.hooks.item_row(item).await.expect("globex's item");
    assert_eq!(row.status, "pending");

    let out = globex
        .claim_for_execution("globex-worker", &["shared"], None, &filter)
        .await
        .unwrap();
    assert!(
        matches!(&out, ExecutionClaim::Claimed(w) if w.id == item),
        "globex's own claim: {out:?}"
    );
    let out = acme
        .claim_for_execution("acme-worker", &["shared"], None, &filter)
        .await
        .unwrap();
    assert!(
        matches!(out, ExecutionClaim::Nothing),
        "acme's claim, item leased in globex: {out:?}"
    );
}

/// Both tenants' items are claimed and both leases lapse. `acme`'s next
/// claim reclaims and takes `acme`'s item, and leaves `globex`'s as it
/// was: still claimed by `globex`'s worker under the same lease, with no
/// retry recorded in `globex`'s run.
pub async fn scoped_fast_reclaim_touches_its_own_tenant_only(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let acme_run = new_run(&*acme).await;
    let theirs = enqueue(&*globex, &globex_run, "step", "shared").await;
    let ours = enqueue(&*acme, &acme_run, "step", "shared").await;
    let held_theirs = globex
        .claim_work_item("globex-worker", &["shared"])
        .await
        .unwrap()
        .expect("globex's item");
    assert_eq!(held_theirs.id, theirs);
    let held_ours = acme
        .claim_work_item("acme-worker", &["shared"])
        .await
        .unwrap()
        .expect("acme's item");
    assert_eq!(held_ours.id, ours);
    fx.hooks.force_lease_expired(theirs).await;
    fx.hooks.force_lease_expired(ours).await;
    let before = fx.hooks.item_row(theirs).await.expect("globex's item");

    let again = acme
        .claim_work_item("acme-worker-2", &["shared"])
        .await
        .unwrap()
        .expect("acme's lapsed item is claimable again");
    assert_eq!(again.id, ours, "acme reclaims its own item");
    assert_eq!(again.attempt, 1, "the reclaim counts an attempt");

    let after = fx.hooks.item_row(theirs).await.expect("globex's item");
    assert_eq!(after, before, "globex's item is untouched");
    assert_eq!(after.status, "claimed");
    let unscoped = fx.hooks.unscoped();
    let retries = unscoped
        .get_events(&globex_run)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::NodeFailed { .. } | EventKind::RetryScheduled { .. }
            )
        })
        .count();
    assert_eq!(retries, 0, "no retry recorded in globex's run");
}

/// `acme`'s append to `globex`'s run is `NotFound` and writes nothing;
/// `globex`'s own append lands.
pub async fn scoped_append_to_another_tenants_run_is_not_found(fx: &Fixture) {
    // Scoped SQLite's `append_event` does not check the run's tenant: the
    // event lands in `globex`'s run, stamped `acme`.
    if finding(
        fx,
        &[StoreKind::SqliteScoped],
        "a scoped append_event lands in another tenant's run",
    ) {
        return;
    }
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let unscoped = fx.hooks.unscoped();
    let started = |by: &str| {
        Event::new(
            globex_run.clone(),
            0,
            EventKind::NodeStarted {
                node_id: "step".into(),
                worker_id: by.into(),
                attempt: 0,
            },
        )
    };

    let out = acme.append_event(started("acme-worker")).await;
    assert!(
        matches!(out, Err(StateBackendError::NotFound(_))),
        "acme's append to globex's run: {out:?}"
    );
    assert!(
        unscoped.get_events(&globex_run).await.unwrap().is_empty(),
        "acme's refused append wrote nothing"
    );

    let seq = globex.append_event(started("globex-worker")).await.unwrap();
    assert_eq!(seq, 1, "globex's own append lands");
}

/// `globex`'s run with an event, an item, a snapshot, an artifact and an
/// approval projection row. `acme` reads none of them, and `globex` reads
/// each.
pub async fn scoped_reads_of_another_tenants_rows_are_empty(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let id = new_run(&*globex).await;
    let seq = globex
        .append_event(Event::new(
            id.clone(),
            0,
            EventKind::NodeScheduled {
                node_id: "step".into(),
                queue_type: "shared".into(),
            },
        ))
        .await
        .unwrap();
    let item = enqueue(&*globex, &id, "step", "shared").await;
    globex
        .write_snapshot(Snapshot::new(id.clone(), seq, json!({ "k": "v" })))
        .await
        .unwrap();
    let artifact = globex
        .put_artifact(b"globex's bytes", Some("text/plain"))
        .await
        .unwrap();
    globex
        .apply_approval_projection(
            ApprovalProjectionRow {
                execution_id: id.clone(),
                node_id: "step".into(),
                status: "pending".into(),
                user_id: None,
                comment: None,
                last_sequence: seq,
                tool_name: Some("tool".into()),
                approver: Some("human".into()),
                context: Some(json!({})),
            },
            "approvals",
            seq,
        )
        .await
        .unwrap();

    let sees = |b: Arc<dyn StateBackend>| {
        let (id, hash) = (id.clone(), artifact.hash.clone());
        async move {
            [
                b.get_execution(&id).await.unwrap().is_some(),
                !b.get_events(&id).await.unwrap().is_empty(),
                b.get_work_item(item).await.unwrap().is_some(),
                b.latest_snapshot(&id).await.unwrap().is_some(),
                b.get_artifact(&hash).await.unwrap().is_some(),
                !b.get_approval_projection(&id).await.unwrap().is_empty(),
            ]
        }
    };
    // In the order of the reads above.
    let reads = [
        "get_execution",
        "get_events",
        "get_work_item",
        "latest_snapshot",
        "get_artifact",
        "get_approval_projection",
    ];
    let seen_by = |seen: [bool; 6], want: bool| -> Vec<&str> {
        reads
            .iter()
            .zip(seen)
            .filter(|(_, s)| *s != want)
            .map(|(r, _)| *r)
            .collect()
    };
    assert_eq!(
        seen_by(sees(globex).await, true),
        Vec::<&str>::new(),
        "globex's own view misses its rows"
    );
    assert_eq!(
        seen_by(sees(acme).await, false),
        Vec::<&str>::new(),
        "acme's view reads globex's rows"
    );
}
