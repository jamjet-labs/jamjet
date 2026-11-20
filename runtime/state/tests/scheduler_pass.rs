//! `commit_scheduler_pass`: what one scheduler pass decided for one run lands
//! in one write. The pass is refused, with nothing written, when the log moved
//! past what the pass read, when a node it dispatches already has a pending or
//! claimed item, or when the run is over; a pass that does not hold together
//! is an error. Two passes built from the same read commit once.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    ClaimFilter, Event, EventKind, InMemoryBackend, PassCommit, SchedulerPass, SqliteBackend,
    StateBackend, StateBackendError, Tenant, TenantId, TenantScopedSqliteBackend, TenantStatus,
    WorkItem, DEFAULT_TENANT,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

fn sqlite_url(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "jamjet_pass_{tag}_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    format!("sqlite://{}", p.display())
}

async fn scoped_to_acme(base: &SqliteBackend) -> TenantScopedSqliteBackend {
    scoped_to(base, "acme").await
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

/// One fresh store per backend. Each test starts one run per store, so the
/// in-memory backend's sequences (one counter for every run) are the run's
/// own.
async fn backends() -> Vec<(&'static str, Arc<dyn StateBackend>)> {
    let sqlite = SqliteBackend::open(&sqlite_url("a")).await.unwrap();
    let base = SqliteBackend::open(&sqlite_url("b")).await.unwrap();
    let scoped = scoped_to_acme(&base).await;
    vec![
        ("memory", Arc::new(InMemoryBackend::new())),
        ("sqlite", Arc::new(sqlite)),
        ("tenant-scoped sqlite", Arc::new(scoped)),
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

/// An item as the scheduler builds one: the default tenant, whatever the
/// run's is.
fn item(execution_id: &ExecutionId, node: &str) -> WorkItem {
    WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: node.into(),
        queue_type: "general".into(),
        payload: json!({ "node_id": node }),
        attempt: 0,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

fn scheduled(node: &str) -> EventKind {
    EventKind::NodeScheduled {
        node_id: node.into(),
        queue_type: "general".into(),
    }
}

/// Start a run as `POST /executions` does: `WorkflowStarted` and node `a`'s
/// `NodeScheduled`, with `a`'s item pending. Returns the latest sequence.
async fn start(b: &dyn StateBackend, id: &ExecutionId) -> i64 {
    b.start_execution_atomic(
        run(id),
        EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({}),
            initiated_by: None,
            run_key: None,
        },
        scheduled("a"),
        item(id, "a"),
    )
    .await
    .unwrap();
    b.latest_sequence(id).await.unwrap()
}

/// The pass a scheduler builds to dispatch `node`.
fn dispatch(id: &ExecutionId, seen: i64, node: &str) -> SchedulerPass {
    SchedulerPass {
        execution_id: id.clone(),
        seen_sequence: seen,
        events: vec![scheduled(node)],
        work_items: vec![item(id, node)],
        finish: None,
    }
}

/// `node_scheduled(b)`, `workflow_completed`: an event as the log reads.
fn label(e: &Event) -> String {
    let value = serde_json::to_value(&e.kind).unwrap();
    let kind = value["type"].as_str().unwrap_or("?").to_string();
    match e.kind.node_id() {
        Some(node) => format!("{kind}({node})"),
        None => kind,
    }
}

async fn log(b: &dyn StateBackend, id: &ExecutionId) -> Vec<String> {
    b.get_events(id)
        .await
        .unwrap()
        .iter()
        .map(|e| format!("{}:{}", e.sequence, label(e)))
        .collect()
}

/// Nothing the refused `pass` carried was written: the log still ends at
/// `latest` and none of its items exists.
async fn assert_nothing_written(
    name: &str,
    b: &dyn StateBackend,
    pass_items: &[WorkItem],
    id: &ExecutionId,
    latest: i64,
) {
    assert_eq!(
        b.latest_sequence(id).await.unwrap(),
        latest,
        "{name}: the log moved; {:?}",
        log(b, id).await
    );
    for it in pass_items {
        assert!(
            b.get_work_item(it.id).await.unwrap().is_none(),
            "{name}: item for {} was written",
            it.node_id
        );
    }
}

#[tokio::test]
async fn a_pass_lands_events_items_and_finish_together() {
    for (name, b) in backends().await {
        let id = ExecutionId::new();
        let seen = start(&*b, &id).await;

        let empty = SchedulerPass {
            execution_id: id.clone(),
            seen_sequence: seen,
            events: vec![],
            work_items: vec![],
            finish: None,
        };
        assert_eq!(
            b.commit_scheduler_pass(empty).await.unwrap(),
            PassCommit::Committed {
                last_sequence: seen
            },
            "{name}: an empty pass"
        );
        assert_eq!(
            b.latest_sequence(&id).await.unwrap(),
            seen,
            "{name}: an empty pass writes nothing"
        );

        let b_item = item(&id, "b");
        let pass = SchedulerPass {
            execution_id: id.clone(),
            seen_sequence: seen,
            events: vec![
                EventKind::NodeSkipped {
                    node_id: "x".into(),
                    reason: "branch not taken".into(),
                },
                scheduled("b"),
            ],
            work_items: vec![b_item.clone()],
            finish: Some((
                WorkflowStatus::Completed,
                EventKind::WorkflowCompleted {
                    final_state: json!({}),
                },
            )),
        };
        let out = b.commit_scheduler_pass(pass).await.unwrap();
        assert_eq!(
            out,
            PassCommit::Committed {
                last_sequence: seen + 3
            },
            "{name}"
        );

        let tail = b.get_events_since(&id, seen).await.unwrap();
        assert_eq!(
            tail.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![seen + 1, seen + 2, seen + 3],
            "{name}: sequences are dense from the one the pass saw"
        );
        assert_eq!(
            tail.iter().map(label).collect::<Vec<_>>(),
            vec!["node_skipped(x)", "node_scheduled(b)", "workflow_completed"],
            "{name}: the events in order, then the terminal event"
        );
        let stored = b.get_work_item(b_item.id).await.unwrap();
        assert!(
            stored.as_ref().is_some_and(|i| i.node_id == "b"),
            "{name}: the item landed: {stored:?}"
        );
        let status = b.get_execution(&id).await.unwrap().unwrap().status;
        assert_eq!(status, WorkflowStatus::Completed, "{name}");
    }
}

#[tokio::test]
async fn a_stale_pass_writes_nothing() {
    for (name, b) in backends().await {
        let id = ExecutionId::new();
        let seen = start(&*b, &id).await;
        // Another writer lands after the pass read the log.
        let moved = b
            .append_event(Event::new(
                id.clone(),
                0,
                EventKind::NodeStarted {
                    node_id: "a".into(),
                    worker_id: "w1".into(),
                    attempt: 0,
                },
            ))
            .await
            .unwrap();
        assert_eq!(moved, seen + 1, "{name}");

        let pass = dispatch(&id, seen, "b");
        let items = pass.work_items.clone();
        let out = b.commit_scheduler_pass(pass).await.unwrap();
        assert_eq!(
            out,
            PassCommit::Stale {
                latest_sequence: moved
            },
            "{name}"
        );
        assert_nothing_written(name, &*b, &items, &id, moved).await;
    }
}

#[tokio::test]
async fn a_pass_for_a_node_with_a_live_item_is_stale() {
    for (name, b) in backends().await {
        let id = ExecutionId::new();
        let seen = start(&*b, &id).await;

        // `a`'s item is pending.
        let pass = dispatch(&id, seen, "a");
        let items = pass.work_items.clone();
        let out = b.commit_scheduler_pass(pass).await.unwrap();
        assert_eq!(
            out,
            PassCommit::Stale {
                latest_sequence: seen
            },
            "{name}: a pending item"
        );
        assert_nothing_written(name, &*b, &items, &id, seen).await;

        // Claimed now.
        let held = b
            .claim_work_item("w1", &["general"])
            .await
            .unwrap()
            .expect("a's item is claimable");
        assert_eq!(held.node_id, "a", "{name}");
        let pass = dispatch(&id, seen, "a");
        let items = pass.work_items.clone();
        let out = b.commit_scheduler_pass(pass).await.unwrap();
        assert_eq!(
            out,
            PassCommit::Stale {
                latest_sequence: seen
            },
            "{name}: a claimed item"
        );
        assert_nothing_written(name, &*b, &items, &id, seen).await;

        // Settled: the node may be dispatched again.
        assert!(
            b.complete_work_item_fenced(held.id, held.lease_fence)
                .await
                .unwrap(),
            "{name}"
        );
        let out = b
            .commit_scheduler_pass(dispatch(&id, seen, "a"))
            .await
            .unwrap();
        assert_eq!(
            out,
            PassCommit::Committed {
                last_sequence: seen + 1
            },
            "{name}: once the item settled"
        );
    }
}

#[tokio::test]
async fn a_pass_on_a_finished_run_is_refused() {
    for (name, b) in backends().await {
        let id = ExecutionId::new();
        start(&*b, &id).await;
        b.finish_execution(
            Event::new(
                id.clone(),
                0,
                EventKind::WorkflowCompleted {
                    final_state: json!({}),
                },
            ),
            WorkflowStatus::Completed,
        )
        .await
        .unwrap();
        // The pass read the whole log, the terminal event included.
        let seen = b.latest_sequence(&id).await.unwrap();

        let pass = dispatch(&id, seen, "b");
        let items = pass.work_items.clone();
        let out = b.commit_scheduler_pass(pass).await.unwrap();
        assert_eq!(
            out,
            PassCommit::ExecutionTerminal(WorkflowStatus::Completed),
            "{name}"
        );
        assert_nothing_written(name, &*b, &items, &id, seen).await;
    }
}

#[tokio::test]
async fn a_pass_item_of_another_run_is_refused() {
    for (name, b) in backends().await {
        let id = ExecutionId::new();
        let seen = start(&*b, &id).await;
        let other = ExecutionId::new();

        let pass = SchedulerPass {
            execution_id: id.clone(),
            seen_sequence: seen,
            events: vec![scheduled("b")],
            work_items: vec![item(&other, "b")],
            finish: None,
        };
        let items = pass.work_items.clone();
        let out = b.commit_scheduler_pass(pass).await;
        assert!(
            matches!(out, Err(StateBackendError::Database(_))),
            "{name}: {out:?}"
        );
        assert_nothing_written(name, &*b, &items, &id, seen).await;
    }
}

/// The other shapes a pass may not have: an item whose node the pass does
/// not schedule, two items for one node, a finish that does not end the run.
#[tokio::test]
async fn a_pass_that_does_not_hold_together_is_refused() {
    for (name, b) in backends().await {
        let id = ExecutionId::new();
        let seen = start(&*b, &id).await;

        let unscheduled = SchedulerPass {
            execution_id: id.clone(),
            seen_sequence: seen,
            events: vec![scheduled("b")],
            work_items: vec![item(&id, "c")],
            finish: None,
        };
        let twice = SchedulerPass {
            execution_id: id.clone(),
            seen_sequence: seen,
            events: vec![scheduled("b")],
            work_items: vec![item(&id, "b"), item(&id, "b")],
            finish: None,
        };
        let not_an_end = SchedulerPass {
            execution_id: id.clone(),
            seen_sequence: seen,
            events: vec![],
            work_items: vec![],
            finish: Some((
                WorkflowStatus::Running,
                EventKind::WorkflowCompleted {
                    final_state: json!({}),
                },
            )),
        };
        for (shape, pass) in [
            ("an item with no NodeScheduled", unscheduled),
            ("two items for one node", twice),
            ("a non-terminal finish", not_an_end),
        ] {
            let items = pass.work_items.clone();
            let out = b.commit_scheduler_pass(pass).await;
            assert!(
                matches!(out, Err(StateBackendError::Database(_))),
                "{name}, {shape}: {out:?}"
            );
            assert_nothing_written(name, &*b, &items, &id, seen).await;
            assert_eq!(
                b.get_execution(&id).await.unwrap().unwrap().status,
                WorkflowStatus::Running,
                "{name}, {shape}"
            );
        }
    }
}

/// The scheduler runs unscoped and builds every item with the default
/// tenant. The pass stamps the run's tenant instead, on the items and on the
/// events, so the run's tenant sees what the scheduler wrote for it.
#[tokio::test]
async fn a_pass_stamps_items_with_the_runs_tenant() {
    let base = SqliteBackend::open(&sqlite_url("tenant")).await.unwrap();
    let scoped = scoped_to_acme(&base).await;
    let unscoped: &dyn StateBackend = &base;
    let in_scope: &dyn StateBackend = &scoped;
    for (name, writer) in [("sqlite", unscoped), ("tenant-scoped sqlite", in_scope)] {
        let id = ExecutionId::new();
        let seen = start(&scoped, &id).await;

        let pass = dispatch(&id, seen, "b");
        let built = pass.work_items[0].clone();
        assert_eq!(built.tenant_id, DEFAULT_TENANT, "{name}");
        let out = writer.commit_scheduler_pass(pass).await.unwrap();
        assert_eq!(
            out,
            PassCommit::Committed {
                last_sequence: seen + 1
            },
            "{name}"
        );

        let stored = base
            .get_work_item(built.id)
            .await
            .unwrap()
            .expect("the item landed");
        assert_eq!(stored.tenant_id, "acme", "{name}: the item's tenant");
        let seen_by_tenant = scoped.get_events_since(&id, seen).await.unwrap();
        assert_eq!(
            seen_by_tenant.iter().map(label).collect::<Vec<_>>(),
            vec!["node_scheduled(b)"],
            "{name}: the run's tenant reads the pass's event"
        );
    }
}

/// A scoped view finds only its own tenant's runs. `acme`'s view, given a
/// pass with the right `seen` for a run of `beta` or of `default`, answers
/// `NotFound` and writes nothing into that run. The run's own store then
/// commits the same pass, so the refusal came from the scope.
#[tokio::test]
async fn a_scoped_pass_for_a_run_of_another_tenant_is_not_found() {
    let base = SqliteBackend::open(&sqlite_url("other_tenant"))
        .await
        .unwrap();
    let in_acme = scoped_to(&base, "acme").await;
    let in_beta = scoped_to(&base, "beta").await;
    let beta: &dyn StateBackend = &in_beta;
    let default: &dyn StateBackend = &base;
    for (owner, own_store) in [("beta", beta), ("default", default)] {
        let id = ExecutionId::new();
        let seen = start(own_store, &id).await;
        let before = log(&base, &id).await;

        let pass = dispatch(&id, seen, "b");
        let items = pass.work_items.clone();
        let out = in_acme.commit_scheduler_pass(pass).await;
        assert!(
            matches!(out, Err(StateBackendError::NotFound(_))),
            "acme's view, a run of {owner}: {out:?}"
        );
        assert_eq!(log(&base, &id).await, before, "a run of {owner}: the log");
        assert_nothing_written(owner, &base, &items, &id, seen).await;

        let out = own_store
            .commit_scheduler_pass(dispatch(&id, seen, "b"))
            .await
            .unwrap();
        assert_eq!(
            out,
            PassCommit::Committed {
                last_sequence: seen + 1
            },
            "the run's own store, {owner}"
        );
    }
}

/// Two stores that write one database, as two engine instances do.
async fn pairs() -> Vec<(&'static str, Arc<dyn StateBackend>, Arc<dyn StateBackend>)> {
    let memory = Arc::new(InMemoryBackend::new());
    let url = sqlite_url("race");
    let one = SqliteBackend::open(&url).await.unwrap();
    let two = SqliteBackend::open(&url).await.unwrap();
    let scoped_url = sqlite_url("race_scoped");
    let scoped_one = SqliteBackend::open(&scoped_url).await.unwrap();
    let scoped_two = SqliteBackend::open(&scoped_url).await.unwrap();
    let scoped_one = scoped_to_acme(&scoped_one).await;
    let scoped_two = scoped_to_acme(&scoped_two).await;
    vec![
        ("memory", memory.clone(), memory),
        ("sqlite", Arc::new(one), Arc::new(two)),
        (
            "tenant-scoped sqlite",
            Arc::new(scoped_one),
            Arc::new(scoped_two),
        ),
    ]
}

/// When a call started and when it returned.
#[derive(Debug, Clone, Copy)]
struct Span {
    start: Instant,
    end: Instant,
}

impl Span {
    /// Both calls had started before either returned.
    fn overlaps(self, other: Span) -> bool {
        self.start.max(other.start) < self.end.min(other.end)
    }
}

async fn timed<F: std::future::Future>(work: F) -> (F::Output, Span) {
    let start = Instant::now();
    let out = work.await;
    let end = Instant::now();
    (out, Span { start, end })
}

/// Run `a` and `b` at once, and say whether they overlapped: both started
/// before either returned. Each runs on a thread of its own that spins until
/// both are ready, so the two start within nanoseconds. Woken by a barrier
/// instead, the second starts a wake-up later, by which time an in-memory
/// write (no await between its checks and its writes) has finished, and the
/// race never opens. Two calls run one after the other give the answers the
/// race tests accept, so a round that did not overlap shows nothing; each
/// race test asserts that some rounds did.
async fn race<A, B>(a: A, b: B) -> (A::Output, B::Output, bool)
where
    A: std::future::Future + Send + 'static,
    A::Output: Send + 'static,
    B: std::future::Future + Send + 'static,
    B::Output: Send + 'static,
{
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    fn spin_then_run<F>(
        work: F,
        ready: Arc<AtomicUsize>,
        go: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<(F::Output, Span)>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            ready.fetch_add(1, Ordering::AcqRel);
            while !go.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            runtime.block_on(timed(work))
        })
    }
    let ready = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let a = spin_then_run(a, ready.clone(), go.clone());
    let b = spin_then_run(b, ready.clone(), go.clone());
    while ready.load(Ordering::Acquire) < 2 {
        tokio::task::yield_now().await;
    }
    go.store(true, Ordering::Release);
    let (x, a_span) = a.await.unwrap();
    let (y, b_span) = b.await.unwrap();
    (x, y, a_span.overlaps(b_span))
}

/// Two schedulers that read the same log build the same pass. Released at
/// once, exactly one commits and the other is told it is stale: the node is
/// scheduled once and has one item. In a round where the two calls
/// overlapped, the second ran while the first held the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_passes_at_one_sequence_commit_once() {
    const ROUNDS: usize = 200;
    for (name, first, second) in pairs().await {
        let mut overlapped = 0;
        for round in 0..ROUNDS {
            let id = ExecutionId::new();
            let seen = start(&*first, &id).await;
            let passes = [dispatch(&id, seen, "b"), dispatch(&id, seen, "b")];
            let items: Vec<WorkItem> = passes.iter().map(|p| p.work_items[0].clone()).collect();
            let [one, two] = passes;
            let (a, b) = (first.clone(), second.clone());
            let (x, y, at_once) = race(
                async move { a.commit_scheduler_pass(one).await },
                async move { b.commit_scheduler_pass(two).await },
            )
            .await;
            overlapped += usize::from(at_once);
            let outcomes = [x.unwrap(), y.unwrap()];

            let committed: Vec<i64> = outcomes
                .iter()
                .filter_map(|o| match o {
                    PassCommit::Committed { last_sequence } => Some(*last_sequence),
                    _ => None,
                })
                .collect();
            let stale: Vec<i64> = outcomes
                .iter()
                .filter_map(|o| match o {
                    PassCommit::Stale { latest_sequence } => Some(*latest_sequence),
                    _ => None,
                })
                .collect();
            assert_eq!(
                (committed.as_slice(), stale.as_slice()),
                (&[seen + 1][..], &[seen + 1][..]),
                "{name}, round {round}: one commits, the other is stale at its sequence; \
                 {outcomes:?}, log {:?}",
                log(&*first, &id).await
            );
            let scheduled_b = first
                .get_events(&id)
                .await
                .unwrap()
                .iter()
                .filter(|e| label(e) == "node_scheduled(b)")
                .count();
            assert_eq!(scheduled_b, 1, "{name}, round {round}");
            let mut landed = 0;
            for it in &items {
                if first.get_work_item(it.id).await.unwrap().is_some() {
                    landed += 1;
                }
            }
            assert_eq!(landed, 1, "{name}, round {round}: items for b");
        }
        assert!(
            overlapped > 0,
            "{name}: the two passes never overlapped in {ROUNDS} rounds, so no round raced"
        );
    }
}

/// A worker's commit and a scheduler's pass that read the log before it,
/// released at once, land one after the other: the pass first and the
/// commit after it, or the commit first and the pass stale. Never the pass
/// checked before the commit and written after it. Some rounds must
/// overlap, or either order could come from two calls in turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pass_and_a_worker_commit_racing_land_one_after_the_other() {
    const ROUNDS: usize = 200;
    for (name, scheduler, worker) in pairs().await {
        let mut overlapped = 0;
        for round in 0..ROUNDS {
            let id = ExecutionId::new();
            let seen = start(&*scheduler, &id).await;
            let held = worker
                .claim_work_item_filtered(
                    "w1",
                    &["general"],
                    None,
                    &ClaimFilter {
                        execution_id: Some(id.clone()),
                        node_id: None,
                    },
                )
                .await
                .unwrap()
                .expect("a's item is claimable");
            let failed = Event::new(
                id.clone(),
                0,
                EventKind::NodeFailed {
                    node_id: "a".into(),
                    error: "boom".into(),
                    attempt: 0,
                    retryable: false,
                },
            );
            let pass = dispatch(&id, seen, "b");
            let (s, w) = (scheduler.clone(), worker.clone());
            let (passed, committed, at_once) = race(
                async move { s.commit_scheduler_pass(pass).await },
                async move {
                    w.commit_turn(held.id, held.lease_fence, failed, false)
                        .await
                },
            )
            .await;
            overlapped += usize::from(at_once);
            let (passed, committed) = (passed.unwrap(), committed.unwrap());
            let one_after_the_other = match passed {
                PassCommit::Committed { last_sequence } => {
                    last_sequence == seen + 1 && committed == seen + 2
                }
                PassCommit::Stale { latest_sequence } => {
                    latest_sequence == seen + 1 && committed == seen + 1
                }
                PassCommit::ExecutionTerminal(_) => false,
            };
            assert!(
                one_after_the_other,
                "{name}, round {round}: pass {passed:?}, commit at {committed}; log {:?}",
                log(&*scheduler, &id).await
            );
        }
        assert!(
            overlapped > 0,
            "{name}: the pass and the commit never overlapped in {ROUNDS} rounds, \
             so no round raced"
        );
    }
}
