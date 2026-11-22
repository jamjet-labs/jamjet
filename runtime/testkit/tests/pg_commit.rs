//! The fenced commit of a node's turn on the Postgres store, where the
//! suite's cases do not reach: two siblings of one run committing at once on
//! two engines, a cancel that lands before the commit, a cancel that lands
//! while the commit holds the run, and a commit replayed with the held fence.
//!
//! The commit locks the run's row, then the item's, and folds the snapshot
//! under the run's lock. So two siblings take turns at the run, and the
//! second folds the first's event; a cancel lands wholly before a commit,
//! which then withdraws its item, or wholly after it.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::faults::{Fault, Scope};
use jamjet_state::{
    Event, EventKind, ExecutionFinish, PgBackend, PgSettings, StateBackend, StateBackendError,
    Tenant, TenantId, TenantStatus, TurnCommit, WorkItem, DEFAULT_TENANT,
};
use jamjet_testkit::conformance::cases::support::race;
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use serde_json::json;
use sqlx::PgPool;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The point right after the commit has locked the run and the item.
const LOCKED: &str = "pg.commit_turn";

fn settings() -> PgSettings {
    PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    }
}

async fn store(db: &TestDatabase) -> PgBackend {
    PgBackend::connect(db.url(), settings())
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-pg-commit".into(),
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

/// A pending item of `node` of the run, on a queue of the run's own.
fn item(id: &ExecutionId, node: &str) -> WorkItem {
    WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: node.into(),
        queue_type: queue_of(id),
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

fn queue_of(id: &ExecutionId) -> String {
    format!("commit-{}", id.0.simple())
}

/// Queue `node`'s item and claim it as `worker`.
async fn claimed(b: &dyn StateBackend, id: &ExecutionId, node: &str, worker: &str) -> WorkItem {
    b.enqueue_work_item(item(id, node)).await.unwrap();
    b.claim_work_item(worker, &[queue_of(id).as_str()])
        .await
        .unwrap()
        .expect("the run's item is claimable")
}

/// `node`'s completion, with `patch` and an idempotency key.
fn completed(id: &ExecutionId, node: &str, patch: serde_json::Value) -> Event {
    Event::new(
        id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: node.into(),
            output: json!({ "ok": true }),
            state_patch: patch,
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: Some(format!("key-{node}-{}", id.0.simple())),
        },
    )
}

async fn cancel(b: &dyn StateBackend, id: &ExecutionId) -> ExecutionFinish {
    let event = Event::new(id.clone(), 0, EventKind::WorkflowCancelled { reason: None });
    b.finish_execution(event, WorkflowStatus::Cancelled)
        .await
        .unwrap()
}

/// The `type` tags of the run's events, in order, read past any view.
async fn kinds_of(pool: &PgPool, id: &ExecutionId) -> Vec<String> {
    sqlx::query_scalar("SELECT kind_type FROM events WHERE execution_id = $1 ORDER BY sequence")
        .bind(id.0)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// The item's status and worker, read past any view.
async fn item_row(pool: &PgPool, id: uuid::Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, worker_id FROM work_items WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn make_tenant(b: &PgBackend, id: &str) -> PgBackend {
    let view = b.for_tenant(TenantId::from(id));
    let now = chrono::Utc::now();
    view.create_tenant(Tenant {
        id: TenantId::from(id),
        name: id.to_string(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
    view
}

/// Two nodes of one run, each held on its own engine, commit with a
/// snapshot at the same moment, 200 times. Every time both land, at
/// sequences 1 and 2, and the run's latest snapshot holds both patches and
/// both nodes: the second commit folded the first's event under the run's
/// lock. The calls overlapped in some rounds, so the commits did meet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sibling_commits_never_lose_a_patch() {
    const ROUNDS: i64 = 200;
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("sibling_commits_never_lose_a_patch");
    };
    let (one, two) = (Arc::new(store(&db).await), Arc::new(store(&db).await));
    let mut met = 0;
    for round in 0..ROUNDS {
        let id = new_run(&*one).await;
        let a = claimed(&*one, &id, "n_a", "worker-a").await;
        let b = claimed(&*two, &id, "n_b", "worker-b").await;
        let (left, right, overlapped) = race(
            {
                let (one, id) = (one.clone(), id.clone());
                async move {
                    let event = completed(&id, "n_a", json!({ "a": round }));
                    one.commit_turn(a.id, a.lease_fence, event, true).await
                }
            },
            {
                let (two, id) = (two.clone(), id.clone());
                async move {
                    let event = completed(&id, "n_b", json!({ "b": round }));
                    two.commit_turn(b.id, b.lease_fence, event, true).await
                }
            },
        )
        .await;
        met += i64::from(overlapped);
        let sequences = BTreeSet::from([
            left.unwrap_or_else(|e| panic!("round {round}: n_a: {e}")),
            right.unwrap_or_else(|e| panic!("round {round}: n_b: {e}")),
        ]);
        assert_eq!(sequences, BTreeSet::from([1, 2]), "round {round}");
        let snapshot = one
            .latest_snapshot(&id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("round {round}: no snapshot"));
        assert_eq!(
            snapshot.at_sequence, 2,
            "round {round}: the latest snapshot"
        );
        assert_eq!(
            (snapshot.state.get("a"), snapshot.state.get("b")),
            (Some(&json!(round)), Some(&json!(round))),
            "round {round}: a patch was lost: {}",
            snapshot.state
        );
        assert!(
            snapshot.completed_nodes.contains_key("n_a")
                && snapshot.completed_nodes.contains_key("n_b"),
            "round {round}: a node was lost: {:?}",
            snapshot.completed_nodes
        );
    }
    eprintln!("sibling commits: {met} of {ROUNDS} rounds overlapped");
    assert!(
        met > 0,
        "the two commits never ran at once in {ROUNDS} rounds"
    );
}

/// A run is cancelled, through another engine, after its item was claimed
/// and before the turn commits. The commit, unscoped and through a tenant's
/// view, answers `ExecutionTerminal`: the item is withdrawn, failed with no
/// worker, and nothing is recorded, no `NodeCompleted`, no snapshot, no
/// tool effect. A stale fence first gets `FenceLost` and leaves the item as
/// it was, ended run or not.
#[tokio::test]
async fn commit_after_cancel_withdraws() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("commit_after_cancel_withdraws");
    };
    let (one, two) = (store(&db).await, store(&db).await);
    let pool = db.pool().await;
    let acme = make_tenant(&one, "acme").await;
    let views: [(&str, PgBackend, PgBackend); 2] = [
        ("unscoped", one.clone(), two.clone()),
        ("acme", acme, two.for_tenant(TenantId::from("acme"))),
    ];
    for (whose, engine, other) in views {
        let id = new_run(&engine).await;
        let held = claimed(&engine, &id, "step", "worker-1").await;
        assert!(matches!(
            cancel(&other, &id).await,
            ExecutionFinish::Finished(_)
        ));

        let event = completed(&id, "step", json!({ "step": 1 }));
        let stale = engine
            .commit_turn_unless_terminal(held.id, held.lease_fence + 1, event.clone(), true)
            .await;
        assert!(
            matches!(stale, Err(StateBackendError::FenceLost(_))),
            "{whose}: a stale fence on the ended run: {stale:?}"
        );
        assert_eq!(
            item_row(&pool, held.id).await,
            ("claimed".to_string(), Some("worker-1".to_string())),
            "{whose}: the stale fence touched the item"
        );

        let commit = engine
            .commit_turn_unless_terminal(held.id, held.lease_fence, event, true)
            .await;
        assert_eq!(
            commit.unwrap_or_else(|e| panic!("{whose}: {e}")),
            TurnCommit::ExecutionTerminal(WorkflowStatus::Cancelled),
            "{whose}"
        );
        assert_eq!(
            kinds_of(&pool, &id).await,
            vec!["workflow_cancelled"],
            "{whose}: the commit recorded something"
        );
        assert_eq!(
            item_row(&pool, held.id).await,
            ("failed".to_string(), None),
            "{whose}: the item is withdrawn"
        );
        assert!(
            engine.latest_snapshot(&id).await.unwrap().is_none(),
            "{whose}: a snapshot was written"
        );
        let key = format!("key-step-{}", id.0.simple());
        assert!(
            engine.get_tool_effect(&key).await.unwrap().is_none(),
            "{whose}: a tool effect was recorded"
        );
    }
}

/// A commit holds its run, right after taking the locks, while another
/// engine cancels the run. The cancel waits for the commit: the commit
/// lands at sequence 1 and the cancel after it, at 2, and the cancel
/// returns only once the commit has. Had the commit read the run without
/// its lock, the cancel would have landed during the hold and the
/// completion after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_during_a_commit_waits_for_it() {
    const HOLD: Duration = Duration::from_secs(1);
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_cancel_during_a_commit_waits_for_it");
    };
    let (one, two) = (Arc::new(store(&db).await), store(&db).await);
    let pool = db.pool().await;
    let id = new_run(&*one).await;
    let held = claimed(&*one, &id, "step", "worker-1").await;

    let scope = Scope::new();
    scope.arm(LOCKED, 1, Fault::Hang(HOLD));
    let commit = tokio::spawn({
        let (scope, one, id) = (scope.clone(), one.clone(), id.clone());
        async move {
            let event = completed(&id, "step", json!({ "step": 1 }));
            let out = scope
                .run(one.commit_turn_unless_terminal(held.id, held.lease_fence, event, true))
                .await;
            (out, Instant::now())
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while scope.fired(LOCKED, 1) == 0 {
        assert!(
            Instant::now() < deadline && !commit.is_finished(),
            "the commit never reached {LOCKED}#1"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!commit.is_finished(), "the commit ended before the cancel");
    let cancelled = cancel(&two, &id).await;
    let cancel_returned = Instant::now();
    let (commit, commit_returned) = commit.await.unwrap();

    assert_eq!(commit.unwrap(), TurnCommit::Committed(1), "the commit");
    assert_eq!(cancelled, ExecutionFinish::Finished(2), "the cancel");
    assert!(
        cancel_returned >= commit_returned,
        "the cancel returned before the commit it should have waited for"
    );
    assert_eq!(
        kinds_of(&pool, &id).await,
        vec!["node_completed", "workflow_cancelled"]
    );
}

/// `acme` holds a run of its own and knows `globex`'s held item, fence
/// included. Its commit of that item into its own run is `FenceLost`: the
/// item stays `globex`'s worker's and nothing is recorded in either run.
/// `globex`'s own commit then lands.
#[tokio::test]
async fn a_view_cannot_commit_another_tenants_item() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_view_cannot_commit_another_tenants_item");
    };
    let base = store(&db).await;
    let pool = db.pool().await;
    let acme = make_tenant(&base, "acme").await;
    let globex = make_tenant(&base, "globex").await;
    let acme_run = new_run(&acme).await;
    let globex_run = new_run(&globex).await;
    let theirs = claimed(&globex, &globex_run, "step", "globex-worker").await;

    let event = completed(&acme_run, "step", json!({ "step": 1 }));
    for refuse_terminal in [false, true] {
        let out = if refuse_terminal {
            acme.commit_turn_unless_terminal(theirs.id, theirs.lease_fence, event.clone(), true)
                .await
        } else {
            acme.commit_turn(theirs.id, theirs.lease_fence, event.clone(), true)
                .await
                .map(TurnCommit::Committed)
        };
        assert!(
            matches!(out, Err(StateBackendError::FenceLost(_))),
            "acme committed globex's item: {out:?}"
        );
    }
    assert_eq!(
        item_row(&pool, theirs.id).await,
        ("claimed".to_string(), Some("globex-worker".to_string()))
    );
    assert!(kinds_of(&pool, &acme_run).await.is_empty());
    assert!(kinds_of(&pool, &globex_run).await.is_empty());

    let event = completed(&globex_run, "step", json!({ "step": 1 }));
    let seq = globex
        .commit_turn(theirs.id, theirs.lease_fence, event, true)
        .await
        .unwrap();
    assert_eq!(seq, 1, "globex's own commit lands");
}

/// The item's whole row as JSON text, read past any view.
async fn item_text(pool: &PgPool, id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT row_to_json(w)::text FROM work_items w WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Commit `held`'s turn with the fence it was claimed under, through
/// `commit_turn_unless_terminal` or through `commit_turn`.
async fn commit_held(
    b: &PgBackend,
    held: &WorkItem,
    event: Event,
    refuse_terminal: bool,
) -> Result<TurnCommit, StateBackendError> {
    if refuse_terminal {
        b.commit_turn_unless_terminal(held.id, held.lease_fence, event, true)
            .await
    } else {
        b.commit_turn(held.id, held.lease_fence, event, true)
            .await
            .map(TurnCommit::Committed)
    }
}

/// A turn is committed, or withdrawn by a cancel, and then committed again
/// with the fence it was claimed under: with the same event, as a retry
/// after an unknown outcome would, and with a new one, as a worker whose
/// item was withdrawn would. Unscoped and through a tenant's view, through
/// both commit calls, the replay is `FenceLost`: the
/// fence still matches, but the item is no longer claimed. The log keeps
/// its one `NodeCompleted` (none after a withdraw) and the item's row is
/// as it was.
#[tokio::test]
async fn a_replayed_commit_with_the_held_fence_is_fence_lost() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_replayed_commit_with_the_held_fence_is_fence_lost");
    };
    let (one, two) = (store(&db).await, store(&db).await);
    let pool = db.pool().await;
    let acme = make_tenant(&one, "acme").await;
    let views: [(&str, PgBackend, PgBackend); 2] = [
        ("unscoped", one.clone(), two.clone()),
        ("acme", acme, two.for_tenant(TenantId::from("acme"))),
    ];
    for (whose, engine, other) in views {
        for first_refuses in [false, true] {
            let id = new_run(&engine).await;
            let held = claimed(&engine, &id, "step", "worker-1").await;
            let event = completed(&id, "step", json!({ "step": 1 }));
            let first = commit_held(&engine, &held, event.clone(), first_refuses).await;
            assert_eq!(
                first.unwrap_or_else(|e| panic!("{whose}: the first commit: {e}")),
                TurnCommit::Committed(1),
                "{whose}: the first commit"
            );
            let row = item_text(&pool, held.id).await;
            for (replay_refuses, same) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let again = if same {
                    event.clone()
                } else {
                    completed(&id, "step", json!({ "step": 2 }))
                };
                let replay = commit_held(&engine, &held, again, replay_refuses).await;
                assert!(
                    matches!(replay, Err(StateBackendError::FenceLost(_))),
                    "{whose}: a replay after a commit (first refuses {first_refuses}, \
                     replay refuses {replay_refuses}, same event {same}): {replay:?}"
                );
                assert_eq!(
                    kinds_of(&pool, &id).await,
                    vec!["node_completed"],
                    "{whose}: the replay recorded something"
                );
                assert_eq!(
                    item_text(&pool, held.id).await,
                    row,
                    "{whose}: the replay touched the item"
                );
            }
        }

        let id = new_run(&engine).await;
        let held = claimed(&engine, &id, "step", "worker-1").await;
        assert!(matches!(
            cancel(&other, &id).await,
            ExecutionFinish::Finished(_)
        ));
        let event = completed(&id, "step", json!({ "step": 1 }));
        let withdrawn = commit_held(&engine, &held, event.clone(), true).await;
        assert_eq!(
            withdrawn.unwrap_or_else(|e| panic!("{whose}: the withdraw: {e}")),
            TurnCommit::ExecutionTerminal(WorkflowStatus::Cancelled),
            "{whose}: the withdraw"
        );
        let row = item_text(&pool, held.id).await;
        for (replay_refuses, same) in [(false, false), (true, false), (false, true), (true, true)] {
            let again = if same {
                event.clone()
            } else {
                completed(&id, "step", json!({ "step": 2 }))
            };
            let replay = commit_held(&engine, &held, again, replay_refuses).await;
            assert!(
                matches!(replay, Err(StateBackendError::FenceLost(_))),
                "{whose}: a replay after a withdraw (replay refuses {replay_refuses}, \
                 same event {same}): {replay:?}"
            );
            assert_eq!(
                kinds_of(&pool, &id).await,
                vec!["workflow_cancelled"],
                "{whose}: the replay recorded a completion for a withdrawn node"
            );
            assert_eq!(
                item_text(&pool, held.id).await,
                row,
                "{whose}: the replay touched the item"
            );
        }
    }
}
