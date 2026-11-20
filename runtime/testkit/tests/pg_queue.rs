//! The queue and its leases on the Postgres store, where the suite's cases
//! do not reach: a claim that skips rows another transaction holds, many
//! claimers on one queue, leases and parks on the database's clock while
//! this process's clock is an hour wrong, a fast reclaim that skips a run or
//! an item someone holds and leaves a lease granted after its scan, a run's
//! open items, and a mixed load that must not deadlock.
//!
//! A raw connection holds a row with `SELECT ... FOR UPDATE`, as a second
//! engine in the middle of a write would. The clock cases run again in a
//! child of this test binary whose process clock is an hour ahead
//! (`JAMJET_TEST_CLOCK_SKEW_SECS`), since the skew is read once per process.

use chrono::{DateTime, Utc};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::faults::{Fault, Scope};
use jamjet_state::{
    ClaimFilter, Event, EventKind, ExecutionClaim, FailOutcome, GateMark, LeasedItem, PassCommit,
    PgBackend, PgSettings, SchedulerPass, StateBackend, StateBackendError, TenantId, TurnCommit,
    WorkItem, WorkItemId, DEFAULT_TENANT,
};
use jamjet_testkit::conformance::{
    Fixture, PostgresFactory, PostgresScopedFactory, SqliteFactory, SqliteScopedFactory,
    StoreFactory,
};
use jamjet_testkit::pg::{admin_url, TestDatabase, POOL_SIZE};
use serde_json::json;
use sqlx::{Connection, PgConnection, PgPool};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const QUEUE: &str = "general";

/// The point at the start of every attempt of a Postgres transaction.
const ATTEMPT: &str = "pg_tx.attempt";
/// The point before every retry of a Postgres transaction.
const RETRY: &str = "pg_tx.retry";

/// The variable that moves this process's clock, under the `test-hooks`
/// feature, and how far the clock cases move it.
const SKEW_ENV: &str = "JAMJET_TEST_CLOCK_SKEW_SECS";
const SKEW_SECS: i64 = 3600;
/// What a child prints once it has run a clock case's body.
const CHILD_RAN: &str = "ran with the process clock an hour ahead";

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
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-pg-queue".into(),
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

/// A pending item of node `node` of the run, on `queue`.
fn item(id: &ExecutionId, node: &str, queue: &str) -> WorkItem {
    WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: id.clone(),
        node_id: node.into(),
        queue_type: queue.into(),
        payload: json!({ "node_id": node }),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: DEFAULT_TENANT.into(),
    }
}

async fn enqueue(b: &dyn StateBackend, id: &ExecutionId, node: &str, queue: &str) -> WorkItemId {
    b.enqueue_work_item(item(id, node, queue)).await.unwrap()
}

/// A connection of its own on the test database, inside an open
/// transaction that holds the row `select` locks, until it closes.
async fn hold(db: &TestDatabase, select: &'static str, id: uuid::Uuid) -> PgConnection {
    let mut raw = PgConnection::connect(db.url()).await.unwrap();
    sqlx::query("BEGIN").execute(&mut raw).await.unwrap();
    let held: Option<i32> = sqlx::query_scalar(select)
        .bind(id)
        .fetch_optional(&mut raw)
        .await
        .unwrap();
    assert_eq!(held, Some(1), "the row is there to hold");
    raw
}

async fn hold_run(db: &TestDatabase, id: &ExecutionId) -> PgConnection {
    hold(
        db,
        "SELECT 1 FROM workflow_executions WHERE execution_id = $1 FOR UPDATE",
        id.0,
    )
    .await
}

async fn hold_item(db: &TestDatabase, id: WorkItemId) -> PgConnection {
    hold(db, "SELECT 1 FROM work_items WHERE id = $1 FOR UPDATE", id).await
}

/// The item's status, attempt and fence, read past the store.
async fn row_of(pool: &PgPool, id: WorkItemId) -> (String, i32, i64) {
    sqlx::query_as("SELECT status, attempt, lease_fence FROM work_items WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// End the item's lease a second ago by the database's clock.
async fn expire_by_the_database_clock(pool: &PgPool, id: WorkItemId) {
    let done = sqlx::query(
        "UPDATE work_items SET lease_expires_at = statement_timestamp() - interval '1 second' \
         WHERE id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(done.rows_affected(), 1, "no item {id}");
}

/// The database's time now.
async fn database_now(pool: &PgPool) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT statement_timestamp()")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The kinds of the run's events, as their `type` tags.
async fn kinds_of(b: &dyn StateBackend, id: &ExecutionId) -> Vec<String> {
    b.get_events(id)
        .await
        .unwrap()
        .iter()
        .map(|e| {
            serde_json::to_value(&e.kind).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

/// In the parent, run `test` again in a child of this binary whose process
/// clock is an hour ahead, require that it ran the case's body and passed,
/// and answer `false`. In the child, answer `true`: the caller runs the
/// body. Without a Postgres server the case skips here.
fn in_a_skewed_child(test: &str) -> bool {
    if std::env::var_os(SKEW_ENV).is_some() {
        return true;
    }
    if admin_url().is_none() {
        jamjet_testkit::skip(test);
        return false;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", test, "--nocapture", "--test-threads", "1"])
        .env(SKEW_ENV, SKEW_SECS.to_string())
        .output()
        .expect("run the child");
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.status.success(),
        "the child failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(CHILD_RAN),
        "the child did not run the case's body:\n{stdout}\n{stderr}"
    );
    false
}

/// In a child, the process clock is the hour ahead the parent set.
fn assert_the_clock_is_an_hour_ahead() {
    let ahead = jamjet_state::clock::process_now() - Utc::now();
    assert!(
        (ahead.num_seconds() - SKEW_SECS).abs() <= 1,
        "the process clock is {ahead} ahead"
    );
}

/// A lock timeout far under the test's own time, so a claim that waited on
/// a held row would fail with it rather than hang.
fn short_lock_timeout() -> PgSettings {
    PgSettings {
        lock_timeout: Duration::from_secs(2),
        ..settings()
    }
}

/// While a raw connection holds the oldest pending item, a claim takes the
/// next one at once. Released, the oldest is claimed next.
#[tokio::test]
async fn claims_skip_locked_rows_and_never_wait() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("claims_skip_locked_rows_and_never_wait");
    };
    let b = store(&db, short_lock_timeout()).await;
    let pool = db.pool().await;
    let id = new_run(&b).await;
    let oldest = enqueue(&b, &id, "a", QUEUE).await;
    let second = enqueue(&b, &id, "b", QUEUE).await;
    let raw = hold_item(&db, oldest).await;

    let start = Instant::now();
    let claimed = b
        .claim_work_item("w1", &[QUEUE])
        .await
        .unwrap()
        .expect("an item is claimable");
    let took = start.elapsed();
    assert_eq!(claimed.id, second, "the claim took the item nobody holds");
    assert!(
        took < Duration::from_millis(500),
        "the claim waited for the held row: {took:?}"
    );
    assert_eq!(row_of(&pool, oldest).await.0, "pending");

    raw.close().await.unwrap();
    let next = b
        .claim_work_item("w2", &[QUEUE])
        .await
        .unwrap()
        .expect("the oldest is claimable once released");
    assert_eq!(next.id, oldest);
}

/// Eight claimers on two pools drain 200 items: each item is claimed once,
/// none is left pending, in under 10 s, and the claims ran at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_claimers_take_two_hundred_items_once_each() {
    const RUNS: usize = 10;
    const PER_RUN: usize = 20;
    const CLAIMERS: usize = 8;
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("eight_claimers_take_two_hundred_items_once_each");
    };
    let engine = || async {
        Arc::new(
            store(
                &db,
                PgSettings {
                    pool_size: 4,
                    ..settings()
                },
            )
            .await,
        )
    };
    let engines = [engine().await, engine().await];
    let mut queued = BTreeSet::new();
    for _ in 0..RUNS {
        let id = new_run(&*engines[0]).await;
        for n in 0..PER_RUN {
            queued.insert(enqueue(&*engines[0], &id, &format!("n{n}"), QUEUE).await);
        }
    }

    let in_flight = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();
    let claimers: Vec<_> = (0..CLAIMERS)
        .map(|c| {
            let (b, in_flight, most) = (engines[c % 2].clone(), in_flight.clone(), most.clone());
            tokio::spawn(async move {
                let mut mine = Vec::new();
                loop {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    let claimed = b.claim_work_item(&format!("w{c}"), &[QUEUE]).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    match claimed.unwrap() {
                        Some(item) => mine.push(item.id),
                        None => return mine,
                    }
                }
            })
        })
        .collect();
    let mut claimed = Vec::new();
    for c in claimers {
        claimed.extend(c.await.unwrap());
    }
    let took = start.elapsed();

    let once: BTreeSet<WorkItemId> = claimed.iter().copied().collect();
    assert_eq!(claimed.len(), queued.len(), "one claim per item");
    assert_eq!(once, queued, "every item claimed, none twice");
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_items WHERE status = 'pending'")
            .fetch_one(&db.pool().await)
            .await
            .unwrap();
    assert_eq!(pending, 0, "none left pending");
    assert!(took < Duration::from_secs(10), "the claims took {took:?}");
    assert!(
        most.load(Ordering::SeqCst) >= 2,
        "the claims never ran at once"
    );
}

/// The process clock is an hour ahead. A 30 s lease, by the database's
/// clock, is not reclaimed by the sweeps or by a claim's fast reclaim. Once
/// the database's clock passes it, the next poll reclaims it, and so does
/// the sweep.
#[tokio::test]
async fn lease_expiry_follows_the_database_clock() {
    if !in_a_skewed_child("lease_expiry_follows_the_database_clock") {
        return;
    }
    assert_the_clock_is_an_hour_ahead();
    let db = TestDatabase::fresh()
        .await
        .expect("the parent saw a Postgres URL");
    let b = store(&db, settings()).await;
    let pool = db.pool().await;
    let id = new_run(&b).await;
    let item = enqueue(&b, &id, "a", QUEUE).await;
    let held = b
        .claim_work_item_with_lease("w1", &[QUEUE], Some(30))
        .await
        .unwrap()
        .expect("claimable");
    let left: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM lease_expires_at - statement_timestamp())::float8 \
         FROM work_items WHERE id = $1",
    )
    .bind(item)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        (28.0..=30.0).contains(&left),
        "the lease has {left} s left by the database's clock"
    );

    let swept = b.reclaim_expired_leases().await.unwrap();
    assert!(swept.retryable.is_empty() && swept.exhausted.is_empty());
    let swept = b.reclaim_expired_leases_recorded().await.unwrap();
    assert!(swept.retryable.is_empty() && swept.exhausted.is_empty());
    assert!(
        b.claim_work_item("w2", &[QUEUE]).await.unwrap().is_none(),
        "a claim's fast reclaim took a lease the database's clock says is live"
    );
    let listed = b.list_claimed_work_items(10).await.unwrap();
    assert_eq!(
        listed.iter().map(|c| c.work_item_id).collect::<Vec<_>>(),
        vec![item]
    );
    assert_eq!(
        row_of(&pool, item).await,
        ("claimed".to_string(), 0, held.lease_fence),
        "the lease is the first claim's still"
    );

    expire_by_the_database_clock(&pool, item).await;
    let again = b
        .claim_work_item("w2", &[QUEUE])
        .await
        .unwrap()
        .expect("the next poll reclaims a lapsed lease");
    assert_eq!((again.id, again.attempt), (item, 1));

    expire_by_the_database_clock(&pool, item).await;
    let swept = b.reclaim_expired_leases_recorded().await.unwrap();
    let ids: Vec<WorkItemId> = swept.retryable.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![item], "the sweep reclaims a lapsed lease");
    println!("{CHILD_RAN}");
}

/// The process clock is an hour ahead. A park until 5 s from now by this
/// process's clock makes the item claimable 5 s from now by the database's.
#[tokio::test]
async fn park_applies_the_delay_to_the_database_clock() {
    if !in_a_skewed_child("park_applies_the_delay_to_the_database_clock") {
        return;
    }
    assert_the_clock_is_an_hour_ahead();
    let db = TestDatabase::fresh()
        .await
        .expect("the parent saw a Postgres URL");
    let b = store(&db, settings()).await;
    let pool = db.pool().await;
    let id = new_run(&b).await;
    let item = enqueue(&b, &id, "a", QUEUE).await;
    let held = b
        .claim_work_item("w1", &[QUEUE])
        .await
        .unwrap()
        .expect("claimable");

    let retry_after = jamjet_state::clock::process_now() + chrono::Duration::seconds(5);
    assert!(b
        .park_work_item(item, held.lease_fence, &retry_after.to_rfc3339(), 1)
        .await
        .unwrap());
    let parked_at = database_now(&pool).await;
    let due: DateTime<Utc> = sqlx::query_scalar("SELECT retry_after FROM work_items WHERE id = $1")
        .bind(item)
        .fetch_one(&pool)
        .await
        .unwrap();
    let delay = (due - parked_at).num_milliseconds();
    assert!(
        (4_000..=6_000).contains(&delay),
        "due {delay} ms after the park by the database's clock"
    );

    let claimed_at = loop {
        let claim = b.claim_work_item("w2", &[QUEUE]).await.unwrap();
        let now = database_now(&pool).await;
        let waited = (now - parked_at).num_milliseconds();
        match claim {
            Some(c) => {
                assert_eq!((c.id, c.attempt), (item, 1));
                break waited;
            }
            None => assert!(waited < 7_000, "never claimable: {waited} ms"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        (4_000..=6_500).contains(&claimed_at),
        "claimable {claimed_at} ms after the park by the database's clock"
    );
    println!("{CHILD_RAN}");
}

/// While a raw connection holds run A's row, a claim on another queue
/// returns at once with that queue's item, and leaves A's lapsed lease for
/// the next poll, which reclaims it once A is released.
#[tokio::test]
async fn fast_reclaim_skips_a_busy_run() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("fast_reclaim_skips_a_busy_run");
    };
    let b = store(&db, short_lock_timeout()).await;
    let pool = db.pool().await;
    let (a, other) = (new_run(&b).await, new_run(&b).await);
    let lapsed = enqueue(&b, &a, "a", "queue-a").await;
    b.claim_work_item("w1", &["queue-a"])
        .await
        .unwrap()
        .expect("claimable");
    expire_by_the_database_clock(&pool, lapsed).await;
    let waiting = enqueue(&b, &other, "b", "queue-b").await;
    let raw = hold_run(&db, &a).await;

    let start = Instant::now();
    let claimed = b.claim_work_item("w2", &["queue-b"]).await.unwrap();
    let took = start.elapsed();
    assert_eq!(claimed.map(|c| c.id), Some(waiting));
    assert!(
        took < Duration::from_millis(500),
        "the claim waited for the held run: {took:?}"
    );
    assert_eq!(row_of(&pool, lapsed).await.0, "claimed", "A's lease waits");
    assert!(kinds_of(&b, &a).await.is_empty());

    raw.close().await.unwrap();
    assert!(b
        .claim_work_item("w2", &["queue-b"])
        .await
        .unwrap()
        .is_none());
    assert_eq!(row_of(&pool, lapsed).await.0, "pending", "reclaimed");
    assert_eq!(row_of(&pool, lapsed).await.1, 1, "an attempt spent");
    assert_eq!(kinds_of(&b, &a).await, ["node_failed", "retry_scheduled"]);
}

/// While a raw connection holds a lapsed item's row (not its run's), a
/// claim on another queue returns at once and leaves that item for the next
/// poll, which reclaims it once the row is released.
#[tokio::test]
async fn fast_reclaim_skips_a_held_item() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("fast_reclaim_skips_a_held_item");
    };
    let b = store(&db, short_lock_timeout()).await;
    let pool = db.pool().await;
    let a = new_run(&b).await;
    let lapsed = enqueue(&b, &a, "a", "queue-a").await;
    b.claim_work_item("w1", &["queue-a"])
        .await
        .unwrap()
        .expect("claimable");
    expire_by_the_database_clock(&pool, lapsed).await;
    let raw = hold_item(&db, lapsed).await;

    let start = Instant::now();
    assert!(b
        .claim_work_item("w2", &["queue-b"])
        .await
        .unwrap()
        .is_none());
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(500),
        "the claim waited for the held item: {took:?}"
    );
    assert_eq!(row_of(&pool, lapsed).await.0, "claimed", "the item waits");

    raw.close().await.unwrap();
    assert!(b
        .claim_work_item("w2", &["queue-b"])
        .await
        .unwrap()
        .is_none());
    assert_eq!(row_of(&pool, lapsed).await.0, "pending", "reclaimed");
}

/// How long the first engine's claim waits between its scan and its write.
/// Everything the second engine does in the case below lands inside it.
const SCAN_TO_WRITE: Duration = Duration::from_secs(2);

/// Wait until a call in `scope` reached `label#1`.
async fn until_reached(scope: &Scope, label: &'static str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while scope.fired(label, 1) == 0 {
        assert!(Instant::now() < deadline, "{what}: never reached {label}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// The fast reclaim scans for lapsed leases without a lock and writes
/// later, run by run. Engine A's claim scans, finds item X's lease lapsed,
/// and waits before its write. Meanwhile engine B's claim reclaims X and
/// takes it under a new fence. A's write must leave X to B, as it must when
/// X's run ended in between, where the write settles the run's lapsed items
/// failed. On the unscoped store and through a tenant's view.
#[tokio::test]
async fn fast_reclaim_leaves_a_lease_granted_after_its_scan() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("fast_reclaim_leaves_a_lease_granted_after_its_scan");
    };
    let pool = db.pool().await;
    sqlx::query("INSERT INTO tenants (id, name) VALUES ('acme', 'acme')")
        .execute(&pool)
        .await
        .unwrap();
    let (engine_a, engine_b) = (store(&db, settings()).await, store(&db, settings()).await);
    for (view, ended) in [
        ("unscoped", false),
        ("unscoped", true),
        ("acme", false),
        ("acme", true),
    ] {
        let case = format!("{view}, run ended in between: {ended}");
        let (a, b) = match view {
            "unscoped" => (engine_a.clone(), engine_b.clone()),
            tenant => (
                engine_a.for_tenant(TenantId::from(tenant)),
                engine_b.for_tenant(TenantId::from(tenant)),
            ),
        };
        let run = new_run(&b).await;
        let queue = format!("lapsed-{}", run.0.simple());
        let x = enqueue(&b, &run, "x", &queue).await;
        b.claim_work_item("w1", &[queue.as_str()])
            .await
            .unwrap()
            .expect("claimable");
        expire_by_the_database_clock(&pool, x).await;

        // A's wait sits at the start of its transaction for X's run, after
        // the scan and before the run's lock.
        let scope = Scope::new();
        scope.arm(ATTEMPT, 1, Fault::Hang(SCAN_TO_WRITE));
        let start = Instant::now();
        let first = tokio::spawn({
            let (scope, a) = (scope.clone(), a.clone());
            async move {
                scope
                    .run(a.claim_work_item("w-a", &["queue-of-nothing"]))
                    .await
            }
        });
        until_reached(&scope, ATTEMPT, &case).await;
        let taken = b
            .claim_work_item("w2", &[queue.as_str()])
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{case}: B's claim reclaims X and takes it"));
        assert_eq!((taken.id, taken.attempt), (x, 1), "{case}");
        if ended {
            b.update_execution_status(&run, WorkflowStatus::Cancelled)
                .await
                .unwrap();
        }
        assert!(
            start.elapsed() < SCAN_TO_WRITE,
            "{case}: B took {:?}, longer than A's wait, so A may have written first",
            start.elapsed()
        );

        let a_took = first.await.unwrap().unwrap();
        assert!(a_took.is_none(), "{case}: A's queue is empty");
        assert_eq!(
            row_of(&pool, x).await,
            ("claimed".to_string(), 1, taken.lease_fence),
            "{case}: X is still B's"
        );
        let held = b.get_work_item(x).await.unwrap().expect("X is there");
        assert_eq!(held.worker_id.as_deref(), Some("w2"), "{case}");
        assert!(
            held.lease_expires_at.expect("a lease") > database_now(&pool).await,
            "{case}: B's lease is live"
        );
        assert_eq!(
            kinds_of(&b, &run).await,
            ["node_failed", "retry_scheduled"],
            "{case}: only B's reclaim recorded the lapse"
        );
    }
}

/// A run's pending and claimed items, oldest first, on the unscoped store
/// and through its tenant's view; another tenant's view and an unknown run
/// have none. The view judges by the run's row: an item whose own tenant
/// column says `default`, as items queued by an earlier build do, is still
/// the run's tenant's.
#[tokio::test]
async fn open_work_items_lists_pending_and_claimed_only() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("open_work_items_lists_pending_and_claimed_only");
    };
    let pool = db.pool().await;
    for tenant in ["acme", "globex"] {
        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $1)")
            .bind(tenant)
            .execute(&pool)
            .await
            .unwrap();
    }
    let base = store(&db, settings()).await;
    let acme = base.for_tenant(TenantId::from("acme"));
    let globex = base.for_tenant(TenantId::from("globex"));
    let exec = new_run(&acme).await;
    let other = new_run(&acme).await;
    let t0 = Utc::now();
    let mut items = Vec::new();
    for (n, (node, queue)) in [
        ("waiting", "python_fn"),
        ("held", "held_q"),
        ("done", "done_q"),
        ("failed", "failed_q"),
        ("dead", "dead_q"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut i = item(&exec, node, queue);
        i.created_at = t0 + chrono::Duration::seconds(n as i64);
        items.push(i);
    }
    let mut elsewhere = item(&other, "waiting", "python_fn");
    elsewhere.created_at = t0 + chrono::Duration::seconds(5);
    for i in items.iter().chain([&elsewhere]) {
        base.enqueue_work_item(i.clone()).await.unwrap();
    }
    let (waiting, held, done, failed, dead) = (
        items[0].id,
        items[1].id,
        items[2].id,
        items[3].id,
        items[4].id,
    );
    // An item of acme's run whose own tenant column says `default`.
    let mut old = item(&exec, "old", "old_q");
    old.created_at = t0 + chrono::Duration::seconds(6);
    sqlx::query(
        "INSERT INTO work_items (id, execution_id, tenant_id, node_id, queue_type, payload_json, \
         created_at) VALUES ($1, $2, 'default', $3, $4, '{}', $5)",
    )
    .bind(old.id)
    .bind(exec.0)
    .bind(&old.node_id)
    .bind(&old.queue_type)
    .bind(old.created_at)
    .execute(&pool)
    .await
    .unwrap();

    let claimed = base
        .claim_work_item_with_lease("worker-1", &["held_q"], Some(60))
        .await
        .unwrap()
        .expect("the held item is claimable");
    assert_eq!(claimed.id, held);
    let settled = base
        .claim_work_item_with_lease("worker-1", &["done_q"], Some(60))
        .await
        .unwrap()
        .expect("the done item is claimable");
    assert_eq!(settled.id, done);
    assert!(base
        .complete_work_item_fenced(settled.id, settled.lease_fence)
        .await
        .unwrap());
    base.fail_work_item(failed, "boom").await.unwrap();
    base.move_to_dead_letter(dead, "attempts spent")
        .await
        .unwrap();

    for (who, view) in [("unscoped", &base), ("acme", &acme)] {
        let open = view.open_work_items(&exec).await.unwrap();
        let got: Vec<_> = open
            .iter()
            .map(|o| (o.id, o.node_id.as_str(), o.queue_type.as_str(), o.claimed))
            .collect();
        assert_eq!(
            got,
            vec![
                (waiting, "waiting", "python_fn", false),
                (held, "held", "held_q", true),
                (old.id, "old", "old_q", false),
            ],
            "{who}"
        );
        assert!(open[0].lease_expires_at.is_none(), "{who}");
        let lease = open[1].lease_expires_at.expect("the held item has a lease");
        assert_eq!(lease, claimed.lease_expires_at.unwrap(), "{who}");
    }
    assert!(globex.open_work_items(&exec).await.unwrap().is_empty());
    assert!(base
        .open_work_items(&ExecutionId::new())
        .await
        .unwrap()
        .is_empty());
}

// ── Claims by run and the gate's marks ───────────────────────────────────────

/// A leased item as a step of the scenario reports it: its node, its gate
/// mark and its lease length.
fn leases(mut leased: Vec<LeasedItem>) -> Vec<(String, GateMark, u64)> {
    leased.sort_by(|a, b| a.item.node_id.cmp(&b.item.node_id));
    leased
        .into_iter()
        .map(|l| (l.item.node_id, l.gate, l.lease_secs))
        .collect()
}

/// One store's answers, step by step, to claims that name a run or not,
/// the gate's marks and their compare-and-swap, a release that renews the
/// lease, the claim for a run (the trait's default over these methods), and
/// a park that marks an item claimable only by its run. Each step is also
/// checked on its own.
async fn claims_by_run_and_gates(fx: &Fixture) -> Vec<String> {
    let b = fx.backend.as_ref();
    let q = "by-run";
    let run = new_run(b).await;
    let other = new_run(b).await;
    let mut own = item(&run, "own", q);
    own.payload = json!({ "node_id": "own", "claim": "by_execution" });
    let own_id = b.enqueue_work_item(own).await.unwrap();
    let plain_id = enqueue(b, &run, "plain", q).await;
    let by_run = ClaimFilter {
        execution_id: Some(run.clone()),
        node_id: None,
    };
    let by_node = ClaimFilter {
        execution_id: Some(run.clone()),
        node_id: Some("own".into()),
    };
    let by_other = ClaimFilter {
        execution_id: Some(other.clone()),
        node_id: None,
    };
    let all = ClaimFilter::default();
    let mut said = Vec::new();

    // A claim that names no run never takes the item only its run may
    // claim, though that item is older.
    let plain = b
        .claim_work_item("w1", &[q])
        .await
        .unwrap()
        .expect("the plain item");
    assert_eq!(plain.id, plain_id, "{:?}", fx.kind);
    assert!(b.claim_work_item("w1", &[q]).await.unwrap().is_none());
    for (what, queues, filter) in [
        ("run", &[q][..], &by_run),
        ("other run", &[q][..], &by_other),
        ("no run", &[q][..], &all),
        ("no queue", &[][..], &by_run),
    ] {
        let probe = b.has_claimable(queues, filter).await.unwrap();
        said.push(format!("has_claimable, {what}: {probe}"));
    }

    // A claim that names the run takes it, and its new lease is gating.
    let held = b
        .claim_work_item_filtered("w2", &[q], Some(60), &by_node)
        .await
        .unwrap()
        .expect("the run's own item");
    assert_eq!(held.id, own_id, "{:?}", fx.kind);
    let fence = held.lease_fence;
    for (what, filter) in [
        ("run", &by_run),
        ("node", &by_node),
        ("other run", &by_other),
        ("no run", &all),
    ] {
        let leased = leases(b.leased_items(&[q], filter).await.unwrap());
        said.push(format!("leased, {what}: {leased:?}"));
    }
    assert_eq!(
        leases(b.leased_items(&[q], &by_node).await.unwrap()),
        vec![("own".to_string(), GateMark::Gating, 60)],
        "{:?}",
        fx.kind
    );

    // The marks move by compare-and-swap, under the lease's fence only.
    use GateMark::{Gating, Released, Undecided};
    for (from, to) in [(Gating, Released), (Gating, Released), (Undecided, Gating)] {
        let swapped = b.swap_gate(own_id, fence, from, to).await.unwrap();
        said.push(format!("swap {from:?} to {to:?}: {swapped}"));
    }
    said.push(format!(
        "mark undecided: {}",
        b.mark_gate(own_id, fence, Undecided).await.unwrap()
    ));
    said.push(format!(
        "mark under another fence: {}",
        b.mark_gate(own_id, fence + 1, Gating).await.unwrap()
    ));
    for (from, to) in [(Undecided, Gating), (Undecided, Gating)] {
        let swapped = b.swap_gate(own_id, fence, from, to).await.unwrap();
        said.push(format!("swap {from:?} to {to:?}: {swapped}"));
    }
    for from in [Gating, Gating] {
        let released = b.release_lease(own_id, fence, from, 120).await.unwrap();
        said.push(format!("release from {from:?}: {released}"));
    }
    let left = (b
        .get_work_item(own_id)
        .await
        .unwrap()
        .unwrap()
        .lease_expires_at
        .unwrap()
        - Utc::now())
    .num_seconds();
    said.push(format!(
        "the release renewed the lease: {}",
        (110..=121).contains(&left)
    ));
    said.push(format!(
        "leased after the release: {:?}",
        leases(b.leased_items(&[q], &by_node).await.unwrap())
    ));

    // The claim for a run: the holder gets its lease back, another worker
    // learns the item is held.
    let answer = |claim: ExecutionClaim| match claim {
        ExecutionClaim::Claimed(i) => format!("claimed {}", i.node_id),
        ExecutionClaim::AlreadyLeased(l) => format!("already leased {}", l.item.node_id),
        ExecutionClaim::LeasedToOther => "leased to another".into(),
        ExecutionClaim::Nothing => "nothing".into(),
    };
    for worker in ["w2", "w3"] {
        let claim = b
            .claim_for_execution(worker, &[q], None, &by_node)
            .await
            .unwrap();
        said.push(format!("claim for the run by {worker}: {}", answer(claim)));
    }

    // A park for the run marks the plain item claimable only by its run.
    assert!(b
        .park_for_execution(plain.id, plain.lease_fence, 0)
        .await
        .unwrap());
    let parked = b.get_work_item(plain.id).await.unwrap().unwrap();
    said.push(format!(
        "parked: claim {}, node {}, attempt {}",
        parked.payload["claim"], parked.payload["node_id"], parked.attempt
    ));
    let unfiltered = b.claim_work_item("w1", &[q]).await.unwrap();
    said.push(format!(
        "a claim naming no run after the park: {:?}",
        unfiltered.map(|i| i.node_id)
    ));
    let by_its_run = b
        .claim_work_item_filtered("w1", &[q], None, &by_run)
        .await
        .unwrap();
    said.push(format!(
        "a claim naming the run after the park: {:?}",
        by_its_run.map(|i| (i.node_id, i.attempt))
    ));

    // A lapsed lease is neither released nor listed.
    fx.hooks.force_lease_expired(own_id).await;
    said.push(format!(
        "release a lapsed lease: {}",
        b.release_lease(own_id, fence, Released, 60).await.unwrap()
    ));
    said.push(format!(
        "leased after the lapse: {:?}",
        leases(b.leased_items(&[q], &by_node).await.unwrap())
    ));
    said
}

/// The Postgres store answers the scenario as SQLite does, unscoped and
/// through the default tenant's view.
#[tokio::test]
async fn claims_by_run_and_gate_marks_match_sqlite() {
    let (Some(pg), Some(pg_scoped)) = (
        PostgresFactory.fresh().await,
        PostgresScopedFactory.fresh().await,
    ) else {
        return jamjet_testkit::skip("claims_by_run_and_gate_marks_match_sqlite");
    };
    let sqlite = SqliteFactory.fresh().await.unwrap();
    let sqlite_scoped = SqliteScopedFactory.fresh().await.unwrap();
    for (postgres, sqlite) in [(&pg, &sqlite), (&pg_scoped, &sqlite_scoped)] {
        let want = claims_by_run_and_gates(sqlite).await;
        let got = claims_by_run_and_gates(postgres).await;
        assert_eq!(got, want, "{:?} against {:?}", postgres.kind, sqlite.kind);
    }
}

// ── Run before item ───────────────────────────────────────────────────────────

/// Whether the item's row can be locked now, tried without waiting from a
/// connection of its own, which then lets it go.
async fn item_is_free(db: &TestDatabase, id: WorkItemId) -> bool {
    let mut raw = PgConnection::connect(db.url()).await.unwrap();
    sqlx::query("BEGIN").execute(&mut raw).await.unwrap();
    let tried = sqlx::query("SELECT 1 FROM work_items WHERE id = $1 FOR UPDATE NOWAIT")
        .bind(id)
        .fetch_optional(&mut raw)
        .await;
    let free = match tried {
        Ok(_) => true,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("55P03") => false,
        Err(e) => panic!("try the item's lock: {e}"),
    };
    raw.close().await.unwrap();
    free
}

/// Wait until a connection of the test database waits for a lock.
async fn until_a_call_waits_for_a_lock(db: &TestDatabase, pool: &PgPool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 \
             AND wait_event_type = 'Lock'",
        )
        .bind(db.name())
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: never waited for the run"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A run with one item claimed under a 60 s lease, at `attempt` of
/// `max_attempts`, on a queue of its own; with `lapsed`, the lease ended a
/// second ago by the database's clock.
async fn held_item(
    b: &PgBackend,
    pool: &PgPool,
    max_attempts: u32,
    lapsed: bool,
) -> (ExecutionId, WorkItem) {
    let run = new_run(b).await;
    let queue = format!("run-before-item-{}", run.0.simple());
    let mut i = item(&run, "step", &queue);
    i.max_attempts = max_attempts;
    b.enqueue_work_item(i).await.unwrap();
    let held = b
        .claim_work_item_with_lease("w1", &[queue.as_str()], Some(60))
        .await
        .unwrap()
        .expect("claimable");
    if lapsed {
        expire_by_the_database_clock(pool, held.id).await;
    }
    (run, held)
}

/// Every call that locks a run and an item of it takes the run first (rule
/// L3). While a raw connection holds the run, each such call waits, and its
/// item is still free to lock: the call holds no item that the run's holder
/// could wait for, so the two cannot deadlock. Released, each call lands.
#[tokio::test]
async fn a_call_waiting_for_its_run_holds_no_item_lock() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_call_waiting_for_its_run_holds_no_item_lock");
    };
    let b = Arc::new(store(&db, settings()).await);
    let pool = db.pool().await;
    for what in [
        "commit_turn",
        "commit_turn_unless_terminal",
        "fail_work_item_fenced",
        "fail_work_item_fenced_recorded",
        "finalize_rollover_fenced",
        "move_to_dead_letter",
        "reclaim_expired_leases_recorded",
        "reclaim_expired_leases",
    ] {
        // The plain sweep locks the run only to dead-letter an item.
        let max_attempts = if what == "reclaim_expired_leases" {
            1
        } else {
            3
        };
        let lapsed = what.starts_with("reclaim");
        let (run, held) = held_item(&b, &pool, max_attempts, lapsed).await;
        let raw = hold_run(&db, &run).await;
        let call = tokio::spawn({
            let (b, run, id, fence) = (b.clone(), run.clone(), held.id, held.lease_fence);
            async move {
                let done = Event::new(run.clone(), 0, completion("step"));
                match what {
                    "commit_turn" => b
                        .commit_turn(id, fence, done, true)
                        .await
                        .map(|sequence| format!("{sequence}")),
                    "commit_turn_unless_terminal" => b
                        .commit_turn_unless_terminal(id, fence, done, true)
                        .await
                        .map(|commit| format!("{commit:?}")),
                    "fail_work_item_fenced" => {
                        b.fail_work_item_fenced(id, fence, "boom").await.map(|o| {
                            format!(
                                "{:?}",
                                o.map(|o| matches!(o, FailOutcome::Retryable { .. }))
                            )
                        })
                    }
                    "fail_work_item_fenced_recorded" => b
                        .fail_work_item_fenced_recorded(id, fence, "boom")
                        .await
                        .map(|o| {
                            format!(
                                "{:?}",
                                o.map(|o| matches!(o, FailOutcome::Retryable { .. }))
                            )
                        }),
                    "finalize_rollover_fenced" => b
                        .finalize_rollover_fenced(&run, id, fence)
                        .await
                        .map(|done| format!("{done}")),
                    "move_to_dead_letter" => b
                        .move_to_dead_letter(id, "given up")
                        .await
                        .map(|()| "moved".to_string()),
                    "reclaim_expired_leases_recorded" => b
                        .reclaim_expired_leases_recorded()
                        .await
                        .map(|r| format!("{} {}", r.retryable.len(), r.exhausted.len())),
                    _ => b
                        .reclaim_expired_leases()
                        .await
                        .map(|r| format!("{} {}", r.retryable.len(), r.exhausted.len())),
                }
            }
        });
        until_a_call_waits_for_a_lock(&db, &pool, what).await;
        assert!(
            item_is_free(&db, held.id).await,
            "{what}: waits for the run while it holds the item"
        );
        assert!(!call.is_finished(), "{what}: did not wait for the run");
        raw.close().await.unwrap();
        let landed = call
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{what}: {e}"));
        let expected = match what {
            "commit_turn" => "1",
            "commit_turn_unless_terminal" => "Committed(1)",
            "fail_work_item_fenced" | "fail_work_item_fenced_recorded" => "Some(true)",
            "finalize_rollover_fenced" => "true",
            "move_to_dead_letter" => "moved",
            "reclaim_expired_leases_recorded" => "1 0",
            _ => "0 1",
        };
        assert_eq!(landed, expected, "{what}: lands once the run is released");
    }
}

// ── The load ──────────────────────────────────────────────────────────────────

/// A small generator, so each task's choices are the same on every run.
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

/// What the load did, by name, and every answer it did not expect.
#[derive(Default)]
struct Tally {
    done: BTreeMap<&'static str, u64>,
    errors: Vec<String>,
}

impl Tally {
    fn count(&mut self, what: &'static str) {
        *self.done.entry(what).or_default() += 1;
    }

    fn of(&self, what: &str) -> u64 {
        self.done.get(what).copied().unwrap_or(0)
    }
}

/// The runs the load works on. A run ended by a rollover is replaced.
type Runs = Arc<Mutex<Vec<ExecutionId>>>;

/// An item a task holds, with the fence it holds it under.
struct Held {
    id: WorkItemId,
    run: ExecutionId,
    node: String,
    fence: i64,
}

fn completion(node: &str) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node.into(),
        output: json!({ "ok": true }),
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
    }
}

/// One task of the load: until `until`, a mix of scheduler passes, claims,
/// heartbeats, commits of a turn, failures, lapsed leases, sweeps, fast
/// reclaims and rollovers, on `b`, over `runs`.
async fn load_task(
    task: u64,
    b: Arc<PgBackend>,
    raw: PgPool,
    runs: Runs,
    nodes: Arc<AtomicU64>,
    tally: Arc<Mutex<Tally>>,
    until: Instant,
) {
    let worker = format!("worker-{task}");
    let empty = format!("empty-{task}");
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ (task + 1));
    let mut held: Vec<Held> = Vec::new();
    let unexpected = |tally: &Arc<Mutex<Tally>>, what: &str, e: StateBackendError| {
        tally.lock().unwrap().errors.push(format!("{what}: {e}"));
    };
    while Instant::now() < until {
        let roll = rng.below(100);
        let pick_run = |rng: &mut Rng| {
            let runs = runs.lock().unwrap();
            runs[rng.below(runs.len() as u64) as usize].clone()
        };
        match roll {
            // A scheduler pass that dispatches a new node.
            0..=19 => {
                let id = pick_run(&mut rng);
                let seen = match b.latest_sequence(&id).await {
                    Ok(seen) => seen,
                    Err(e) => {
                        unexpected(&tally, "latest_sequence", e);
                        continue;
                    }
                };
                let node = format!("n{}", nodes.fetch_add(1, Ordering::SeqCst));
                let pass = SchedulerPass {
                    execution_id: id.clone(),
                    seen_sequence: seen,
                    events: vec![EventKind::NodeScheduled {
                        node_id: node.clone(),
                        queue_type: QUEUE.into(),
                    }],
                    work_items: vec![item(&id, &node, QUEUE)],
                    finish: None,
                };
                match b.commit_scheduler_pass(pass).await {
                    Ok(PassCommit::Committed { .. }) => tally.lock().unwrap().count("pass"),
                    Ok(PassCommit::Stale { .. }) => tally.lock().unwrap().count("stale pass"),
                    Ok(PassCommit::ExecutionTerminal(_)) => {
                        tally.lock().unwrap().count("pass on an ended run")
                    }
                    Err(e) => unexpected(&tally, "commit_scheduler_pass", e),
                }
            }
            // A claim, with a lease short enough to lapse in the run.
            20..=44 => match b
                .claim_work_item_with_lease(&worker, &[QUEUE], Some(5))
                .await
            {
                Ok(Some(item)) => {
                    tally.lock().unwrap().count("claim");
                    held.push(Held {
                        id: item.id,
                        run: item.execution_id,
                        node: item.node_id,
                        fence: item.lease_fence,
                    });
                }
                Ok(None) => tally.lock().unwrap().count("empty claim"),
                Err(e) => unexpected(&tally, "claim", e),
            },
            // A heartbeat on a held item.
            45..=54 if !held.is_empty() => {
                let i = rng.below(held.len() as u64) as usize;
                match b.heartbeat_lease(held[i].id, &worker, held[i].fence).await {
                    Ok(renewal) => {
                        held[i].fence = renewal.lease_fence;
                        tally.lock().unwrap().count(if renewal.reattached {
                            "reattached"
                        } else {
                            "heartbeat"
                        });
                    }
                    Err(StateBackendError::FenceLost(_)) => {
                        held.swap_remove(i);
                        tally.lock().unwrap().count("lease lost");
                    }
                    Err(e) => unexpected(&tally, "heartbeat_lease", e),
                }
            }
            // A held item's turn committed: the settle, its completion and,
            // every other time, the run's snapshot, in one write. Half the
            // commits refuse a run that has ended.
            55..=69 if !held.is_empty() => {
                let h = held.swap_remove(rng.below(held.len() as u64) as usize);
                let event = Event::new(h.run.clone(), 0, completion(&h.node));
                let snapshot = rng.below(2) == 0;
                let commit = if rng.below(2) == 0 {
                    b.commit_turn_unless_terminal(h.id, h.fence, event, snapshot)
                        .await
                } else {
                    b.commit_turn(h.id, h.fence, event, snapshot)
                        .await
                        .map(TurnCommit::Committed)
                };
                match commit {
                    Ok(TurnCommit::Committed(_)) => tally.lock().unwrap().count("settle"),
                    Ok(TurnCommit::ExecutionTerminal(_)) => {
                        tally.lock().unwrap().count("settle on an ended run")
                    }
                    Err(StateBackendError::FenceLost(_)) => {
                        tally.lock().unwrap().count("settle refused")
                    }
                    Err(e) => unexpected(&tally, "commit_turn", e),
                }
            }
            // A held item failed, with its events.
            70..=79 if !held.is_empty() => {
                let h = held.swap_remove(rng.below(held.len() as u64) as usize);
                match b
                    .fail_work_item_fenced_recorded(h.id, h.fence, "boom")
                    .await
                {
                    Ok(Some(_)) => tally.lock().unwrap().count("failure"),
                    Ok(None) => tally.lock().unwrap().count("failure refused"),
                    Err(e) => unexpected(&tally, "fail_work_item_fenced_recorded", e),
                }
            }
            // A worker that dies holding an item: its lease lapses.
            80..=87 if !held.is_empty() => {
                let h = held.swap_remove(rng.below(held.len() as u64) as usize);
                sqlx::query(
                    "UPDATE work_items SET lease_expires_at = statement_timestamp() \
                     - interval '1 second' WHERE id = $1 AND status = 'claimed'",
                )
                .bind(h.id)
                .execute(&raw)
                .await
                .unwrap();
                tally.lock().unwrap().count("lapse");
            }
            // The scheduler's sweep.
            88..=92 => match b.reclaim_expired_leases_recorded().await {
                Ok(swept) => {
                    let mut t = tally.lock().unwrap();
                    t.count("sweep");
                    for _ in 0..swept.retryable.len() + swept.exhausted.len() {
                        t.count("swept");
                    }
                }
                Err(e) => unexpected(&tally, "reclaim_expired_leases_recorded", e),
            },
            // A claim of a queue nobody uses: the fast reclaim alone.
            93..=97 => match b.claim_work_item(&worker, &[empty.as_str()]).await {
                Ok(None) => tally.lock().unwrap().count("fast reclaim"),
                Ok(Some(item)) => unexpected(
                    &tally,
                    "fast reclaim",
                    StateBackendError::Database(format!("took {}", item.id)),
                ),
                Err(e) => unexpected(&tally, "fast reclaim", e),
            },
            // A rollover that ends a held item's run; a new run takes its place.
            98..=99 if !held.is_empty() => {
                let h = held.swap_remove(rng.below(held.len() as u64) as usize);
                match b.finalize_rollover_fenced(&h.run, h.id, h.fence).await {
                    Ok(true) => {
                        let fresh = new_run(&*b).await;
                        let mut runs = runs.lock().unwrap();
                        if let Some(slot) = runs.iter_mut().find(|r| **r == h.run) {
                            *slot = fresh;
                        }
                        drop(runs);
                        tally.lock().unwrap().count("rollover");
                    }
                    Ok(false) => tally.lock().unwrap().count("rollover refused"),
                    Err(e) => unexpected(&tally, "finalize_rollover_fenced", e),
                }
            }
            _ => {}
        }
    }
}

/// What must hold of the store after any mix of calls: each run's log is
/// numbered 1 to n with no gap, a claimed item has a worker, a lease and the
/// fence of its epoch, a pending item has neither, no node has two live
/// items, an item is dead-lettered exactly when it has a dead-letter row,
/// and items and events carry their run's tenant.
async fn violations(pool: &PgPool) -> Vec<String> {
    let checks: [(&str, &str); 7] = [
        (
            "a log with a gap or not starting at 1",
            "SELECT execution_id::text FROM events GROUP BY execution_id \
             HAVING count(*) <> max(sequence) OR min(sequence) <> 1",
        ),
        (
            "a claimed item without a worker, a lease or its epoch's fence",
            "SELECT id::text FROM work_items WHERE status = 'claimed' AND (worker_id IS NULL \
             OR lease_expires_at IS NULL OR lease_fence <= 0 \
             OR (lease_fence & 4294967295) <> lease_epoch)",
        ),
        (
            "a pending item with a worker or a fence",
            "SELECT id::text FROM work_items WHERE status = 'pending' \
             AND (worker_id IS NOT NULL OR lease_fence <> 0)",
        ),
        (
            "a node with two live items",
            "SELECT execution_id::text || ' ' || node_id FROM work_items \
             WHERE status IN ('pending', 'claimed') GROUP BY execution_id, node_id \
             HAVING count(*) > 1",
        ),
        (
            "a dead-lettered item without its row, or a row without its item",
            "SELECT w.id::text FROM work_items w WHERE w.status = 'dead_lettered' \
             AND NOT EXISTS (SELECT 1 FROM dead_letter_items d WHERE d.id = w.id) \
             UNION ALL SELECT d.id::text FROM dead_letter_items d WHERE NOT EXISTS \
             (SELECT 1 FROM work_items w WHERE w.id = d.id AND w.status = 'dead_lettered')",
        ),
        (
            "an item of another tenant than its run",
            "SELECT w.id::text FROM work_items w JOIN workflow_executions e \
             ON e.execution_id = w.execution_id WHERE w.tenant_id <> e.tenant_id",
        ),
        (
            "an event of another tenant than its run",
            "SELECT v.id::text FROM events v JOIN workflow_executions e \
             ON e.execution_id = v.execution_id WHERE v.tenant_id <> e.tenant_id",
        ),
    ];
    let mut found = Vec::new();
    for (what, sql) in checks {
        let rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(pool).await.unwrap();
        if !rows.is_empty() {
            found.push(format!("{what}: {rows:?}"));
        }
    }
    found
}

/// 10 s of 8 tasks on two engines mixing claims, heartbeats, commits of a
/// turn, failures, lapsed leases, fast reclaims, sweeps, scheduler passes and
/// rollovers on 20 runs: no call answers an error it should not, under 1
/// percent of the transactions' attempts are retries, and what must hold
/// of the store holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn lock_order_has_no_deadlock_under_load() {
    const RUNS: usize = 20;
    const TASKS: u64 = 8;
    const FOR: Duration = Duration::from_secs(10);
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("lock_order_has_no_deadlock_under_load");
    };
    let engine = || async {
        Arc::new(
            store(
                &db,
                PgSettings {
                    pool_size: 5,
                    ..settings()
                },
            )
            .await,
        )
    };
    let engines = [engine().await, engine().await];
    let raw = db.pool().await;
    let mut ids = Vec::new();
    for _ in 0..RUNS {
        ids.push(new_run(&*engines[0]).await);
    }
    let runs: Runs = Arc::new(Mutex::new(ids));
    let nodes = Arc::new(AtomicU64::new(0));
    let tally = Arc::new(Mutex::new(Tally::default()));

    let scope = Scope::new();
    scope.arm(ATTEMPT, 1, Fault::Hang(Duration::ZERO));
    scope.arm(RETRY, 1, Fault::Hang(Duration::ZERO));
    let until = Instant::now() + FOR;
    let tasks: Vec<_> = (0..TASKS)
        .map(|task| {
            let work = load_task(
                task,
                engines[(task % 2) as usize].clone(),
                raw.clone(),
                runs.clone(),
                nodes.clone(),
                tally.clone(),
                until,
            );
            let scope = scope.clone();
            tokio::spawn(async move { scope.run(work).await })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }

    let tally = std::mem::take(&mut *tally.lock().unwrap());
    assert!(
        tally.errors.is_empty(),
        "{} unexpected answers, first ones: {:#?}",
        tally.errors.len(),
        &tally.errors[..tally.errors.len().min(10)]
    );
    let (attempts, retries) = (scope.fired(ATTEMPT, 1), scope.fired(RETRY, 1));
    eprintln!(
        "load: {:?}; {attempts} attempts, {retries} retries",
        tally.done
    );
    assert!(
        retries * 100 < attempts,
        "{retries} retries in {attempts} attempts"
    );
    for what in [
        "pass",
        "stale pass",
        "claim",
        "heartbeat",
        "settle",
        "failure",
        "lapse",
        "sweep",
        "fast reclaim",
    ] {
        assert!(
            tally.of(what) > 0,
            "the load made no {what}: {:?}",
            tally.done
        );
    }
    let fast_reclaims: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE kind_type = 'retry_scheduled' \
         AND kind_json LIKE '%\"delay_ms\":0%'",
    )
    .fetch_one(&raw)
    .await
    .unwrap();
    assert!(
        fast_reclaims > 0 || tally.of("swept") > 0,
        "no lapsed lease was reclaimed: {:?}",
        tally.done
    );
    let found = violations(&raw).await;
    assert!(found.is_empty(), "{found:#?}");
}
