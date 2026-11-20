//! A tenant's view of the store reaches only that tenant's rows: its claims
//! never take, reclaim or report another tenant's items, its writes never
//! land in another tenant's run, and its reads of another tenant's rows come
//! back empty. Each case runs `acme` beside `globex` on the stores that have
//! tenant views, and checks the same call on `globex`'s own view, so an
//! empty answer comes from the scope and not from a missing row.
//!
//! A write that names a run the view does not own is `NotFound`, as a
//! scoped read of that run is, and writes nothing. A run of another tenant
//! and a run that does not exist get the same answer, so the error does not
//! tell one tenant which run ids another holds.

use crate::conformance::cases::support::{tenant_view, without_tenant_views};
use crate::conformance::Fixture;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    ApprovalProjectionRow, BackendResult, ClaimFilter, ConditionalAppend, Event, EventKind,
    ExecutionClaim, SchedulerPass, Snapshot, StateBackend, StateBackendError, WorkItem, WorkItemId,
    DEFAULT_TENANT,
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

/// A pending item of the run on `queue`.
fn item(id: &ExecutionId, node: &str, queue: &str) -> WorkItem {
    WorkItem {
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
    }
}

/// A pending item of the run on `queue`, through the run's tenant's view.
async fn enqueue(b: &dyn StateBackend, id: &ExecutionId, node: &str, queue: &str) -> WorkItemId {
    b.enqueue_work_item(item(id, node, queue)).await.unwrap()
}

/// `node`'s completion in the run.
fn completed(id: &ExecutionId, node: &str) -> Event {
    Event::new(
        id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: node.into(),
            output: json!({ "ok": true }),
            state_patch: json!({ node: 1 }),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: Some(format!("key-{}", uuid::Uuid::new_v4())),
        },
    )
}

/// An approval row of `node` in the run.
fn approval(id: &ExecutionId, node: &str, status: &str, sequence: i64) -> ApprovalProjectionRow {
    ApprovalProjectionRow {
        execution_id: id.clone(),
        node_id: node.into(),
        status: status.into(),
        user_id: None,
        comment: None,
        last_sequence: sequence,
        tool_name: Some("tool".into()),
        approver: Some("human".into()),
        context: Some(json!({})),
    }
}

/// `globex`'s run, and a run id no tenant holds: the two runs `acme` does
/// not own.
fn not_acmes(globex_run: &ExecutionId) -> [(&'static str, ExecutionId); 2] {
    [
        ("globex's run", globex_run.clone()),
        ("an unknown run", ExecutionId::new()),
    ]
}

/// The call answered `NotFound`, and the store holds every row it held
/// `before`, where it keeps rows.
async fn refused<T: std::fmt::Debug>(
    fx: &Fixture,
    before: &Option<crate::conformance::StoreRows>,
    what: &str,
    out: BackendResult<T>,
) {
    assert!(
        matches!(out, Err(StateBackendError::NotFound(_))),
        "{what}: {out:?}"
    );
    assert!(
        fx.hooks.every_row().await == *before,
        "{what}: the refused write changed the store"
    );
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

/// `acme`'s append to `globex`'s run is `NotFound` and writes nothing, as
/// is its append to a run nobody holds; `globex`'s own append lands.
pub async fn scoped_append_to_another_tenants_run_is_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let unscoped = fx.hooks.unscoped();
    let started = |run: &ExecutionId, by: &str| {
        Event::new(
            run.clone(),
            0,
            EventKind::NodeStarted {
                node_id: "step".into(),
                worker_id: by.into(),
                attempt: 0,
            },
        )
    };

    let before = fx.hooks.every_row().await;
    let out = acme.append_event(started(&globex_run, "acme-worker")).await;
    assert!(
        matches!(out, Err(StateBackendError::NotFound(_))),
        "acme's append to globex's run: {out:?}"
    );
    assert!(
        unscoped.get_events(&globex_run).await.unwrap().is_empty(),
        "acme's refused append wrote nothing"
    );
    let unknown = ExecutionId::new();
    refused(
        fx,
        &before,
        "acme's append to an unknown run",
        acme.append_event(started(&unknown, "acme-worker")).await,
    )
    .await;

    let seq = globex
        .append_event(started(&globex_run, "globex-worker"))
        .await
        .unwrap();
    assert_eq!(seq, 1, "globex's own append lands");
}

/// `acme` holds an item of its own run and commits a turn whose event names
/// `globex`'s run, by both commit calls: `NotFound`, the item stays held and
/// nothing is written. The same commit into `acme`'s own run lands.
pub async fn scoped_commit_into_another_tenants_run_is_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let acme_run = new_run(&*acme).await;
    let queue = format!("commit-{}", acme_run.0.simple());
    enqueue(&*acme, &acme_run, "step", &queue).await;
    let held = acme
        .claim_work_item("acme-worker", &[queue.as_str()])
        .await
        .unwrap()
        .expect("acme's item");

    let before = fx.hooks.every_row().await;
    for (whose, run) in not_acmes(&globex_run) {
        for snapshot in [false, true] {
            let out = acme
                .commit_turn(held.id, held.lease_fence, completed(&run, "step"), snapshot)
                .await;
            let what = format!("commit_turn into {whose}, snapshot {snapshot}");
            refused(fx, &before, &what, out).await;
            let out = acme
                .commit_turn_unless_terminal(
                    held.id,
                    held.lease_fence,
                    completed(&run, "step"),
                    snapshot,
                )
                .await;
            let what = format!("commit_turn_unless_terminal into {whose}, snapshot {snapshot}");
            refused(fx, &before, &what, out).await;
        }
    }

    let seq = acme
        .commit_turn(
            held.id,
            held.lease_fence,
            completed(&acme_run, "step"),
            true,
        )
        .await
        .unwrap();
    assert_eq!(seq, 1, "acme's commit into its own run lands");
}

/// `globex`'s run has a snapshot at sequence 0. `acme`'s snapshot of that
/// run at the same sequence is `NotFound`, and the engine still reads
/// `globex`'s.
pub async fn scoped_snapshot_of_another_tenants_run_is_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    globex
        .write_snapshot(Snapshot::new(
            globex_run.clone(),
            0,
            json!({ "by": "globex" }),
        ))
        .await
        .unwrap();

    let before = fx.hooks.every_row().await;
    for (whose, run) in not_acmes(&globex_run) {
        let out = acme
            .write_snapshot(Snapshot::new(run, 0, json!({ "by": "acme" })))
            .await;
        refused(fx, &before, &format!("write_snapshot of {whose}"), out).await;
    }
    let read = fx
        .hooks
        .unscoped()
        .latest_snapshot(&globex_run)
        .await
        .unwrap()
        .expect("globex's snapshot");
    assert_eq!(read.state, json!({ "by": "globex" }));
}

/// `acme`'s item for `globex`'s run is `NotFound` and is not queued, so no
/// worker runs a node of `globex`'s run on `acme`'s word. `globex`'s own
/// item is queued.
pub async fn scoped_enqueue_into_another_tenants_run_is_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;

    let before = fx.hooks.every_row().await;
    for (whose, run) in not_acmes(&globex_run) {
        let out = acme.enqueue_work_item(item(&run, "step", "shared")).await;
        refused(fx, &before, &format!("enqueue into {whose}"), out).await;
    }
    globex
        .enqueue_work_item(item(&globex_run, "step", "shared"))
        .await
        .unwrap();
}

/// `globex`'s run has a pending approval row and a projector checkpoint.
/// `acme`'s projection of that run, single, batched or the checkpoint alone,
/// is `NotFound`: the row stays pending for `globex`'s reviewers and the
/// projector does not skip ahead.
pub async fn scoped_projection_of_another_tenants_run_is_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    globex
        .apply_approval_projection(approval(&globex_run, "gate", "pending", 2), "approvals", 2)
        .await
        .unwrap();

    let before = fx.hooks.every_row().await;
    for (whose, run) in not_acmes(&globex_run) {
        let out = acme
            .apply_approval_projection(approval(&run, "gate", "approved", 9), "approvals", 9)
            .await;
        refused(
            fx,
            &before,
            &format!("apply_approval_projection to {whose}"),
            out,
        )
        .await;
        let out = acme
            .apply_approval_projection_batch(
                vec![approval(&run, "gate", "approved", 9)],
                "approvals",
                &run,
                9,
            )
            .await;
        let what = format!("apply_approval_projection_batch to {whose}");
        refused(fx, &before, &what, out).await;
        let out = acme.set_projector_checkpoint("approvals", &run, 9).await;
        refused(
            fx,
            &before,
            &format!("set_projector_checkpoint of {whose}"),
            out,
        )
        .await;
    }
    let rows = globex.get_approval_projection(&globex_run).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "pending");
    let checkpoint = globex
        .get_projector_checkpoint("approvals", &globex_run)
        .await
        .unwrap();
    assert_eq!(checkpoint, 2);
}

/// `acme` starts a run whose start item names `globex`'s run, and a segment
/// whose parent is `globex`'s run: both `NotFound`, with no run started.
/// The segment would take the id `globex`'s own next segment is due to
/// take. `acme`'s segment of its own run starts.
pub async fn scoped_start_naming_another_tenants_run_is_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let acme_run = new_run(&*acme).await;
    let started = || EventKind::WorkflowStarted {
        workflow_id: "wf-tenancy".into(),
        workflow_version: "1.0.0".into(),
        initial_input: json!({}),
        initiated_by: None,
    };
    let scheduled = |start: &WorkItem| EventKind::NodeScheduled {
        node_id: start.node_id.clone(),
        queue_type: start.queue_type.clone(),
    };
    let segment_of = |parent: &ExecutionId| {
        let id = jamjet_state::segment::segment_execution_id(parent, 1);
        let mut segment = run(&id);
        segment.parent_execution_id = Some(parent.clone());
        segment.segment_number = 1;
        segment
    };

    let before = fx.hooks.every_row().await;
    for (whose, other) in not_acmes(&globex_run) {
        let new = ExecutionId::new();
        let start = item(&other, "start", "shared");
        let out = acme
            .start_execution_atomic(run(&new), started(), scheduled(&start), start)
            .await;
        let what = format!("start_execution_atomic with an item of {whose}");
        refused(fx, &before, &what, out).await;

        let segment = segment_of(&other);
        let start = item(&segment.execution_id, "start", "shared");
        let seed = Snapshot::seed_for_segment(segment.execution_id.clone(), &json!({}));
        let out = acme
            .create_segment_atomic(segment, seed, started(), scheduled(&start), start)
            .await;
        let what = format!("create_segment_atomic under {whose}");
        refused(fx, &before, &what, out).await;
    }

    let segment = segment_of(&acme_run);
    let start = item(&segment.execution_id, "start", "shared");
    let seed = Snapshot::seed_for_segment(segment.execution_id.clone(), &json!({}));
    let id = segment.execution_id.clone();
    acme.create_segment_atomic(segment, seed, started(), scheduled(&start), start)
        .await
        .unwrap();
    assert!(acme.get_execution(&id).await.unwrap().is_some());
}

/// The scoped writes that read the run's row before they write: each is
/// `NotFound` on `globex`'s run and on an unknown run, and writes nothing.
pub async fn scoped_checked_writes_to_another_tenants_run_are_not_found(fx: &Fixture) {
    let Some((acme, globex)) = two_tenants(fx).await else {
        return;
    };
    let globex_run = new_run(&*globex).await;
    let skipped = |run: &ExecutionId| {
        Event::new(
            run.clone(),
            0,
            EventKind::NodeSkipped {
                node_id: "step".into(),
                reason: "not taken".into(),
            },
        )
    };

    let before = fx.hooks.every_row().await;
    for (whose, run) in not_acmes(&globex_run) {
        let always = |_: &[Event]| true;
        let out = acme.append_event_if(skipped(&run), &always).await;
        refused(fx, &before, &format!("append_event_if to {whose}"), out).await;

        let cancelled = Event::new(
            run.clone(),
            0,
            EventKind::WorkflowCancelled { reason: None },
        );
        let out = acme
            .finish_execution(cancelled, WorkflowStatus::Cancelled)
            .await;
        refused(fx, &before, &format!("finish_execution of {whose}"), out).await;

        let out = acme
            .update_execution_status(&run, WorkflowStatus::Paused)
            .await;
        refused(
            fx,
            &before,
            &format!("update_execution_status of {whose}"),
            out,
        )
        .await;

        let out = acme.patch_append_array(&run, "list", json!(1)).await;
        refused(fx, &before, &format!("patch_append_array of {whose}"), out).await;

        let pass = SchedulerPass {
            execution_id: run.clone(),
            seen_sequence: 0,
            events: vec![EventKind::NodeScheduled {
                node_id: "step".into(),
                queue_type: "shared".into(),
            }],
            work_items: vec![item(&run, "step", "shared")],
            finish: None,
        };
        let out = acme.commit_scheduler_pass(pass).await;
        refused(
            fx,
            &before,
            &format!("commit_scheduler_pass of {whose}"),
            out,
        )
        .await;
    }
    let always = |_: &[Event]| true;
    let out = globex
        .append_event_if(skipped(&globex_run), &always)
        .await
        .unwrap();
    assert_eq!(
        out,
        ConditionalAppend::Appended(1),
        "globex's own append lands"
    );
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
