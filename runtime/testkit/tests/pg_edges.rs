//! Edges of the Postgres store the suite does not reach: a pool with no
//! connection free, a slow sweep beside the claims, and a NUL in an
//! identifier or in a run's JSON. A server restart under the pool is in
//! `api/tests/pg_restart.rs`, which runs alone.
//!
//! What these share: a call that cannot get a connection answers
//! `Unavailable` and never hangs past the acquire timeout, and a value
//! Postgres text cannot hold is refused before it is sent.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::faults::{Fault, Scope};
use jamjet_state::{
    Event, EventKind, PgBackend, PgSettings, StateBackend, StateBackendError, WorkItem,
    DEFAULT_TENANT,
};
use jamjet_testkit::pg::TestDatabase;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The text every NUL refusal answers with.
const NUL_IN_TEXT: &str = "text contains a NUL character";

fn settings(pool_size: u32, acquire_timeout: Duration) -> PgSettings {
    PgSettings {
        pool_size,
        acquire_timeout,
        ..PgSettings::default()
    }
}

async fn store(db: &TestDatabase, settings: PgSettings) -> PgBackend {
    PgBackend::connect(db.url(), settings)
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

fn run_row(id: &ExecutionId, input: Value) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: input.clone(),
        current_state: input,
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn item(run: &ExecutionId, node: &str, queue: &str, max_attempts: u32) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: run.clone(),
        node_id: node.into(),
        queue_type: queue.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

/// Start a run with `input` whose start node `node` is pending on `queue`.
async fn start(
    b: &PgBackend,
    input: Value,
    node: &str,
    queue: &str,
    max_attempts: u32,
) -> Result<ExecutionId, StateBackendError> {
    let id = ExecutionId::new();
    let first = item(&id, node, queue, max_attempts);
    b.start_execution_atomic(
        run_row(&id, input.clone()),
        EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: input,
            initiated_by: None,
            run_key: None,
        },
        EventKind::NodeScheduled {
            node_id: node.into(),
            queue_type: queue.into(),
        },
        first,
    )
    .await?;
    Ok(id)
}

/// The rows of the run, event and item tables, in that order.
async fn counts(pool: &PgPool) -> (i64, i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM workflow_executions), (SELECT COUNT(*) FROM events), \
                (SELECT COUNT(*) FROM work_items)",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// With both connections of a pool of two held elsewhere, a claim waits for
/// the 500 ms acquire timeout, not longer, and answers `Unavailable`. Once
/// they are back, the same claim takes the item.
#[tokio::test]
async fn exhausted_pool_answers_unavailable_within_the_acquire_timeout() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip(
            "exhausted_pool_answers_unavailable_within_the_acquire_timeout",
        );
    };
    let b = store(&db, settings(2, Duration::from_millis(500))).await;
    start(&b, json!({}), "begin", "work", 3).await.unwrap();

    let pool = b.pool();
    let held = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    let began = Instant::now();
    let out = b.claim_work_item("w1", &["work"]).await;
    let waited = began.elapsed();
    assert!(
        matches!(out, Err(StateBackendError::Unavailable(_))),
        "a claim with no connection free: {out:?}"
    );
    assert!(
        (Duration::from_millis(400)..=Duration::from_millis(1500)).contains(&waited),
        "the claim answered after {waited:?}, for a 500 ms acquire timeout"
    );

    drop(held);
    let claimed = b.claim_work_item("w1", &["work"]).await.unwrap();
    assert_eq!(
        claimed.map(|i| i.node_id),
        Some("begin".to_string()),
        "the claim once the connections were back"
    );
}

/// On a pool of four, the scheduler's sweep dead-letters ten lapsed items,
/// one run each, and holds each run for a second inside its transaction.
/// Claims on another queue meanwhile answer each in under 1.5 s: the sweep
/// takes one connection and one run at a time, and a claim never waits for
/// a run the sweep holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sweep_does_not_starve_claims() {
    const RUNS: usize = 10;
    const HANG: Duration = Duration::from_secs(1);
    const LABEL: &str = "pg.reclaim_expired_leases_recorded";
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("the_sweep_does_not_starve_claims");
    };
    let b = store(&db, settings(4, Duration::from_secs(5))).await;
    let pool = db.pool().await;

    // Ten runs whose one item is out of attempts once its lease lapses, so
    // the claim's fast reclaim leaves it to the sweep.
    for _ in 0..RUNS {
        start(&b, json!({}), "slow", "sweep", 1).await.unwrap();
        assert!(b
            .claim_work_item("w-old", &["sweep"])
            .await
            .unwrap()
            .is_some());
    }
    sqlx::query(
        "UPDATE work_items SET lease_expires_at = statement_timestamp() - interval '1 minute' \
         WHERE queue_type = 'sweep'",
    )
    .execute(&pool)
    .await
    .unwrap();
    // Work for the claims, more than they can take while the sweep runs.
    let work = start(&b, json!({}), "w-0", "work", 3).await.unwrap();
    for n in 1..200 {
        b.enqueue_work_item(item(&work, &format!("w-{n}"), "work", 3))
            .await
            .unwrap();
    }

    let scope = Scope::new();
    scope.arm(LABEL, 1, Fault::Hang(HANG));
    let sweep = tokio::spawn({
        let (scope, b) = (scope.clone(), b.clone());
        async move {
            let began = Instant::now();
            let out = scope.run(b.reclaim_expired_leases_recorded()).await;
            (out, began.elapsed())
        }
    });

    let mut slowest = Duration::ZERO;
    let mut claims = 0;
    while !sweep.is_finished() {
        let began = Instant::now();
        let claimed = b.claim_work_item("w-new", &["work"]).await.unwrap();
        let took = began.elapsed();
        assert!(claimed.is_some(), "claim {claims} found no work");
        assert!(
            took < Duration::from_millis(1500),
            "claim {claims} took {took:?} while the sweep ran"
        );
        slowest = slowest.max(took);
        claims += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (out, swept_in) = sweep.await.unwrap();
    let out = out.unwrap();
    assert_eq!(out.exhausted.len(), RUNS, "the sweep dead-lettered {out:?}");
    assert_eq!(
        scope.fired(LABEL, 1),
        RUNS as u64,
        "the sweep hung once per run"
    );
    assert!(
        swept_in >= HANG * RUNS as u32,
        "the sweep took {swept_in:?}, so it did not hang in every run"
    );
    assert!(
        claims >= 20,
        "only {claims} claims ran during a {swept_in:?} sweep"
    );
    println!("{claims} claims during a {swept_in:?} sweep, the slowest {slowest:?}");
}

/// A node id with a NUL that reaches the store directly, past the IR
/// validation that refuses it at the API, is refused before anything is
/// sent: the answer comes even with no connection free, and nothing is
/// written.
#[tokio::test]
async fn nul_in_an_identifier_is_refused_cleanly() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("nul_in_an_identifier_is_refused_cleanly");
    };
    let b = store(&db, settings(2, Duration::from_millis(500))).await;
    let pool = db.pool().await;
    let run = start(&b, json!({}), "begin", "work", 3).await.unwrap();
    let before = counts(&pool).await;

    let bad = "step\0one";
    let pool_of_b = b.pool();
    let held = (
        pool_of_b.acquire().await.unwrap(),
        pool_of_b.acquire().await.unwrap(),
    );
    let calls: Vec<(&str, Result<(), StateBackendError>)> = vec![
        (
            "enqueue_work_item",
            b.enqueue_work_item(item(&run, bad, "work", 3))
                .await
                .map(drop),
        ),
        (
            "append_event",
            b.append_event(Event::new(
                run.clone(),
                0,
                EventKind::NodeStarted {
                    node_id: bad.into(),
                    worker_id: "w1".into(),
                    attempt: 0,
                },
            ))
            .await
            .map(drop),
        ),
        (
            "start_execution_atomic",
            start(&b, json!({}), bad, "work", 3).await.map(drop),
        ),
    ];
    drop(held);
    for (call, out) in calls {
        assert!(
            matches!(&out, Err(StateBackendError::Database(m)) if m == NUL_IN_TEXT),
            "{call} with a NUL in the node id: {out:?}"
        );
    }
    assert_eq!(counts(&pool).await, before, "a refused call wrote a row");
}

/// A NUL inside a string of a run's input and state, which serde writes as
/// `\u0000`, is stored and reads back equal: in the run's row, in its
/// `WorkflowStarted`, and in a snapshot of its state.
#[tokio::test]
async fn a_run_input_with_nul_starts_and_reads_back() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_run_input_with_nul_starts_and_reads_back");
    };
    let b = store(&db, settings(3, Duration::from_secs(5))).await;
    let input = json!({ "text": "before\u{0000}after", "list": ["\u{0000}"], "n": 1 });
    let run = start(&b, input.clone(), "begin", "work", 3).await.unwrap();

    let stored = b.get_execution(&run).await.unwrap().expect("the run");
    assert_eq!(stored.initial_input, input);
    assert_eq!(stored.current_state, input);
    let events = b.get_events(&run).await.unwrap();
    assert!(
        matches!(&events[0].kind, EventKind::WorkflowStarted { initial_input, .. } if *initial_input == input),
        "{:?}",
        events[0].kind
    );

    let state = json!({ "text": "\u{0000}", "input": input });
    b.update_execution_current_state(&run, &state)
        .await
        .unwrap();
    assert_eq!(
        b.get_execution(&run).await.unwrap().unwrap().current_state,
        state
    );
}

/// The audit log on a pool whose one connection is held elsewhere: a read
/// answers `Unavailable` within the acquire timeout, not a database error,
/// so `GET /audit` can answer 503 as the store's routes do.
#[tokio::test]
async fn an_audit_read_with_no_connection_free_answers_unavailable() {
    use jamjet_audit::{AuditBackend, AuditError, AuditQuery, PgAuditBackend};
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("an_audit_read_with_no_connection_free_answers_unavailable");
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(300))
        .connect_with(db.connect_options())
        .await
        .unwrap();
    let audit = PgAuditBackend::new(pool.clone());
    let query = AuditQuery {
        limit: 10,
        ..AuditQuery::default()
    };
    assert!(
        audit.query(&query).await.unwrap().is_empty(),
        "the log reads"
    );
    let held = pool.acquire().await.unwrap();
    let began = Instant::now();
    let read = audit.query(&query).await;
    assert!(
        matches!(read, Err(AuditError::Unavailable(_))),
        "a read with no connection free: {read:?}"
    );
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    let counted = audit.count(&query).await;
    assert!(
        matches!(counted, Err(AuditError::Unavailable(_))),
        "a count with no connection free: {counted:?}"
    );
    drop(held);
    assert!(audit.query(&query).await.is_ok(), "the log reads again");
}
