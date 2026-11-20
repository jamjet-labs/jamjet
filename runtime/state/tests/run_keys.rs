//! A start that carries a run key claims the key in the same step as the
//! run's rows. A second start with the key finds it taken and writes nothing,
//! a start that fails leaves no key behind, `replaces` moves the key only from
//! the run it names, and keys are per tenant.
//!
//! Each case runs on memory, SQLite and tenant-scoped SQLite unless it says
//! otherwise.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{KeyedStart, RunKeyClaim, RunKeyRecord, WorkItem};
use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};
use jamjet_state::{BackendResult, EventKind, InMemoryBackend, SqliteBackend, StateBackend};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

// ── Stores ──────────────────────────────────────────────────────────────

struct Store {
    name: &'static str,
    backend: Arc<dyn StateBackend>,
    /// The SQLite file's pool, to read the rows themselves.
    pool: Option<SqlitePool>,
    /// The tenant the store's runs belong to.
    tenant: &'static str,
}

fn temp_url(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "jamjet_run_keys_{tag}_{}.db",
        Uuid::new_v4().simple()
    ));
    format!("sqlite://{}", p.display())
}

async fn sqlite_file(tag: &str) -> SqliteBackend {
    SqliteBackend::open(&temp_url(tag)).await.unwrap()
}

/// Register a tenant so the scoped rows' foreign keys hold.
async fn register_tenant(db: &SqliteBackend, id: &str) {
    let now = Utc::now();
    db.for_tenant(TenantId::default())
        .create_tenant(Tenant {
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
}

/// Memory, SQLite and SQLite scoped to `acme`, each on its own store.
async fn stores() -> Vec<Store> {
    let base = sqlite_file("base").await;
    let file = sqlite_file("scoped").await;
    register_tenant(&file, "acme").await;
    vec![
        Store {
            name: "memory",
            backend: Arc::new(InMemoryBackend::new()),
            pool: None,
            tenant: "default",
        },
        Store {
            name: "sqlite",
            pool: Some(base.pool()),
            backend: Arc::new(base),
            tenant: "default",
        },
        Store {
            name: "scoped",
            pool: Some(file.pool()),
            backend: Arc::new(file.for_tenant(TenantId::from("acme"))),
            tenant: "acme",
        },
    ]
}

// ── A run's rows ────────────────────────────────────────────────────────

#[derive(Clone)]
struct Start {
    execution: WorkflowExecution,
    started: EventKind,
    scheduled: EventKind,
    item: WorkItem,
}

impl Start {
    fn id(&self) -> ExecutionId {
        self.execution.execution_id.clone()
    }
}

/// The rows of a fresh run of `wf` 1.0.0 whose start node is `node`. The
/// `WorkflowStarted` carries no key: the store writes it.
fn start(tenant: &str, node: &str) -> Start {
    let id = ExecutionId::new();
    let now = Utc::now();
    let input = json!({"order": 1});
    Start {
        execution: WorkflowExecution {
            execution_id: id.clone(),
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: input.clone(),
            current_state: input.clone(),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        },
        started: EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: input,
            initiated_by: None,
            run_key: None,
        },
        scheduled: EventKind::NodeScheduled {
            node_id: node.into(),
            queue_type: "general".into(),
        },
        item: WorkItem {
            id: Uuid::new_v4(),
            execution_id: id,
            node_id: node.into(),
            queue_type: "general".into(),
            payload: json!({"workflow_id": "wf", "workflow_version": "1.0.0"}),
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
    b.start_execution_keyed(
        s.execution.clone(),
        s.started.clone(),
        s.scheduled.clone(),
        s.item.clone(),
        c,
    )
    .await
}

// ── Reading the rows ────────────────────────────────────────────────────

async fn count(pool: &SqlitePool, sql: &'static str, bind: &str) -> i64 {
    sqlx::query(sql)
        .bind(bind)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

/// Every row of `table`, whatever its tenant.
async fn rows_in(pool: &SqlitePool, table: &str) -> i64 {
    let sql = match table {
        "workflow_executions" => "SELECT COUNT(*) FROM workflow_executions",
        "work_items" => "SELECT COUNT(*) FROM work_items",
        "events" => "SELECT COUNT(*) FROM events",
        "run_keys" => "SELECT COUNT(*) FROM run_keys",
        other => panic!("no count for {other}"),
    };
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>(0)
}

/// `(tenant_id, execution_id)` of every stored row of `key`, by tenant.
async fn key_rows(pool: &SqlitePool, key: &str) -> Vec<(String, String)> {
    sqlx::query("SELECT tenant_id, execution_id FROM run_keys WHERE run_key = ? ORDER BY tenant_id")
        .bind(key)
        .fetch_all(pool)
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get("tenant_id"), r.get("execution_id")))
        .collect()
}

/// Nothing of the run is stored: no execution row, no event, no work item.
async fn assert_absent(store: &Store, s: &Start) {
    let b = &*store.backend;
    let name = store.name;
    assert!(
        b.get_execution(&s.id()).await.unwrap().is_none(),
        "{name}: an execution row was written"
    );
    assert!(
        b.get_events(&s.id()).await.unwrap().is_empty(),
        "{name}: events were written"
    );
    assert!(
        b.get_work_item(s.item.id).await.unwrap().is_none(),
        "{name}: a work item was written"
    );
    if let Some(pool) = &store.pool {
        // Read past the tenant filter too.
        let id = s.id().0.to_string();
        for sql in [
            "SELECT COUNT(*) FROM workflow_executions WHERE execution_id = ?",
            "SELECT COUNT(*) FROM events WHERE execution_id = ?",
            "SELECT COUNT(*) FROM work_items WHERE execution_id = ?",
        ] {
            assert_eq!(count(pool, sql, &id).await, 0, "{name}: {sql}");
        }
    }
}

// ── Cases ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_keyed_start_writes_the_key_and_the_run_together() {
    for store in stores().await {
        let b = &*store.backend;
        let name = store.name;
        let s = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &s, claim("order:1", None)).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );

        assert!(b.get_execution(&s.id()).await.unwrap().is_some(), "{name}");
        assert_eq!(
            b.get_run_key("order:1").await.unwrap(),
            Some(record(&s.id())),
            "{name}"
        );
        let events = b.get_events(&s.id()).await.unwrap();
        assert_eq!(events.len(), 2, "{name}: {events:?}");
        assert_eq!(
            serde_json::to_value(&events[0].kind).unwrap(),
            json!({
                "type": "workflow_started",
                "workflow_id": "wf",
                "workflow_version": "1.0.0",
                "initial_input": {"order": 1},
                "run_key": "order:1",
            }),
            "{name}: the store writes the key into WorkflowStarted"
        );
        assert!(
            matches!(&events[1].kind, EventKind::NodeScheduled { node_id, .. } if node_id == "begin"),
            "{name}: {:?}",
            events[1].kind
        );
        assert!(
            b.get_work_item(s.item.id).await.unwrap().is_some(),
            "{name}"
        );

        if let Some(pool) = &store.pool {
            // The run's id as workflow_executions stores it, under the store's tenant.
            let id = s.id().0.to_string();
            assert_eq!(
                key_rows(pool, "order:1").await,
                vec![(store.tenant.to_string(), id.clone())],
                "{name}"
            );
            assert_eq!(
                count(
                    pool,
                    "SELECT COUNT(*) FROM workflow_executions WHERE execution_id = ?",
                    &id
                )
                .await,
                1,
                "{name}"
            );
            let row = sqlx::query(
                "SELECT workflow_id, workflow_version, input_hash, created_at, updated_at \
                 FROM run_keys WHERE run_key = ?",
            )
            .bind("order:1")
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(row.get::<String, _>("workflow_id"), "wf");
            assert_eq!(row.get::<String, _>("workflow_version"), "1.0.0");
            assert_eq!(row.get::<String, _>("input_hash"), "hash-1");
            let created: String = row.get("created_at");
            assert!(
                chrono::DateTime::parse_from_rfc3339(&created).is_ok(),
                "{name}: {created}"
            );
            assert_eq!(row.get::<String, _>("updated_at"), created, "{name}");
        }
    }
}

#[tokio::test]
async fn a_taken_key_writes_nothing() {
    for store in stores().await {
        let b = &*store.backend;
        let name = store.name;
        let first = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &first, claim("order:2", None)).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );

        let again = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &again, claim("order:2", None)).await.unwrap(),
            KeyedStart::Taken(record(&first.id())),
            "{name}"
        );
        assert_absent(&store, &again).await;

        // Other input under the key: the store reports what the key holds
        // and leaves judging it to the caller.
        let other = start(store.tenant, "begin");
        let mut c = claim("order:2", None);
        c.input_hash = "hash-2".into();
        assert_eq!(
            keyed(b, &other, c).await.unwrap(),
            KeyedStart::Taken(record(&first.id())),
            "{name}"
        );
        assert_absent(&store, &other).await;

        // The key still names the first run, and its rows are whole.
        assert_eq!(
            b.get_run_key("order:2").await.unwrap(),
            Some(record(&first.id())),
            "{name}"
        );
        assert_eq!(b.get_events(&first.id()).await.unwrap().len(), 2, "{name}");
        if let Some(pool) = &store.pool {
            for (table, n) in [
                ("workflow_executions", 1),
                ("work_items", 1),
                ("events", 2),
                ("run_keys", 1),
            ] {
                assert_eq!(rows_in(pool, table).await, n, "{name}: {table}");
            }
        }
    }
}

#[tokio::test]
async fn replaces_moves_the_key_only_from_the_named_run() {
    for store in stores().await {
        let b = &*store.backend;
        let name = store.name;
        let old = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &old, claim("order:3", None)).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );

        // Named: the key moves to the new run.
        let new = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &new, claim("order:3", Some(&old.id())))
                .await
                .unwrap(),
            KeyedStart::Started,
            "{name}"
        );
        assert_eq!(
            b.get_run_key("order:3").await.unwrap(),
            Some(record(&new.id())),
            "{name}"
        );

        // A stale `replaces`: the key no longer names the old run.
        let stale = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &stale, claim("order:3", Some(&old.id())))
                .await
                .unwrap(),
            KeyedStart::Taken(record(&new.id())),
            "{name}: a stale replaces moved the key"
        );
        assert_absent(&store, &stale).await;

        // A `replaces` naming a run the key never named.
        let stray = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &stray, claim("order:3", Some(&ExecutionId::new())))
                .await
                .unwrap(),
            KeyedStart::Taken(record(&new.id())),
            "{name}"
        );
        assert_absent(&store, &stray).await;

        assert_eq!(
            b.get_run_key("order:3").await.unwrap(),
            Some(record(&new.id())),
            "{name}"
        );
        // The old run keeps its rows and the key in its WorkflowStarted.
        let events = b.get_events(&old.id()).await.unwrap();
        assert!(
            matches!(&events[0].kind, EventKind::WorkflowStarted { run_key: Some(k), .. } if k == "order:3"),
            "{name}: {:?}",
            events[0].kind
        );
        if let Some(pool) = &store.pool {
            assert_eq!(
                key_rows(pool, "order:3").await,
                vec![(store.tenant.to_string(), new.id().0.to_string())],
                "{name}"
            );
        }
    }
}

const ROUNDS: usize = 200;

/// Two engine processes, each with its own pool on one file, start the same
/// key at once. Each round one start wins and the other finds the key taken,
/// and only the winner's run is stored. The rounds whose two calls were in
/// flight at the same time are counted: on SQLite at least one must be, or
/// the race never opened. Memory runs the same rounds, but its start takes a
/// few microseconds and the two calls seldom overlap, so there the rounds
/// show the outcome only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_keyed_starts_at_once_make_one_run() {
    let base_url = temp_url("race_base");
    let base = (
        SqliteBackend::open(&base_url).await.unwrap(),
        SqliteBackend::open(&base_url).await.unwrap(),
    );
    let base_pool = base.0.pool();
    let scoped_url = temp_url("race_scoped");
    let scoped = (
        SqliteBackend::open(&scoped_url).await.unwrap(),
        SqliteBackend::open(&scoped_url).await.unwrap(),
    );
    register_tenant(&scoped.0, "acme").await;
    let scoped_pool = scoped.0.pool();
    let memory: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());

    type Pair = (Arc<dyn StateBackend>, Arc<dyn StateBackend>);
    let cases: Vec<(&str, Pair, Option<SqlitePool>, &str)> = vec![
        ("memory", (memory.clone(), memory), None, "default"),
        (
            "sqlite",
            (Arc::new(base.0), Arc::new(base.1)),
            Some(base_pool),
            "default",
        ),
        (
            "scoped",
            (
                Arc::new(scoped.0.for_tenant(TenantId::from("acme"))),
                Arc::new(scoped.1.for_tenant(TenantId::from("acme"))),
            ),
            Some(scoped_pool),
            "acme",
        ),
    ];

    for (name, (one, two), pool, tenant) in cases {
        let mut overlapped = 0;
        for round in 0..ROUNDS {
            let key = format!("race:{round}");
            let runs = [start(tenant, "begin"), start(tenant, "begin")];
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let mut handles = Vec::new();
            for (b, s) in [
                (one.clone(), runs[0].clone()),
                (two.clone(), runs[1].clone()),
            ] {
                let barrier = barrier.clone();
                let c = claim(&key, None);
                handles.push(tokio::spawn(async move {
                    barrier.wait().await;
                    let began = Instant::now();
                    let outcome = keyed(&*b, &s, c).await;
                    (outcome, began, Instant::now())
                }));
            }
            let (a, a_began, a_ended) = handles.remove(0).await.unwrap();
            let (z, z_began, z_ended) = handles.remove(0).await.unwrap();
            if a_began.max(z_began) < a_ended.min(z_ended) {
                overlapped += 1;
            }
            let outcomes = (
                a.unwrap_or_else(|e| panic!("{name} round {round}: {e}")),
                z.unwrap_or_else(|e| panic!("{name} round {round}: {e}")),
            );
            let (winner, loser) = match &outcomes {
                (KeyedStart::Started, KeyedStart::Taken(r)) => {
                    assert_eq!(r, &record(&runs[0].id()), "{name} round {round}");
                    (&runs[0], &runs[1])
                }
                (KeyedStart::Taken(r), KeyedStart::Started) => {
                    assert_eq!(r, &record(&runs[1].id()), "{name} round {round}");
                    (&runs[1], &runs[0])
                }
                other => panic!("{name} round {round}: one start must win, got {other:?}"),
            };
            assert!(
                one.get_execution(&winner.id()).await.unwrap().is_some(),
                "{name} round {round}: the winner's run"
            );
            assert!(
                one.get_execution(&loser.id()).await.unwrap().is_none(),
                "{name} round {round}: the loser's run was stored too"
            );
            assert!(
                one.get_events(&loser.id()).await.unwrap().is_empty(),
                "{name} round {round}: the loser's events were stored"
            );
        }

        // One run per round, and one key.
        match &pool {
            Some(pool) => {
                for table in ["workflow_executions", "work_items", "run_keys"] {
                    assert_eq!(rows_in(pool, table).await, ROUNDS as i64, "{name}: {table}");
                }
            }
            None => assert_eq!(
                one.list_executions(None, 1000, 0).await.unwrap().len(),
                ROUNDS,
                "{name}"
            ),
        }
        eprintln!("{name}: both starts in flight at once in {overlapped} of {ROUNDS} rounds");
        if pool.is_some() {
            assert!(overlapped > 0, "{name}: the two starts never overlapped");
        }
    }
}

#[tokio::test]
async fn keys_are_per_tenant() {
    // Scoped: one file, two tenants, one key.
    let file = sqlite_file("tenants").await;
    register_tenant(&file, "acme").await;
    register_tenant(&file, "globex").await;
    let acme = file.for_tenant(TenantId::from("acme"));
    let globex = file.for_tenant(TenantId::from("globex"));
    let a = start("acme", "begin");
    let g = start("globex", "begin");
    assert_eq!(
        keyed(&acme, &a, claim("shared:1", None)).await.unwrap(),
        KeyedStart::Started
    );
    assert_eq!(
        keyed(&globex, &g, claim("shared:1", None)).await.unwrap(),
        KeyedStart::Started,
        "globex found acme's key"
    );
    assert_ne!(a.id(), g.id());
    assert_eq!(
        acme.get_run_key("shared:1").await.unwrap(),
        Some(record(&a.id()))
    );
    assert_eq!(
        globex.get_run_key("shared:1").await.unwrap(),
        Some(record(&g.id()))
    );
    assert_eq!(
        key_rows(&file.pool(), "shared:1").await,
        vec![
            ("acme".to_string(), a.id().0.to_string()),
            ("globex".to_string(), g.id().0.to_string()),
        ]
    );

    // The unscoped backends take the tenant from the work item.
    let sqlite = sqlite_file("tenants_base").await;
    let pool = sqlite.pool();
    let unscoped: Vec<(&str, Arc<dyn StateBackend>)> = vec![
        ("memory", Arc::new(InMemoryBackend::new())),
        ("sqlite", Arc::new(sqlite)),
    ];
    for (name, b) in unscoped {
        let a = start("acme", "begin");
        let g = start("globex", "begin");
        assert_eq!(
            keyed(&*b, &a, claim("shared:2", None)).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );
        assert_eq!(
            keyed(&*b, &g, claim("shared:2", None)).await.unwrap(),
            KeyedStart::Started,
            "{name}: globex's item found acme's key"
        );
        if name == "sqlite" {
            assert_eq!(
                key_rows(&pool, "shared:2").await,
                vec![
                    ("acme".to_string(), a.id().0.to_string()),
                    ("globex".to_string(), g.id().0.to_string()),
                ]
            );
        }
    }
}

/// SQLite only: memory has no step inside a start that can fail.
#[tokio::test]
async fn a_failure_inside_the_start_leaves_neither_key_nor_run() {
    for store in stores().await {
        let Some(pool) = store.pool.clone() else {
            continue;
        };
        let b = &*store.backend;
        let name = store.name;
        // The work item is the start's last row, after the key.
        sqlx::query(
            "CREATE TRIGGER boom_item BEFORE INSERT ON work_items WHEN NEW.node_id = 'boom' \
             BEGIN SELECT RAISE(ABORT, 'boom'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let s = start(store.tenant, "boom");
        let err = keyed(b, &s, claim("order:6", None))
            .await
            .expect_err("the trigger fails the start");
        assert!(err.to_string().contains("boom"), "{name}: {err}");
        assert_absent(&store, &s).await;
        assert_eq!(b.get_run_key("order:6").await.unwrap(), None, "{name}");
        assert_eq!(
            rows_in(&pool, "run_keys").await,
            0,
            "{name}: the key row outlived its run"
        );

        // The key is free: the next start with it runs.
        let next = start(store.tenant, "begin");
        assert_eq!(
            keyed(b, &next, claim("order:6", None)).await.unwrap(),
            KeyedStart::Started,
            "{name}"
        );
        assert_eq!(
            b.get_run_key("order:6").await.unwrap(),
            Some(record(&next.id())),
            "{name}"
        );
    }
}

#[tokio::test]
async fn get_run_key_of_another_tenant_is_none() {
    let file = sqlite_file("lookup").await;
    register_tenant(&file, "acme").await;
    register_tenant(&file, "globex").await;
    let acme = file.for_tenant(TenantId::from("acme"));
    let globex = file.for_tenant(TenantId::from("globex"));
    let a = start("acme", "begin");
    assert_eq!(
        keyed(&acme, &a, claim("mine:1", None)).await.unwrap(),
        KeyedStart::Started
    );

    assert_eq!(globex.get_run_key("mine:1").await.unwrap(), None);
    // The unscoped backend reads the default tenant's keys.
    assert_eq!(file.get_run_key("mine:1").await.unwrap(), None);
    // The row is there, under acme.
    assert_eq!(
        acme.get_run_key("mine:1").await.unwrap(),
        Some(record(&a.id()))
    );
    assert_eq!(
        key_rows(&file.pool(), "mine:1").await,
        vec![("acme".to_string(), a.id().0.to_string())]
    );
}

/// The key is recorded in the run's `WorkflowStarted`, so a keyed start that
/// begins with another event is refused before anything is written.
#[tokio::test]
async fn a_keyed_start_must_begin_with_workflow_started() {
    for store in stores().await {
        let b = &*store.backend;
        let name = store.name;
        let mut s = start(store.tenant, "begin");
        s.started = s.scheduled.clone();
        let err = keyed(b, &s, claim("order:8", None))
            .await
            .expect_err("a start without WorkflowStarted");
        assert!(err.to_string().contains("WorkflowStarted"), "{name}: {err}");
        assert_absent(&store, &s).await;
        assert_eq!(b.get_run_key("order:8").await.unwrap(), None, "{name}");
    }
}
