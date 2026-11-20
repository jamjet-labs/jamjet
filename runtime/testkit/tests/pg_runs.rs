//! Runs and their logs on the Postgres store, where the suite's cases do
//! not reach: the run lock and its timeouts, a connection ended in the
//! middle of a transaction, a scheduler pass of 10,000 events, and the
//! indexed completion count against a scan of the log.
//!
//! A raw connection holds a run's row with `SELECT ... FOR UPDATE`, as a
//! second engine in the middle of a write would. Attempts are counted at
//! the fault point that opens each attempt of a transaction.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::faults::{Fault, Scope};
use jamjet_state::{
    ConditionalAppend, Event, EventKind, PassCommit, PgBackend, PgSettings, SchedulerPass,
    StateBackend, StateBackendError, TenantId,
};
use jamjet_testkit::pg::{admin_url, TestDatabase, POOL_SIZE};
use serde_json::json;
use sqlx::{Connection, PgConnection};
use std::time::{Duration, Instant};

/// The point at the start of every attempt of a Postgres transaction.
const ATTEMPT: &str = "pg_tx.attempt";

fn settings() -> PgSettings {
    PgSettings {
        pool_size: POOL_SIZE,
        ..PgSettings::default()
    }
}

async fn store(db: &TestDatabase, settings: PgSettings) -> PgBackend {
    PgBackend::connect(db.url(), settings)
        .await
        .unwrap_or_else(|e| panic!("connect to the test database: {e}"))
}

fn run(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-pg-runs".into(),
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

fn started(id: &ExecutionId, attempt: u32) -> Event {
    Event::new(
        id.clone(),
        0,
        EventKind::NodeStarted {
            node_id: "step".into(),
            worker_id: "w1".into(),
            attempt,
        },
    )
}

/// A connection of its own on the test database, holding the run's row
/// locked inside an open transaction until it commits or drops.
async fn hold_run(db: &TestDatabase, id: &ExecutionId) -> PgConnection {
    let mut raw = PgConnection::connect(db.url()).await.unwrap();
    sqlx::query("BEGIN").execute(&mut raw).await.unwrap();
    let held: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM workflow_executions WHERE execution_id = $1 FOR UPDATE")
            .bind(id.0)
            .fetch_optional(&mut raw)
            .await
            .unwrap();
    assert_eq!(held, Some(1), "the run's row is there to hold");
    raw
}

/// How many events the run's log holds, read past the store.
async fn rows_of(conn: &mut PgConnection, id: &ExecutionId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE execution_id = $1")
        .bind(id.0)
        .fetch_one(conn)
        .await
        .unwrap()
}

/// A small generator, so the logs are random and the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
    }
}

/// Node ids that differ only a little, so a count that matched loosely
/// would take the wrong ones.
const NODES: [&str; 5] = ["n1", "n1 ", "N1", "n'2", "nœud-3"];

fn random_kind(rng: &mut Rng) -> EventKind {
    let node = NODES[rng.below(NODES.len() as u64) as usize].to_string();
    match rng.below(6) {
        0 | 1 => EventKind::NodeCompleted {
            node_id: node,
            output: json!({ "big": "payload".repeat(rng.below(40) as usize) }),
            state_patch: json!({}),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: None,
        },
        2 => EventKind::NodeScheduled {
            node_id: node,
            queue_type: "general".into(),
        },
        3 => EventKind::NodeStarted {
            node_id: node,
            worker_id: "w".into(),
            attempt: 0,
        },
        4 => EventKind::NodeFailed {
            node_id: node,
            error: "boom".into(),
            attempt: 0,
            retryable: true,
        },
        _ => EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "1".into(),
            initial_input: json!({}),
            initiated_by: None,
        },
    }
}

/// For 50 random logs, the indexed count of a node's completions is what a
/// scan of the log counts, on the unscoped store and through a tenant view.
/// Half the logs are written one append at a time and half in one
/// scheduler pass, so both ways of inserting fill the indexed columns.
#[tokio::test]
async fn count_node_completions_matches_the_scan() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("count_node_completions_matches_the_scan");
    };
    let b = store(&db, settings()).await;
    let scoped = b.for_tenant(TenantId::default());
    let mut rng = Rng(0x5eed_1234_abcd_0042);
    let mut completions = 0;
    for log in 0..50 {
        let id = new_run(&b).await;
        let kinds: Vec<EventKind> = (0..rng.below(40)).map(|_| random_kind(&mut rng)).collect();
        if log % 2 == 0 {
            for kind in kinds {
                b.append_event(Event::new(id.clone(), 0, kind))
                    .await
                    .unwrap();
            }
        } else if !kinds.is_empty() {
            let pass = SchedulerPass {
                execution_id: id.clone(),
                seen_sequence: 0,
                events: kinds,
                work_items: vec![],
                finish: None,
            };
            let out = b.commit_scheduler_pass(pass).await.unwrap();
            assert!(matches!(out, PassCommit::Committed { .. }), "{out:?}");
        }
        for view in [&b, &scoped] {
            let log_events = view.get_events(&id).await.unwrap();
            for node in NODES.iter().chain(&["never-ran"]) {
                let scanned = log_events
                    .iter()
                    .filter(|e| {
                        matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == node)
                    })
                    .count() as u64;
                let counted = view.count_node_completions(&id, node).await.unwrap();
                assert_eq!(counted, scanned, "log {log}, node {node:?}");
                completions += scanned;
            }
        }
    }
    assert!(
        completions > 100,
        "the logs hold completions to count: {completions}"
    );
}

/// One pass of 10,000 skips lands in one attempt of one transaction: every
/// row carries the same inserting transaction id (`xmin`), and the
/// sequences run 1 to 10,000.
#[tokio::test]
async fn a_10000_event_pass_commits_once() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_10000_event_pass_commits_once");
    };
    let b = store(&db, settings()).await;
    let id = new_run(&b).await;
    let events: Vec<EventKind> = (0..10_000)
        .map(|i| EventKind::NodeSkipped {
            node_id: format!("n{i}"),
            reason: "branch not taken".into(),
        })
        .collect();
    let pass = SchedulerPass {
        execution_id: id.clone(),
        seen_sequence: 0,
        events,
        work_items: vec![],
        finish: None,
    };

    let scope = Scope::new();
    scope.arm(ATTEMPT, 1, Fault::Hang(Duration::ZERO));
    let start = Instant::now();
    let out = scope.run(b.commit_scheduler_pass(pass)).await.unwrap();
    let took = start.elapsed();

    assert_eq!(
        out,
        PassCommit::Committed {
            last_sequence: 10_000
        }
    );
    assert_eq!(scope.fired(ATTEMPT, 1), 1, "one attempt");
    let (rows, writers, first, last): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT xmin::text), min(sequence), max(sequence) \
         FROM events WHERE execution_id = $1",
    )
    .bind(id.0)
    .fetch_one(&db.pool().await)
    .await
    .unwrap();
    assert_eq!(
        (rows, writers, first, last),
        (10_000, 1, 1, 10_000),
        "rows, inserting transactions, first and last sequence"
    );
    assert!(took < Duration::from_secs(10), "the pass took {took:?}");
}

/// The running page's cursor is a run id: Postgres compares it as a uuid,
/// so text that is not one is a `Database` error, on the unscoped store and
/// through a tenant view. SQLite compares it as text and answers a page.
#[tokio::test]
async fn a_running_page_after_text_that_is_not_a_uuid_is_a_database_error() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip(
            "a_running_page_after_text_that_is_not_a_uuid_is_a_database_error",
        );
    };
    let b = store(&db, settings()).await;
    new_run(&b).await;
    let scoped = b.for_tenant(TenantId::default());
    for view in [&b, &scoped] {
        let out = view
            .list_running_executions_after(Some("not-a-uuid"), 10)
            .await;
        assert!(
            matches!(&out, Err(StateBackendError::Database(_))),
            "{:?}",
            out.map(|page| page.next_after)
        );
        let page = view.list_running_executions_after(None, 10).await.unwrap();
        assert_eq!(
            page.executions.len(),
            1,
            "a page from the start reads the run"
        );
    }
}

/// While a raw connection holds one run's row, an append to that run waits
/// and an append to another run commits at once: the lock is the run's, not
/// the table's.
#[tokio::test]
async fn append_waits_for_the_run_lock_not_the_table() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("append_waits_for_the_run_lock_not_the_table");
    };
    let b = store(&db, settings()).await;
    let held = new_run(&b).await;
    let free = new_run(&b).await;
    let mut raw = hold_run(&db, &held).await;

    let blocked = tokio::spawn({
        let (b, event) = (b.clone(), started(&held, 0));
        async move { b.append_event(event).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !blocked.is_finished(),
        "an append to the held run waits for its lock"
    );

    let start = Instant::now();
    let seq = b.append_event(started(&free, 0)).await.unwrap();
    let took = start.elapsed();
    assert_eq!(seq, 1);
    assert!(
        took < Duration::from_secs(1),
        "the append to the other run took {took:?}"
    );
    assert_eq!(
        rows_of(&mut raw, &free).await,
        1,
        "the other run's append is committed while the lock is held"
    );
    assert!(!blocked.is_finished(), "the held run's append still waits");

    raw.close().await.unwrap();
    let seq = blocked.await.unwrap().unwrap();
    assert_eq!(seq, 1, "the held run's append lands once the lock is gone");
}

/// With a 200 ms lock timeout, an append to a held run answers `Unavailable`
/// after the lock timeout's two attempts, and writes nothing.
#[tokio::test]
async fn lock_timeout_answers_unavailable_and_writes_nothing() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("lock_timeout_answers_unavailable_and_writes_nothing");
    };
    let b = store(
        &db,
        PgSettings {
            lock_timeout: Duration::from_millis(200),
            ..settings()
        },
    )
    .await;
    let held = new_run(&b).await;
    let mut raw = hold_run(&db, &held).await;

    let scope = Scope::new();
    scope.arm(ATTEMPT, 1, Fault::Hang(Duration::ZERO));
    let start = Instant::now();
    let out = scope.run(b.append_event(started(&held, 0))).await;
    let took = start.elapsed();

    match &out {
        Err(StateBackendError::Unavailable(why))
            if why.contains("lock timeout") && why.contains("gave up after 2 attempts") => {}
        other => panic!("the append answered {other:?}"),
    }
    assert_eq!(scope.fired(ATTEMPT, 1), 2, "a lock timeout is tried twice");
    assert!(
        took >= Duration::from_millis(400),
        "each attempt waited out the lock timeout: {took:?}"
    );
    assert_eq!(rows_of(&mut raw, &held).await, 0, "nothing was written");
    raw.close().await.unwrap();
    assert!(b.get_events(&held).await.unwrap().is_empty());
}

/// With a 200 ms statement timeout and a lock timeout far longer, an append
/// to a held run answers `Unavailable` after one attempt: a statement
/// timeout is not retried.
#[tokio::test]
async fn statement_timeout_answers_unavailable_and_is_not_retried() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("statement_timeout_answers_unavailable_and_is_not_retried");
    };
    let b = store(
        &db,
        PgSettings {
            statement_timeout: Duration::from_millis(200),
            // Longer than the test, so only the statement timeout fires.
            lock_timeout: Duration::from_secs(60),
            ..settings()
        },
    )
    .await;
    let held = new_run(&b).await;
    let mut raw = hold_run(&db, &held).await;

    let scope = Scope::new();
    scope.arm(ATTEMPT, 1, Fault::Hang(Duration::ZERO));
    let start = Instant::now();
    let out = scope.run(b.append_event(started(&held, 0))).await;
    let took = start.elapsed();

    match &out {
        Err(StateBackendError::Unavailable(why)) if why.contains("57014") => {}
        other => panic!("the append answered {other:?}"),
    }
    assert_eq!(scope.fired(ATTEMPT, 1), 1, "one attempt, no retry");
    assert!(
        took >= Duration::from_millis(200) && took < Duration::from_secs(30),
        "the attempt waited out the statement timeout: {took:?}"
    );
    assert_eq!(rows_of(&mut raw, &held).await, 0, "nothing was written");
    raw.close().await.unwrap();
    assert!(b.get_events(&held).await.unwrap().is_empty());
}

/// A conditional append hangs after reading the log, inside its
/// transaction, and the server ends that connection. The store retries a
/// lost connection on a new one, so the hang is armed for every attempt and
/// each attempt's connection is ended in turn: after the third, the call
/// answers `Unavailable` and the log is as it was. The next call lands.
#[tokio::test]
async fn terminated_backend_mid_transaction_writes_nothing() {
    const LABEL: &str = "pg.append_event_if";
    const STEP: u32 = 2;
    const HANG: Duration = Duration::from_millis(1500);
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("terminated_backend_mid_transaction_writes_nothing");
    };
    let mut admin = PgConnection::connect(&admin_url().expect("the admin URL is set"))
        .await
        .unwrap();
    let b = store(&db, settings()).await;
    let id = new_run(&b).await;
    b.append_event(started(&id, 0)).await.unwrap();
    let before: Vec<i64> = b
        .get_events(&id)
        .await
        .unwrap()
        .iter()
        .map(|e| e.sequence)
        .collect();

    let scope = Scope::new();
    scope.arm(LABEL, STEP, Fault::Hang(HANG));
    scope.arm(ATTEMPT, 1, Fault::Hang(Duration::ZERO));
    let call = tokio::spawn({
        let (scope, b, event) = (scope.clone(), b.clone(), started(&id, 1));
        async move {
            let always = |_: &[Event]| true;
            scope.run(b.append_event_if(event, &always)).await
        }
    });

    let mut ended = Vec::new();
    for attempt in 1..=3u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        while scope.fired(LABEL, STEP) < attempt {
            assert!(
                Instant::now() < deadline && !call.is_finished(),
                "attempt {attempt} never reached {LABEL}#{STEP}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let pids: Vec<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity WHERE datname = $1 AND state = 'idle in transaction'",
        )
        .bind(db.name())
        .fetch_all(&mut admin)
        .await
        .unwrap();
        assert_eq!(
            pids.len(),
            1,
            "attempt {attempt}: one connection is hanging"
        );
        let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
            .bind(pids[0])
            .fetch_one(&mut admin)
            .await
            .unwrap();
        assert!(terminated, "attempt {attempt}: its connection was ended");
        ended.push(pids[0]);
    }

    let out = call.await.unwrap();
    match &out {
        Err(StateBackendError::Unavailable(why)) if why.contains("gave up after 3 attempts") => {}
        other => panic!("the conditional append answered {other:?}"),
    }
    assert_eq!(scope.fired(ATTEMPT, 1), 3, "three attempts");
    assert_eq!(scope.fired(LABEL, STEP), 3, "each reached the hang");
    ended.sort_unstable();
    ended.dedup();
    assert_eq!(ended.len(), 3, "each attempt ran on a new connection");
    let after: Vec<i64> = b
        .get_events(&id)
        .await
        .unwrap()
        .iter()
        .map(|e| e.sequence)
        .collect();
    assert_eq!(after, before, "the log is as it was");

    let always = |_: &[Event]| true;
    let next = b.append_event_if(started(&id, 2), &always).await.unwrap();
    assert_eq!(next, ConditionalAppend::Appended(2), "the next call lands");
}
