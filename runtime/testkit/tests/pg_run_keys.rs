//! Run keys on the Postgres store, where the suite's cases do not reach:
//! a key moved from the run `replaces` names and from no other, two engines
//! moving one key at once, the tenant an unscoped start claims under, and a
//! key with a NUL.
//!
//! The key's row is the lock. A move is one update that re-checks the run
//! the key names on the row as it stands when the update gets it, so two
//! moves from one run cannot both land.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    BackendResult, EventKind, KeyedStart, PgBackend, PgSettings, RunKeyClaim, RunKeyRecord,
    StateBackend, StateBackendError, Tenant, TenantId, TenantStatus, WorkItem,
};
use jamjet_testkit::conformance::cases::support::race;
use jamjet_testkit::pg::{TestDatabase, POOL_SIZE};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

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

async fn make_tenant(b: &PgBackend, id: &str) -> PgBackend {
    let view = b.for_tenant(TenantId::from(id));
    let now = Utc::now();
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

/// The rows of a fresh run of `wf` 1.0.0, its item stamped `tenant`.
struct Start {
    execution: WorkflowExecution,
    item: WorkItem,
}

impl Start {
    fn new(tenant: &str) -> Self {
        let id = ExecutionId::new();
        let now = Utc::now();
        Self {
            execution: WorkflowExecution {
                execution_id: id.clone(),
                workflow_id: "wf".into(),
                workflow_version: "1.0.0".into(),
                status: WorkflowStatus::Running,
                initial_input: json!({ "order": 1 }),
                current_state: json!({ "order": 1 }),
                started_at: now,
                updated_at: now,
                completed_at: None,
                session_type: None,
                parent_execution_id: None,
                segment_number: 0,
            },
            item: WorkItem {
                id: Uuid::new_v4(),
                execution_id: id,
                node_id: "begin".into(),
                queue_type: "general".into(),
                payload: json!({}),
                attempt: 0,
                max_attempts: 3,
                created_at: now,
                lease_expires_at: None,
                worker_id: None,
                lease_fence: 0,
                tenant_id: tenant.into(),
            },
        }
    }

    fn id(&self) -> ExecutionId {
        self.execution.execution_id.clone()
    }
}

fn claim(key: &str, replaces: Option<&ExecutionId>) -> RunKeyClaim {
    RunKeyClaim {
        run_key: key.into(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        input_hash: "hash-1".into(),
        replaces: replaces.cloned(),
    }
}

/// What the key records for a run started with [`claim`].
fn record(id: &ExecutionId) -> RunKeyRecord {
    RunKeyRecord {
        execution_id: id.clone(),
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        input_hash: "hash-1".into(),
    }
}

async fn keyed(b: &dyn StateBackend, s: &Start, c: RunKeyClaim) -> BackendResult<KeyedStart> {
    let started = EventKind::WorkflowStarted {
        workflow_id: "wf".into(),
        workflow_version: "1.0.0".into(),
        initial_input: json!({ "order": 1 }),
        initiated_by: None,
        run_key: None,
    };
    let scheduled = EventKind::NodeScheduled {
        node_id: "begin".into(),
        queue_type: "general".into(),
    };
    b.start_execution_keyed(s.execution.clone(), started, scheduled, s.item.clone(), c)
        .await
}

/// `(tenant_id, execution_id)` of every stored row of `key`, by tenant.
async fn key_rows(pool: &PgPool, key: &str) -> Vec<(String, Uuid)> {
    sqlx::query_as(
        "SELECT tenant_id, execution_id FROM run_keys WHERE run_key = $1 ORDER BY tenant_id",
    )
    .bind(key)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Whether any row of the run `id` is stored.
async fn stored(pool: &PgPool, id: &ExecutionId) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workflow_executions WHERE execution_id = $1) \
             OR EXISTS (SELECT 1 FROM events WHERE execution_id = $1) \
             OR EXISTS (SELECT 1 FROM work_items WHERE execution_id = $1)",
    )
    .bind(id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// On the unscoped store and on a tenant's view: a start naming the run the
/// key holds moves the key, one naming an earlier holder or a run the key
/// never named is told who holds it and writes nothing, and the old run keeps
/// its rows.
#[tokio::test]
async fn replaces_moves_the_key_only_from_the_named_run() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("replaces_moves_the_key_only_from_the_named_run");
    };
    let base = store(&db).await;
    let acme = make_tenant(&base, "acme").await;
    let pool = db.pool().await;
    for (name, b, tenant) in [("unscoped", &base, "default"), ("acme", &acme, "acme")] {
        let key = format!("order:{name}");
        let old = Start::new(tenant);
        assert_eq!(
            keyed(b, &old, claim(&key, None)).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );

        let new = Start::new(tenant);
        assert_eq!(
            keyed(b, &new, claim(&key, Some(&old.id()))).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );
        assert_eq!(
            b.get_run_key(&key).await.unwrap(),
            Some(record(&new.id())),
            "{name}"
        );

        for (what, replaces) in [
            ("the key's earlier run", old.id()),
            ("a run the key never named", ExecutionId::new()),
        ] {
            let late = Start::new(tenant);
            assert_eq!(
                keyed(b, &late, claim(&key, Some(&replaces))).await.unwrap(),
                KeyedStart::Taken(record(&new.id())),
                "{name}: a start replacing {what} moved the key"
            );
            assert!(!stored(&pool, &late.id()).await, "{name}: {what}");
        }

        assert_eq!(
            key_rows(&pool, &key).await,
            vec![(tenant.to_string(), new.id().0)],
            "{name}"
        );
        let events = b.get_events(&old.id()).await.unwrap();
        assert!(
            matches!(&events[0].kind, EventKind::WorkflowStarted { run_key: Some(k), .. } if *k == key),
            "{name}: {:?}",
            events[0].kind
        );
    }
}

/// Two engines each start a run that replaces the one the key names, at
/// once, 100 times: one moves the key, the other is told the winner holds
/// it and leaves no run. Some rounds must overlap, or the race never opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_moves_from_one_run_at_once_move_the_key_once() {
    const ROUNDS: usize = 100;
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("two_moves_from_one_run_at_once_move_the_key_once");
    };
    let (one, two) = (store(&db).await, store(&db).await);
    let pool = db.pool().await;
    let mut overlapped = 0;
    for round in 0..ROUNDS {
        let key = format!("move:{round}");
        let old = Start::new("default");
        assert_eq!(
            keyed(&one, &old, claim(&key, None)).await.unwrap(),
            KeyedStart::Started
        );
        let (a, b) = (Start::new("default"), Start::new("default"));
        let (ida, idb) = (a.id(), b.id());
        let (ca, cb) = (claim(&key, Some(&old.id())), claim(&key, Some(&old.id())));
        let (x, y) = (one.clone(), two.clone());
        let (ra, rb, at_once) = race(async move { keyed(&x, &a, ca).await }, async move {
            keyed(&y, &b, cb).await
        })
        .await;
        overlapped += usize::from(at_once);
        let (winner, loser) = match (ra.unwrap(), rb.unwrap()) {
            (KeyedStart::Started, KeyedStart::Taken(held)) => {
                assert_eq!(held, record(&ida), "round {round}");
                (ida, idb)
            }
            (KeyedStart::Taken(held), KeyedStart::Started) => {
                assert_eq!(held, record(&idb), "round {round}");
                (idb, ida)
            }
            other => panic!("round {round}: not one move and one refusal: {other:?}"),
        };
        assert!(!stored(&pool, &loser).await, "round {round}");
        assert_eq!(
            key_rows(&pool, &key).await,
            vec![("default".to_string(), winner.0)],
            "round {round}"
        );
    }
    assert!(
        overlapped > 0,
        "the two moves never overlapped in {ROUNDS} rounds, so no round raced"
    );
}

/// An unscoped start claims the key under its work item's tenant, as the
/// trait says and SQLite does, and the unscoped lookup reads the default
/// tenant's keys. A tenant's view claims under its own tenant whatever the
/// item carries.
#[tokio::test]
async fn a_key_is_claimed_under_the_items_tenant_or_the_views() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_key_is_claimed_under_the_items_tenant_or_the_views");
    };
    let base = store(&db).await;
    let acme = make_tenant(&base, "acme").await;
    let pool = db.pool().await;

    let unscoped = Start::new("acme");
    assert_eq!(
        keyed(&base, &unscoped, claim("tenant:1", None))
            .await
            .unwrap(),
        KeyedStart::Started
    );
    assert_eq!(
        key_rows(&pool, "tenant:1").await,
        vec![("acme".to_string(), unscoped.id().0)]
    );
    assert_eq!(base.get_run_key("tenant:1").await.unwrap(), None);
    assert_eq!(
        acme.get_run_key("tenant:1").await.unwrap(),
        Some(record(&unscoped.id()))
    );

    let scoped = Start::new("default");
    assert_eq!(
        keyed(&acme, &scoped, claim("tenant:2", None))
            .await
            .unwrap(),
        KeyedStart::Started
    );
    assert_eq!(
        key_rows(&pool, "tenant:2").await,
        vec![("acme".to_string(), scoped.id().0)]
    );
}

/// A claim with a NUL in its text is refused before anything is sent, and a
/// lookup of such a key finds nothing.
#[tokio::test]
async fn a_key_with_a_nul_is_refused_and_names_nothing() {
    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("a_key_with_a_nul_is_refused_and_names_nothing");
    };
    let base = store(&db).await;
    let pool = db.pool().await;
    let mut bad = vec![claim("nul:\0", None)];
    for field in 0..3 {
        let mut c = claim("nul:field", None);
        match field {
            0 => c.workflow_id = "w\0f".into(),
            1 => c.workflow_version = "1\0".into(),
            _ => c.input_hash = "h\0".into(),
        }
        bad.push(c);
    }
    for c in bad {
        let s = Start::new("default");
        let out = keyed(&base, &s, c.clone()).await;
        assert!(
            matches!(&out, Err(StateBackendError::Database(m)) if m == "text contains a NUL character"),
            "{c:?}: {out:?}"
        );
        assert!(!stored(&pool, &s.id()).await, "{c:?}");
    }
    let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(keys, 0);
    assert_eq!(base.get_run_key("nul:\0").await.unwrap(), None);
}
