//! `POST /ops/tick`: what the engine's loops would do, once, on request.
//!
//! With the background loops off, a tick is what reclaims lapsed leases,
//! applies approval deadlines, dispatches what is ready, runs the engine's
//! own steps (the model and eval calls among them) and projects. These cases
//! drive it through the router, the way a scheduler's HTTP call does, over
//! the memory store, a SQLite file and, when `JAMJET_TEST_POSTGRES_URL` is
//! set, Postgres.
//!
//! Who may call it is the point of the first cases: the tick makes the
//! engine act, so it takes an operator's token and nothing else, and a
//! refused request leaves the tick row, the runs and the queues as they were.

mod support;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::driver::{Driver, TickResult};
use jamjet_api::routes::{build_router_with_opts, refuse_claims_while_draining, Draining};
use jamjet_api::startup::{Background, DriverConfig};
use jamjet_api::state::AppState;
use jamjet_audit::{AuditBackend, AuditEnricher, AuditSigner, NoopAuditBackend, PgAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_models::{
    ModelAdapter, ModelError, ModelRegistry, ModelRequest, ModelResponse, StructuredRequest,
};
use jamjet_state::backend::{StateBackend, StateBackendError};
use jamjet_state::event::EventKind;
use jamjet_state::{
    ClaimFilter, Event, InMemoryBackend, PgBackend, PgSettings, SqliteBackend, Tenant, TenantId,
    TenantStatus,
};
use jamjet_testkit::invariants::{self, Store};
use jamjet_testkit::pg::TestDatabase;
use jamjet_testkit::{Hooks, Intercept, Op};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;

// ── What a test does around the store's calls ────────────────────────────────

/// Counts, records and holds calls for a case. Does nothing until armed.
struct Probe {
    /// Calls to `begin_tick` that reached the store.
    begins: AtomicUsize,
    /// What `begin_tick` fails with: 0 not at all, 1 as a store with no tick
    /// state does, 2 as a database that cannot be reached does.
    begin_fault: AtomicUsize,
    /// The `after` of each read of the running runs, in order.
    lists: Mutex<Vec<Option<String>>>,
    /// The run of each `get_execution`, in order: the order a walk advances.
    visits: Mutex<Vec<String>>,
    /// Sleep this long before every `get_execution`: a slow store, so a
    /// small budget cuts a walk.
    delay_ms: AtomicU64,
    /// Sleep once, before the `get_execution` of this run.
    hang: Mutex<Option<(String, Duration)>>,
    /// Sleep this long in every sweep of lapsed leases.
    reclaim_ms: AtomicU64,
    /// The first read of the running runs says so on `entered` and waits for
    /// a permit on `release`.
    hold_first_list: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Probe {
    fn new() -> Self {
        Self {
            begins: AtomicUsize::new(0),
            begin_fault: AtomicUsize::new(0),
            lists: Mutex::new(Vec::new()),
            visits: Mutex::new(Vec::new()),
            delay_ms: AtomicU64::new(0),
            hang: Mutex::new(None),
            reclaim_ms: AtomicU64::new(0),
            hold_first_list: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }

    fn lists(&self) -> Vec<Option<String>> {
        self.lists.lock().unwrap().clone()
    }

    fn visits(&self) -> Vec<String> {
        self.visits.lock().unwrap().clone()
    }

    /// Forget what was recorded, before the call a case looks at.
    fn forget(&self) {
        self.lists.lock().unwrap().clear();
        self.visits.lock().unwrap().clear();
    }
}

#[async_trait]
impl Hooks for Probe {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        match op {
            Op::BeginTick { .. } => {
                self.begins.fetch_add(1, Ordering::SeqCst);
                match self.begin_fault.load(Ordering::SeqCst) {
                    1 => {
                        return Some(StateBackendError::Database(
                            "this backend cannot coordinate ticks".into(),
                        ))
                    }
                    2 => return Some(StateBackendError::Unavailable("injected".into())),
                    _ => {}
                }
            }
            Op::ListRunningExecutionsAfter { after, .. } => {
                self.lists.lock().unwrap().push(after.map(str::to_string));
                if self.hold_first_list.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.acquire().await.unwrap().forget();
                }
            }
            Op::GetExecution(id) => {
                let id = id.0.to_string();
                self.visits.lock().unwrap().push(id.clone());
                let ms = self.delay_ms.load(Ordering::SeqCst);
                if ms > 0 {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                let hang = {
                    let mut hang = self.hang.lock().unwrap();
                    match &*hang {
                        Some((run, _)) if *run == id => hang.take(),
                        _ => None,
                    }
                };
                if let Some((_, how_long)) = hang {
                    tokio::time::sleep(how_long).await;
                }
            }
            Op::ReclaimExpiredLeasesRecorded => {
                let ms = self.reclaim_ms.load(Ordering::SeqCst);
                if ms > 0 {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
            _ => {}
        }
        None
    }
}

/// A model that takes `delay` to answer, and counts the calls it gets.
struct SlowModel {
    calls: Arc<AtomicUsize>,
    delay: Duration,
}

#[async_trait]
impl ModelAdapter for SlowModel {
    fn system_name(&self) -> &'static str {
        "slow"
    }
    fn default_model(&self) -> &str {
        "slow-model"
    }
    async fn chat(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(ModelResponse {
            content: "Paris.".into(),
            model: "slow-model".into(),
            finish_reason: "stop".into(),
            input_tokens: 1,
            output_tokens: 1,
            structured: None,
            tool_calls: vec![],
            cost_usd: None,
        })
    }
    async fn structured_output(&self, _: StructuredRequest) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }
}

// ── A store, an engine on it, and a driver ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Memory,
    Sqlite,
    Postgres,
}

const KINDS: [Kind; 3] = [Kind::Memory, Kind::Sqlite, Kind::Postgres];

/// The stores a case runs on: all, or the ones `OPS_TICK_KINDS` names
/// (`memory,sqlite,postgres`), to run a case on one.
fn kinds() -> Vec<Kind> {
    let wanted = std::env::var("OPS_TICK_KINDS").unwrap_or_default();
    KINDS
        .into_iter()
        .filter(|k| wanted.is_empty() || wanted.contains(&format!("{k:?}").to_lowercase()))
        .collect()
}

/// The tick budget the test engines are configured with.
const CONFIGURED_TICK_MS: u64 = 60_000;

/// What a case may change about the engine it runs.
struct Options {
    config: DriverConfig,
    models: Arc<ModelRegistry>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            config: DriverConfig {
                // Roomy, so a slow shared database cannot cut a case that is
                // not about budgets. The cases about budgets ask for theirs.
                inline_budget: Duration::from_secs(30),
                // What a request's `budget_ms` is held to.
                tick_budget: Duration::from_millis(CONFIGURED_TICK_MS),
                engine_concurrency: 2,
                tick_interval: Duration::from_secs(60),
            },
            models: Arc::new(ModelRegistry::new()),
        }
    }
}

struct Env {
    kind: Kind,
    /// The store itself, read by the case without passing the probe.
    inner: Arc<dyn StateBackend>,
    sqlite: Option<Arc<SqliteBackend>>,
    pg: Option<PgBackend>,
    probe: Arc<Probe>,
    state: AppState,
    driver: Arc<Driver>,
    draining: Draining,
    trigger: Mutex<Option<jamjet_worker::ShutdownTrigger>>,
    store: Option<Store>,
    dir: Option<PathBuf>,
    _db: Option<TestDatabase>,
    /// Tokens, by the role they hold: operator, developer, reviewer, viewer.
    ops: String,
    alice: String,
    bob: String,
    viewer: String,
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Env {
    /// `None` for Postgres when it is not configured.
    async fn new(kind: Kind, case: &str) -> Option<Env> {
        Self::with(kind, case, Options::default()).await
    }

    async fn with(kind: Kind, case: &str, options: Options) -> Option<Env> {
        let mut dir = None;
        let mut db = None;
        let mut store = None;
        let mut sqlite = None;
        let mut pg = None;
        let inner: Arc<dyn StateBackend> = match kind {
            Kind::Memory => Arc::new(InMemoryBackend::new()),
            Kind::Sqlite => {
                let path = std::env::temp_dir().join(format!("jjtick-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                let file = path.join("runtime.db");
                let backend = Arc::new(
                    SqliteBackend::open(&format!("sqlite://{}", file.display()))
                        .await
                        .expect("open the test database"),
                );
                dir = Some(path);
                store = Some(Store::SqliteFile(file));
                sqlite = Some(backend.clone());
                backend
            }
            Kind::Postgres => {
                let Some(fresh) = TestDatabase::fresh().await else {
                    jamjet_testkit::skip(case);
                    return None;
                };
                let backend = PgBackend::connect(fresh.url(), PgSettings::default())
                    .await
                    .unwrap_or_else(|e| panic!("connect to the test database: {e}"));
                store = Some(Store::Postgres(fresh.url().to_string()));
                pg = Some(backend.clone());
                db = Some(fresh);
                Arc::new(backend)
            }
        };
        let probe = Arc::new(Probe::new());
        let backend: Arc<dyn StateBackend> = Arc::new(Intercept::new(inner.clone(), probe.clone()));
        let (state, driver, trigger) = engine_on(
            backend.clone(),
            pg.as_ref(),
            &options.config,
            options.models.clone(),
        );
        // The default tenant's row, which the token routes may look up. A
        // store that has it already says so, and that is fine.
        let now = chrono::Utc::now();
        let _ = inner
            .create_tenant(Tenant {
                id: TenantId::default(),
                name: "default".into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await;
        let mint = |name: &'static str, role: &'static str| {
            let inner = inner.clone();
            async move { inner.create_token(name, role).await.unwrap().0 }
        };
        let ops = mint("ops", "operator").await;
        let alice = mint("alice", "developer").await;
        let bob = mint("bob", "reviewer").await;
        let viewer = mint("viewer", "viewer").await;
        Some(Env {
            kind,
            inner,
            sqlite,
            pg,
            probe,
            state,
            driver,
            draining: Draining::default(),
            trigger: Mutex::new(Some(trigger)),
            store,
            dir,
            _db: db,
            ops,
            alice,
            bob,
            viewer,
        })
    }

    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Memory => "memory",
            Kind::Sqlite => "sqlite",
            Kind::Postgres => "postgres",
        }
    }

    /// The server's router with a driver, outside dev mode, behind the drain
    /// flag as `main` runs it.
    fn app(&self) -> axum::Router {
        self.app_with(&self.driver)
    }

    fn app_with(&self, driver: &Arc<Driver>) -> axum::Router {
        build_router_with_opts(self.state.clone(), false)
            .layer(axum::Extension(driver.clone()))
            .layer(axum::middleware::from_fn_with_state(
                self.draining.clone(),
                refuse_claims_while_draining,
            ))
    }

    /// The dev-mode router: no auth layer.
    fn dev_app(&self) -> axum::Router {
        build_router_with_opts(self.state.clone(), true)
            .layer(axum::Extension(self.driver.clone()))
            .layer(axum::middleware::from_fn_with_state(
                self.draining.clone(),
                refuse_claims_while_draining,
            ))
    }

    /// `POST /ops/tick` with the operator's token and this body.
    async fn tick(&self, body: &str) -> (StatusCode, Value) {
        let reply = send(self.app(), "POST", "/ops/tick", Some(&self.ops), Some(body)).await;
        (reply.status, reply.body)
    }

    /// A tick that must answer 200 and have run.
    async fn tick_ran(&self, body: &str) -> Value {
        let (st, report) = self.tick(body).await;
        assert_eq!(st, StatusCode::OK, "{}: {report}", self.name());
        assert_eq!(report["ran"], true, "{}: {report}", self.name());
        report
    }

    async fn register(&self, ir: &Value) {
        let (st, body) = support::post(&self.state, "/workflows", json!({ "ir": ir })).await;
        assert!(
            st.is_success(),
            "{}: register failed: {st} {body}",
            self.name()
        );
    }

    /// Start a run of `workflow` through the route, as a caller does.
    async fn start(&self, workflow: &str) -> ExecutionId {
        self.start_with(workflow, json!({})).await
    }

    async fn start_with(&self, workflow: &str, input: Value) -> ExecutionId {
        let (st, body) = support::post(
            &self.state,
            "/executions",
            json!({ "workflow_id": workflow, "workflow_version": "0.1.0", "input": input }),
        )
        .await;
        assert!(
            st.is_success(),
            "{}: start failed: {st} {body}",
            self.name()
        );
        parse_id(body["execution_id"].as_str().expect("an execution_id"))
    }

    /// An external worker finishes `node`'s item on `queue`: claim it by run
    /// and node, and commit its completion.
    async fn complete(&self, run: &ExecutionId, node: &str, queue: &str) {
        let item = self
            .inner
            .claim_work_item_filtered(
                "test-worker",
                &[queue],
                None,
                &ClaimFilter {
                    execution_id: Some(run.clone()),
                    node_id: Some(node.to_string()),
                },
            )
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{}: no pending item for {node}", self.name()));
        self.inner
            .commit_turn(
                item.id,
                item.lease_fence,
                Event::new(
                    run.clone(),
                    0,
                    EventKind::NodeCompleted {
                        node_id: node.into(),
                        output: json!({}),
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
                ),
                true,
            )
            .await
            .unwrap();
    }

    async fn events(&self, run: &ExecutionId) -> Vec<Event> {
        self.inner.get_events(run).await.unwrap()
    }

    /// Every run, by id, with the kind of each event it holds.
    async fn log(&self) -> Vec<(String, Vec<String>)> {
        let mut all = Vec::new();
        for run in self.inner.list_executions(None, 10_000, 0).await.unwrap() {
            let events = self.events(&run.execution_id).await;
            all.push((
                run.execution_id.0.to_string(),
                events
                    .iter()
                    .map(|e| format!("{}:{}", e.sequence, kind_name(e)))
                    .collect(),
            ));
        }
        all.sort();
        all
    }

    /// Every run's open work items, by node and whether it is claimed.
    async fn items(&self) -> Vec<(String, Vec<(String, bool)>)> {
        let mut all = Vec::new();
        for run in self.inner.list_executions(None, 10_000, 0).await.unwrap() {
            let mut open: Vec<(String, bool)> = self
                .inner
                .open_work_items(&run.execution_id)
                .await
                .unwrap()
                .into_iter()
                .map(|i| (i.node_id, i.claimed))
                .collect();
            open.sort();
            all.push((run.execution_id.0.to_string(), open));
        }
        all.sort();
        all
    }

    async fn completed(&self, run: &ExecutionId) -> Vec<String> {
        self.events(run)
            .await
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NodeCompleted { node_id, .. } => Some(node_id.to_string()),
                _ => None,
            })
            .collect()
    }

    async fn scheduled(&self, run: &ExecutionId) -> Vec<String> {
        self.events(run)
            .await
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NodeScheduled { node_id, .. } => Some(node_id.to_string()),
                _ => None,
            })
            .collect()
    }

    async fn count(&self, run: &ExecutionId, wanted: fn(&EventKind) -> bool) -> usize {
        self.events(run)
            .await
            .iter()
            .filter(|e| wanted(&e.kind))
            .count()
    }

    /// When the run's one approval request expires, as the request records it.
    async fn deadline(&self, run: &ExecutionId) -> chrono::DateTime<chrono::Utc> {
        let events = self.events(run).await;
        let context = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolApprovalRequired { context, .. } => Some(context.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: the run has no approval request", self.name()));
        context["expires_at"]
            .as_str()
            .and_then(|at| at.parse().ok())
            .unwrap_or_else(|| panic!("{}: the request has no deadline: {context}", self.name()))
    }

    async fn status(&self, run: &ExecutionId) -> WorkflowStatus {
        self.inner
            .get_execution(run)
            .await
            .unwrap()
            .expect("the run exists")
            .status
    }

    /// The tick row: `None` until a tick has begun.
    async fn row(&self) -> Option<jamjet_state::TickStatus> {
        self.inner.tick_status().await.unwrap()
    }

    /// Every invariant of the store (on a file or a database, not memory).
    async fn assert_invariants(&self) {
        let Some(store) = &self.store else { return };
        let broken = invariants::check(store).await;
        assert!(
            broken.is_empty(),
            "{}: invariants broken:\n{}",
            self.name(),
            broken
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// A second engine on the same store, with a probe of its own.
    fn second_engine(&self) -> (Arc<Driver>, Arc<Probe>, jamjet_worker::ShutdownTrigger) {
        let probe = Arc::new(Probe::new());
        let backend: Arc<dyn StateBackend> =
            Arc::new(Intercept::new(self.inner.clone(), probe.clone()));
        let (_, driver, trigger) = engine_on(
            backend,
            self.pg.as_ref(),
            &Options::default().config,
            Arc::new(ModelRegistry::new()),
        );
        (driver, probe, trigger)
    }
}

/// The server's state on `backend`, and a driver built as `main` builds it.
fn engine_on(
    backend: Arc<dyn StateBackend>,
    pg: Option<&PgBackend>,
    config: &DriverConfig,
    models: Arc<ModelRegistry>,
) -> (AppState, Arc<Driver>, jamjet_worker::ShutdownTrigger) {
    // On Postgres the audit log is the database's, so the invariants check
    // the chain the engine wrote.
    let (audit, enricher): (Arc<dyn AuditBackend>, Arc<AuditEnricher>) = match pg {
        Some(pg) => {
            let audit: Arc<dyn AuditBackend> = Arc::new(PgAuditBackend::new(pg.pool()));
            let enricher = Arc::new(AuditEnricher::with_signer(
                audit.clone(),
                AuditSigner::new(invariants::AUDIT_KEY),
            ));
            (audit, enricher)
        }
        None => {
            let audit: Arc<dyn AuditBackend> = Arc::new(NoopAuditBackend);
            let enricher = Arc::new(AuditEnricher::new(audit.clone()));
            (audit, enricher)
        }
    };
    let for_routes = backend.clone();
    let state = AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_t: &TenantId| for_routes.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    let (trigger, shutdown) = jamjet_worker::shutdown();
    let mut driver =
        Driver::new(&state, models, shutdown, *config).with_background(Background::Off);
    if let Some(pg) = pg {
        driver = driver.with_outbox(pg.clone());
    }
    (state, Arc::new(driver), trigger)
}

/// Wait until `at` has passed.
async fn sleep_until(at: chrono::DateTime<chrono::Utc>) {
    if let Ok(left) = (at - chrono::Utc::now()).to_std() {
        tokio::time::sleep(left).await;
    }
}

fn kind_name(event: &Event) -> String {
    format!("{:?}", event.kind)
        .split([' ', '{', '('])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn parse_id(api: &str) -> ExecutionId {
    ExecutionId(uuid::Uuid::parse_str(api.trim_start_matches("exec_")).unwrap())
}

/// What a request got back.
struct Reply {
    status: StatusCode,
    retry_after: Option<String>,
    allow: Option<String>,
    body: Value,
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let resp = app
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    let allow = resp
        .headers()
        .get("allow")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        retry_after,
        allow,
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    }
}

// ── Workflows ────────────────────────────────────────────────────────────────

fn node(id: &str, kind: Value) -> Value {
    json!({
        "id": id, "kind": kind, "retry_policy": null, "node_timeout_secs": null,
        "description": null, "labels": {}
    })
}

/// A `python_fn` node: an external worker's step.
fn py(id: &str) -> Value {
    node(
        id,
        json!({ "type": "python_fn", "module": "app.steps", "function": id, "output_schema": "" }),
    )
}

fn cond(id: &str) -> Value {
    node(id, json!({ "type": "condition", "branches": [] }))
}

/// An approval step whose request expires after `secs`, with a fallback.
fn review(id: &str, secs: Option<u64>, fallback: Option<&str>) -> Value {
    node(
        id,
        json!({ "type": "human_approval", "description": "Ship it?", "timeout_secs": secs,
                "fallback_node": fallback }),
    )
}

/// The workflow `id` with start node `s`, these nodes and these edges, and
/// an edge from every node nothing leads out of to the end.
fn workflow(id: &str, nodes: Vec<Value>, edges: &[(&str, &str)]) -> Value {
    let by_id: serde_json::Map<String, Value> = nodes
        .into_iter()
        .map(|n| (n["id"].as_str().unwrap().to_string(), n))
        .collect();
    let mut all: Vec<Value> = edges
        .iter()
        .map(|(from, to)| json!({ "from": from, "to": to, "condition": null }))
        .collect();
    for node in by_id.keys() {
        if !edges.iter().any(|(from, _)| from == node) {
            all.push(json!({ "from": node, "to": "end", "condition": null }));
        }
    }
    json!({
        "workflow_id": id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "s",
        "nodes": by_id, "edges": all,
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// `s` (a worker's step) then `p` (another).
fn two_steps(id: &str) -> Value {
    workflow(id, vec![py("s"), py("p")], &[("s", "p")])
}

// ── Who may tick ─────────────────────────────────────────────────────────────

/// With the loops off a tick makes the engine act, so a request with no
/// token, a revoked one, an expired one or a made-up one is refused, and
/// the engine does nothing: no tick row, no call to take the lease, runs as
/// they were.
#[tokio::test]
async fn an_unauthenticated_tick_is_401_and_changes_nothing() {
    let env = Env::new(Kind::Sqlite, "an_unauthenticated_tick")
        .await
        .unwrap();
    env.register(&two_steps("wf-auth")).await;
    let run = env.start("wf-auth").await;
    env.complete(&run, "s", "python_tool").await;

    // A token that worked, then was revoked, and one that expired.
    let sqlite = env.sqlite.as_ref().unwrap();
    let (revoked, info) = env.inner.create_token("old-ops", "operator").await.unwrap();
    let before = env.tick_with(&revoked, "{}").await;
    assert_eq!(before.status, StatusCode::OK, "good before it is revoked");
    sqlite.revoke_token(&info.id).await.unwrap().unwrap();
    let (expired, info) = env
        .inner
        .create_token("late-ops", "operator")
        .await
        .unwrap();
    sqlx::query("UPDATE api_tokens SET expires_at = ? WHERE id = ?")
        .bind((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339())
        .bind(&info.id)
        .execute(&sqlite.pool())
        .await
        .unwrap();
    // What the good token's tick left is the baseline.
    let row_before = env.row().await;
    let begins_before = env.probe.begins.load(Ordering::SeqCst);
    let log_before = env.log().await;
    let items_before = env.items().await;

    let cases: [(&str, Option<&str>); 6] = [
        ("no token", None),
        ("a made-up token", Some("jj_not_a_token")),
        ("a revoked operator token", Some(&revoked)),
        ("an expired operator token", Some(&expired)),
        ("an empty token", Some("")),
        // The scheme is Bearer; the token alone is no credential.
        ("a token without its scheme", Some("raw")),
    ];
    for (what, token) in cases {
        let mut request = Request::builder().method("POST").uri("/ops/tick");
        if let Some(token) = token {
            let value = if what == "a token without its scheme" {
                token.to_string()
            } else {
                format!("Bearer {token}")
            };
            request = request.header("authorization", value);
        }
        let resp = env
            .app()
            .oneshot(request.body(Body::from("{}")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{what}");
    }
    assert_eq!(env.row().await, row_before, "the tick row did not move");
    assert_eq!(
        env.probe.begins.load(Ordering::SeqCst),
        begins_before,
        "no tick was begun"
    );
    assert_eq!(env.log().await, log_before, "no event was written");
    assert_eq!(env.items().await, items_before, "no item was touched");
}

impl Env {
    /// `POST /ops/tick` with `token` as given.
    async fn tick_with(&self, token: &str, body: &str) -> Reply {
        send(self.app(), "POST", "/ops/tick", Some(token), Some(body)).await
    }
}

/// A token that is good for everything else is not good for this: only an
/// operator's, in the engine's tenant, makes it act.
#[tokio::test]
async fn non_operator_tokens_get_403() {
    let env = Env::new(Kind::Sqlite, "non_operator_tokens").await.unwrap();
    // An operator of another tenant: valid, not revoked, not expired.
    let sqlite = env.sqlite.as_ref().unwrap();
    let now = chrono::Utc::now();
    sqlite
        .for_tenant(TenantId::default())
        .create_tenant(Tenant {
            id: TenantId::from("acme".to_string()),
            name: "Acme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let (other, info) = env
        .inner
        .create_token("acme-ops", "operator")
        .await
        .unwrap();
    sqlx::query("UPDATE api_tokens SET tenant_id = 'acme' WHERE id = ?")
        .bind(&info.id)
        .execute(&sqlite.pool())
        .await
        .unwrap();
    env.register(&two_steps("wf-403")).await;
    let run = env.start("wf-403").await;
    env.complete(&run, "s", "python_tool").await;
    let log = env.log().await;
    let items = env.items().await;

    for (who, token) in [
        ("a developer", &env.alice),
        ("a reviewer", &env.bob),
        ("a viewer", &env.viewer),
        ("an operator of another tenant", &other),
    ] {
        let reply = env.tick_with(token, "{}").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{who}: {}", reply.body);
        assert_ne!(reply.body["ran"], true, "{who}");
    }
    assert!(env.row().await.is_none(), "no tick row was written");
    assert_eq!(env.probe.begins.load(Ordering::SeqCst), 0);
    assert_eq!(env.log().await, log, "no event was written");
    assert_eq!(env.items().await, items, "no item was touched");

    // The operator's token, the same request: it ticks.
    let report = env.tick_ran("{}").await;
    assert_eq!(report["runs"]["visited"], 1, "{report}");
    assert!(env.row().await.is_some());
    assert_eq!(env.probe.begins.load(Ordering::SeqCst), 1);
}

/// The route takes POST and a body that is empty, `{}` or a `budget_ms`
/// from 100 up. A larger budget is held to the configured one.
#[tokio::test]
async fn bad_requests_are_refused() {
    let env = Env::new(Kind::Sqlite, "bad_requests").await.unwrap();
    let ops = &env.ops;
    let begins = || env.probe.begins.load(Ordering::SeqCst);

    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let reply = send(env.app(), method, "/ops/tick", Some(ops), None).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(reply.allow.as_deref(), Some("POST"), "{method}");
    }
    for (what, body) in [
        ("an unknown field", r#"{"budget_ms":200,"run":"x"}"#),
        ("a field that names a run", r#"{"execution_id":"exec_0"}"#),
        ("a zero budget", r#"{"budget_ms":0}"#),
        ("a budget under the least", r#"{"budget_ms":99}"#),
        ("a negative budget", r#"{"budget_ms":-5}"#),
        ("a fractional budget", r#"{"budget_ms":150.5}"#),
        ("a budget as text", r#"{"budget_ms":"200"}"#),
        ("a JSON null", "null"),
        ("an array", "[]"),
        ("not JSON", "tick now"),
    ] {
        let reply = send(env.app(), "POST", "/ops/tick", Some(ops), Some(body)).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{what}: {}",
            reply.body
        );
        assert!(reply.body["error"].is_string(), "{what}: {}", reply.body);
    }
    assert!(env.row().await.is_none(), "a refused request ticked");
    assert_eq!(begins(), 0, "a refused request took the lease");

    // What is accepted: nothing, white space, `{}`, an explicit null.
    for body in ["", "  \n", "{}", r#"{"budget_ms":null}"#] {
        let reply = send(env.app(), "POST", "/ops/tick", Some(ops), Some(body)).await;
        assert_eq!(reply.status, StatusCode::OK, "{body:?}: {}", reply.body);
        assert_eq!(
            reply.body["budget_ms"], CONFIGURED_TICK_MS,
            "{body:?}: the configured budget"
        );
    }
    // No body at all, and no content type.
    let reply = send(env.app(), "POST", "/ops/tick", Some(ops), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(begins(), 5);

    // A budget is a limit the caller may lower and not raise.
    let low = env.tick_ran(r#"{"budget_ms":150}"#).await;
    assert_eq!(low["budget_ms"], 150, "{low}");
    let least = env.tick_ran(r#"{"budget_ms":100}"#).await;
    assert_eq!(least["budget_ms"], 100, "{least}");
    let huge = env.tick_ran(r#"{"budget_ms":18446744073709551615}"#).await;
    assert_eq!(
        huge["budget_ms"], CONFIGURED_TICK_MS,
        "a huge budget is held to the configured one"
    );
    let large = env.tick_ran(r#"{"budget_ms":6000000}"#).await;
    assert_eq!(large["budget_ms"], CONFIGURED_TICK_MS, "{large}");
}

/// Dev mode has no auth layer, and its tick needs no token.
#[tokio::test]
async fn dev_mode_ticks_without_a_token() {
    let env = Env::new(Kind::Memory, "dev_mode_ticks").await.unwrap();
    let reply = send(env.dev_app(), "POST", "/ops/tick", None, Some("{}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.body["ran"], true, "{}", reply.body);
    assert_eq!(env.probe.begins.load(Ordering::SeqCst), 1);
    // Outside dev mode the same request is refused: the lack of a token is
    // never permission.
    let reply = send(env.app(), "POST", "/ops/tick", None, Some("{}")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.probe.begins.load(Ordering::SeqCst), 1);
}

/// An engine with no driver (the loops on and no workers of its own) says so
/// rather than answering as if it had ticked.
#[tokio::test]
async fn an_engine_without_a_driver_cannot_tick() {
    let env = Env::new(Kind::Memory, "an_engine_without_a_driver")
        .await
        .unwrap();
    let app = build_router_with_opts(env.state.clone(), false);
    let reply = send(app, "POST", "/ops/tick", Some(&env.ops), Some("{}")).await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert!(reply.body["error"].is_string());
    assert!(env.row().await.is_none());
}

/// A store that cannot coordinate ticks is a 500, and one that cannot be
/// reached a 503 to try again; either way the engine's own lock is let go, so
/// the next tick can run.
#[tokio::test]
async fn a_store_that_cannot_take_the_lease_answers_500_or_503() {
    let env = Env::new(Kind::Memory, "a_store_that_cannot_take_the_lease")
        .await
        .unwrap();
    env.probe.begin_fault.store(1, Ordering::SeqCst);
    let reply = env.tick_with(&env.ops, "{}").await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.body
    );
    assert!(
        reply.body["error"]
            .as_str()
            .is_some_and(|e| e.contains("cannot coordinate ticks")),
        "{}",
        reply.body
    );
    assert_eq!(reply.retry_after, None, "a refusal is not worth a retry");

    env.probe.begin_fault.store(2, Ordering::SeqCst);
    let reply = env.tick_with(&env.ops, "{}").await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(reply.retry_after.as_deref(), Some("1"));

    assert!(env.row().await.is_none(), "nothing was written");
    env.probe.begin_fault.store(0, Ordering::SeqCst);
    env.tick_ran("{}").await;
    assert_eq!(env.probe.begins.load(Ordering::SeqCst), 3);
}

// ── What a tick does ─────────────────────────────────────────────────────────

/// Nothing to do: it answers at once, appends no event and touches no item.
/// It writes its own row.
#[tokio::test]
async fn a_tick_with_no_work_answers_fast_and_writes_only_its_row() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_tick_with_no_work").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-idle")).await;
        for _ in 0..3 {
            env.start("wf-idle").await;
        }
        let log = env.log().await;
        let items = env.items().await;
        assert!(env.row().await.is_none(), "{name}: no tick has begun");

        let began = Instant::now();
        let report = env.tick_ran("{}").await;
        let took = began.elapsed();

        assert!(took < Duration::from_millis(200), "{name}: {took:?}");
        assert_eq!(report["complete"], true, "{name}: {report}");
        assert_eq!(report["over_budget_ms"], 0, "{name}: {report}");
        assert_eq!(report["budget_ms"], CONFIGURED_TICK_MS, "{name}: {report}");
        assert_eq!(report["background"], "off", "{name}: {report}");
        assert_eq!(report["next_after"], Value::Null, "{name}: {report}");
        assert_eq!(report["projected"], 0, "{name}: {report}");
        assert_eq!(
            report["runs"],
            json!({ "visited": 3, "advanced": 0, "stale_passes": 0, "errors": 0 }),
            "{name}: {report}"
        );
        assert_eq!(
            report["reclaimed"],
            json!({ "retried": 0, "dead_lettered": 0 }),
            "{name}"
        );
        assert_eq!(
            report["engine_steps"],
            json!({ "quick": 0, "slow": 0 }),
            "{name}"
        );
        assert_eq!(
            report["deadlines"],
            json!({ "expired": 0, "escalated": 0 }),
            "{name}"
        );
        assert_eq!(
            report["audit_outbox"],
            json!({ "sealed": 0, "failed": 0 }),
            "{name}"
        );
        assert_eq!(env.log().await, log, "{name}: no event was appended");
        assert_eq!(env.items().await, items, "{name}: no item was touched");

        // Its row: free again, with the report it kept.
        let row = env
            .row()
            .await
            .unwrap_or_else(|| panic!("{name}: no tick row"));
        assert_eq!(row.holder, None, "{name}: the lease is given back");
        assert!(row.last_finished_at.is_some(), "{name}");
        assert_eq!(row.last_complete, Some(true), "{name}");
        assert_eq!(row.cursor, None, "{name}");
        assert_eq!(
            row.last_report.as_ref().unwrap()["runs"]["visited"],
            3,
            "{name}"
        );
        env.assert_invariants().await;
    }
}

/// Two ticks at the same moment on one engine: one runs, the other is told
/// the engine is busy and does not reach the store.
#[tokio::test]
async fn two_ticks_at_once_on_one_engine_one_runs_one_is_busy() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "two_ticks_at_once_on_one_engine").await else {
            continue;
        };
        let name = env.name();
        let env = Arc::new(env);
        env.register(&two_steps("wf-busy")).await;
        env.start("wf-busy").await;

        env.probe.hold_first_list.store(true, Ordering::SeqCst);
        let first = {
            let env = env.clone();
            tokio::spawn(async move { env.tick("{}").await })
        };
        tokio::time::timeout(Duration::from_secs(20), env.probe.entered.notified())
            .await
            .unwrap_or_else(|_| panic!("{name}: the first tick never reached the walk"));
        // The race opened: the first tick holds the lease and is mid-walk.
        let held = env.row().await.expect("a row").holder;
        assert!(held.is_some(), "{name}: the first tick holds the lease");

        let (st, busy) = env.tick("{}").await;
        assert_eq!(st, StatusCode::OK, "{name}: {busy}");
        assert_eq!(busy["ran"], false, "{name}: {busy}");
        assert_eq!(busy["reason"], "busy", "{name}: {busy}");
        assert_eq!(
            busy["held_by"], "this instance",
            "{name}: refused by the engine's own lock, not by the store: {busy}"
        );
        assert_eq!(
            env.probe.begins.load(Ordering::SeqCst),
            1,
            "{name}: the second tick never asked the store"
        );

        env.probe.release.add_permits(1);
        let (st, report) = first.await.unwrap();
        assert_eq!(st, StatusCode::OK, "{name}: {report}");
        assert_eq!(report["ran"], true, "{name}: {report}");
        assert_eq!(env.row().await.unwrap().holder, None, "{name}");
        // The lock is free again.
        env.tick_ran("{}").await;
        env.assert_invariants().await;
    }
}

/// Two engines on one store: the store's lease lets one tick run. The other
/// is told who holds it and until when.
#[tokio::test]
async fn two_ticks_at_once_on_two_engines_one_runs_one_is_busy() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "two_ticks_at_once_on_two_engines").await else {
            continue;
        };
        let name = env.name();
        let env = Arc::new(env);
        let (other, other_probe, _other_trigger) = env.second_engine();
        env.register(&two_steps("wf-two")).await;
        env.start("wf-two").await;

        env.probe.hold_first_list.store(true, Ordering::SeqCst);
        let first = {
            let env = env.clone();
            tokio::spawn(async move { env.tick("{}").await })
        };
        tokio::time::timeout(Duration::from_secs(20), env.probe.entered.notified())
            .await
            .unwrap_or_else(|_| panic!("{name}: the first tick never reached the walk"));
        let holder = env
            .row()
            .await
            .unwrap()
            .holder
            .expect("the first tick holds the lease");

        let TickResult::Busy {
            held_by,
            lease_until,
        } = other.tick(Duration::from_secs(5)).await.unwrap()
        else {
            panic!("{name}: the second engine ticked while the first held the lease");
        };
        assert_eq!(held_by, holder, "{name}: the store names the holder");
        assert_ne!(held_by, "this instance", "{name}");
        // The lease is the first tick's budget (the configured 60 s) and the
        // two minutes allowed for a slow step.
        let until = lease_until.expect("the store says when the lease ends");
        let left = (until - chrono::Utc::now()).num_seconds();
        assert!((170..=180).contains(&left), "{name}: {left} s left");
        assert_eq!(other_probe.begins.load(Ordering::SeqCst), 1, "{name}");

        env.probe.release.add_permits(1);
        let (_, report) = first.await.unwrap();
        assert_eq!(report["ran"], true, "{name}: {report}");
        // Free again: the second engine's tick runs.
        let TickResult::Ran(report) = other.tick(Duration::from_secs(5)).await.unwrap() else {
            panic!("{name}: the lease was given back and the second engine could not take it");
        };
        assert!(report.ran, "{name}");
        env.assert_invariants().await;
    }
}

/// A tick, then a tick: the second records nothing. Between them an approval
/// passes its deadline, and the next tick expires it, once.
#[tokio::test]
async fn a_repeated_tick_records_nothing_twice() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_repeated_tick_records_nothing_twice").await else {
            continue;
        };
        let name = env.name();
        env.register(&workflow(
            "wf-chain",
            vec![
                py("s"),
                cond("c1"),
                cond("c2"),
                review("review", Some(4), None),
                py("publish"),
            ],
            &[
                ("s", "c1"),
                ("c1", "c2"),
                ("c2", "review"),
                ("review", "publish"),
            ],
        ))
        .await;
        let a = env.start("wf-chain").await;
        let b = env.start("wf-chain").await;
        let waiting = env.start("wf-chain").await;
        env.complete(&a, "s", "python_tool").await;
        env.complete(&b, "s", "python_tool").await;

        // The first tick does the work: two conditions and a park, per run.
        let first = env.tick_ran("{}").await;
        assert_eq!(first["engine_steps"]["quick"], 6, "{name}: {first}");
        assert_eq!(first["runs"]["advanced"], 2, "{name}: {first}");
        for run in [&a, &b] {
            assert_eq!(
                env.count(run, |k| matches!(k, EventKind::ToolApprovalRequired { .. }))
                    .await,
                1,
                "{name}: parked once"
            );
        }
        assert_eq!(env.scheduled(&waiting).await, ["s"], "{name}");

        // The second changes nothing: its approvals have time left. (A
        // slow store can use up the seconds: say so, not "wrote".)
        let first_deadline = env.deadline(&a).await.min(env.deadline(&b).await);
        assert!(
            chrono::Utc::now() < first_deadline,
            "{name}: the first tick took longer than the approvals' 4 s"
        );
        let log = env.log().await;
        let items = env.items().await;
        let second = env.tick_ran("{}").await;
        assert_eq!(second["runs"]["advanced"], 0, "{name}: {second}");
        assert_eq!(
            second["engine_steps"],
            json!({ "quick": 0, "slow": 0 }),
            "{name}"
        );
        assert_eq!(env.log().await, log, "{name}: the second tick wrote");
        assert_eq!(env.items().await, items, "{name}");
        env.assert_invariants().await;

        // Past the deadline the next tick expires each request, and the
        // runs fail with no fallback to take.
        let last_deadline = env.deadline(&a).await.max(env.deadline(&b).await);
        sleep_until(last_deadline + chrono::Duration::milliseconds(200)).await;
        let third = env.tick_ran("{}").await;
        assert_eq!(third["deadlines"]["expired"], 2, "{name}: {third}");
        for run in [&a, &b] {
            assert_eq!(
                env.count(run, |k| matches!(k, EventKind::ApprovalExpired { .. }))
                    .await,
                1,
                "{name}: expired once"
            );
            assert_eq!(env.status(run).await, WorkflowStatus::Failed, "{name}");
        }
        // And the one after finds nothing left to do.
        let log = env.log().await;
        let fourth = env.tick_ran("{}").await;
        assert_eq!(
            fourth["deadlines"],
            json!({ "expired": 0, "escalated": 0 }),
            "{name}: {fourth}"
        );
        assert_eq!(
            fourth["runs"]["visited"], 1,
            "{name}: only the waiting run is left"
        );
        assert_eq!(env.log().await, log, "{name}: the fourth tick wrote");
        env.assert_invariants().await;
    }
}

/// A budget that cuts the walk stores where it stopped, and the next tick
/// goes on from there, round the runs in id order: every run is visited
/// within two cycles.
#[tokio::test]
async fn a_tick_cut_by_its_budget_resumes_where_it_stopped() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_tick_cut_by_its_budget").await else {
            continue;
        };
        let name = env.name();
        // Each start and completion is a transaction: Postgres, shared with
        // other builds, gets fewer runs to start.
        let total = if kind == Kind::Postgres { 150 } else { 600 };
        env.register(&two_steps("wf-many")).await;
        let mut runs = Vec::new();
        for _ in 0..total {
            runs.push(env.start("wf-many").await);
        }
        for run in &runs {
            env.complete(run, "s", "python_tool").await;
        }
        let mut ids: Vec<String> = runs.iter().map(|r| r.0.to_string()).collect();
        ids.sort();
        // A slow store: each run's advance takes a few milliseconds, so the
        // walk cannot fit in the budget.
        env.probe.delay_ms.store(4, Ordering::SeqCst);

        let mut previous: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        let mut visited = 0;
        let mut ticks = 0;
        while seen.len() < total {
            ticks += 1;
            assert!(ticks < 400, "{name}: the walk never got through the runs");
            env.probe.forget();
            let report = env.tick_ran(r#"{"budget_ms":400}"#).await;
            visited += report["runs"]["visited"].as_u64().unwrap();
            let mut order = env.probe.visits();
            order.dedup();
            seen.extend(order.iter().cloned());
            let next = report["next_after"].as_str().map(str::to_string);

            // It began where the last tick stopped: the first read of the
            // running runs is after that cursor, and the first run advanced
            // is the next one in id order (the first, past the last).
            assert_eq!(env.probe.lists()[0], previous, "{name}: tick {ticks}");
            let first = match &previous {
                None => 0,
                Some(p) => (ids.iter().position(|id| id == p).unwrap() + 1) % total,
            };
            assert_eq!(order[0], ids[first], "{name}: tick {ticks}");
            // What it reports is what the store keeps.
            assert_eq!(
                env.row().await.unwrap().cursor,
                next,
                "{name}: tick {ticks}"
            );
            if ticks == 1 {
                assert_eq!(
                    report["complete"], false,
                    "{name}: cut by its budget: {report}"
                );
                assert!(next.is_some(), "{name}: {report}");
                assert!(
                    order.len() < total,
                    "{name}: the budget did not cut the walk"
                );
            }
            previous = next;
        }
        assert!(ticks >= 2, "{name}: the budget never cut the walk");
        assert!(
            visited <= 2 * total as u64,
            "{name}: {visited} visits for {total} runs"
        );
        for run in &runs {
            assert_eq!(
                env.scheduled(run).await,
                ["s", "p"],
                "{name}: every run was advanced"
            );
        }
        env.assert_invariants().await;
    }
}

/// A walk resumes after its cursor and wraps to the start, so every run is
/// visited whatever the cursor was.
#[tokio::test]
async fn a_tick_resumes_after_its_cursor_and_wraps_to_the_start() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_tick_resumes_after_its_cursor").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-wrap")).await;
        let mut runs = Vec::new();
        for _ in 0..12 {
            runs.push(env.start("wf-wrap").await);
        }
        for run in &runs {
            env.complete(run, "s", "python_tool").await;
        }
        let mut ids: Vec<String> = runs.iter().map(|r| r.0.to_string()).collect();
        ids.sort();
        // The last tick stopped after the sixth run.
        let cursor = ids[5].clone();
        env.inner
            .begin_tick("an-earlier-tick", Duration::from_secs(60))
            .await
            .unwrap();
        assert!(env
            .inner
            .finish_tick(
                "an-earlier-tick",
                &jamjet_state::TickOutcome {
                    cursor: Some(cursor.clone()),
                    complete: false,
                    report: json!({}),
                },
            )
            .await
            .unwrap());

        env.probe.forget();
        let report = env.tick_ran("{}").await;
        // An advance reads its run more than once.
        let mut visits = env.probe.visits();
        visits.dedup();
        assert_eq!(report["runs"]["visited"], 12, "{name}: {report}");
        assert_eq!(report["complete"], true, "{name}: {report}");
        assert_eq!(report["next_after"], Value::Null, "{name}: {report}");
        let mut expected: Vec<String> = ids[6..].to_vec();
        expected.extend_from_slice(&ids[..6]);
        assert_eq!(
            visits, expected,
            "{name}: after the cursor first, then from the start up to it"
        );
        assert_eq!(env.probe.lists()[0], Some(cursor), "{name}");
        assert_eq!(env.row().await.unwrap().cursor, None, "{name}");
        env.assert_invariants().await;
    }
}

/// However little budget it has, a tick advances at least one run and moves
/// its cursor on, so a run that is slow cannot hold the tick in place.
#[tokio::test]
async fn every_tick_makes_progress() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "every_tick_makes_progress").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-slow-run")).await;
        let mut runs = Vec::new();
        for _ in 0..3 {
            runs.push(env.start("wf-slow-run").await);
        }
        for run in &runs {
            env.complete(run, "s", "python_tool").await;
        }
        let mut ids: Vec<String> = runs.iter().map(|r| r.0.to_string()).collect();
        ids.sort();

        // The first run's pass hangs for a second; the budget is a tenth.
        *env.probe.hang.lock().unwrap() = Some((ids[0].clone(), Duration::from_millis(1000)));
        env.probe.forget();
        let report = env.tick_ran(r#"{"budget_ms":100}"#).await;
        assert_eq!(report["runs"]["visited"], 1, "{name}: {report}");
        assert_eq!(report["next_after"], ids[0], "{name}: {report}");
        assert_eq!(report["complete"], false, "{name}: {report}");
        assert!(
            report["over_budget_ms"].as_u64().unwrap() >= 800,
            "{name}: it ran past its budget waiting for the run: {report}"
        );
        assert_eq!(
            env.row().await.unwrap().cursor,
            Some(ids[0].clone()),
            "{name}"
        );

        // The next goes on from there, and moves on again.
        env.probe.forget();
        let report = env.tick_ran(r#"{"budget_ms":100}"#).await;
        assert_eq!(
            env.probe.visits().first(),
            Some(&ids[1]),
            "{name}: the second tick begins with the second run: {report}"
        );
        assert_ne!(
            env.row().await.unwrap().cursor,
            Some(ids[0].clone()),
            "{name}"
        );
    }
}

/// The budget spent before the walk begins (a slow sweep of leases) still
/// leaves one run advanced.
#[tokio::test]
async fn a_tick_whose_budget_is_gone_before_the_walk_still_advances_one_run() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_tick_whose_budget_is_gone").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-late-walk")).await;
        let mut runs = Vec::new();
        for _ in 0..3 {
            runs.push(env.start("wf-late-walk").await);
        }
        for run in &runs {
            env.complete(run, "s", "python_tool").await;
        }
        let mut ids: Vec<String> = runs.iter().map(|r| r.0.to_string()).collect();
        ids.sort();
        env.probe.reclaim_ms.store(300, Ordering::SeqCst);
        let report = env.tick_ran(r#"{"budget_ms":100}"#).await;
        assert_eq!(report["runs"]["visited"], 1, "{name}: {report}");
        assert_eq!(report["next_after"], ids[0], "{name}: {report}");
        assert_eq!(report["complete"], false, "{name}: {report}");
    }
}

/// The engine's own model call runs in the tick, which waits for it past its
/// budget and says how far past.
#[tokio::test]
async fn a_tick_runs_engine_model_steps_and_waits_for_them() {
    for kind in kinds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let models = Arc::new(
            ModelRegistry::new()
                .register(Arc::new(SlowModel {
                    calls: calls.clone(),
                    delay: Duration::from_millis(500),
                }))
                .with_default("slow"),
        );
        let options = Options {
            models,
            ..Options::default()
        };
        let Some(env) = Env::with(kind, "a_tick_runs_engine_model_steps", options).await else {
            continue;
        };
        let name = env.name();
        let ir: Value = serde_json::from_str(include_str!("fixtures/hello_agent_ir.json")).unwrap();
        env.register(&ir).await;
        let run = {
            let (st, body) = support::post(
                &env.state,
                "/executions",
                json!({ "workflow_id": "demo", "workflow_version": "0.1.0",
                        "input": { "query": "Capital of France?" } }),
            )
            .await;
            assert!(st.is_success(), "{name}: {st} {body}");
            parse_id(body["execution_id"].as_str().unwrap())
        };
        assert!(env.completed(&run).await.is_empty(), "{name}");

        let report = env.tick_ran(r#"{"budget_ms":100}"#).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the model was called once"
        );
        assert_eq!(
            env.completed(&run).await,
            ["think"],
            "{name}: the model node completed"
        );
        assert_eq!(
            report["engine_steps"],
            json!({ "quick": 0, "slow": 1 }),
            "{name}: {report}"
        );
        assert!(
            report["over_budget_ms"].as_u64().unwrap() >= 300,
            "{name}: the tick waited for the call, past its 100 ms: {report}"
        );
        assert!(
            report["elapsed_ms"].as_u64().unwrap() >= 450,
            "{name}: {report}"
        );
        // The engine ran it, as itself.
        let workers: Vec<String> = env
            .events(&run)
            .await
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NodeStarted { worker_id, .. } => Some(worker_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(workers.len(), 1, "{name}: {workers:?}");
        assert!(workers[0].starts_with("engine-"), "{name}: {workers:?}");

        // The next tick finishes the run without calling the model again.
        let report = env.tick_ran("{}").await;
        assert_eq!(
            report["engine_steps"],
            json!({ "quick": 0, "slow": 0 }),
            "{name}: {report}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
        assert_eq!(env.status(&run).await, WorkflowStatus::Completed, "{name}");
        env.assert_invariants().await;
    }
}

/// A decision after the deadline is refused, and the tick that follows
/// applies the fallback: the approval completes as expired, and the fallback
/// node runs, once.
#[tokio::test]
async fn a_late_decision_is_refused_and_the_next_tick_applies_the_fallback() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_late_decision_is_refused").await else {
            continue;
        };
        let name = env.name();
        // `hold` has no edge into it: it runs when `review` falls back.
        env.register(&workflow(
            "wf-late",
            vec![
                py("s"),
                review("review", Some(1), Some("hold")),
                py("publish"),
                cond("hold"),
            ],
            &[("s", "review"), ("review", "publish")],
        ))
        .await;
        let app = env.app();
        let alice = Some(env.alice.as_str());
        let started = send(
            app.clone(),
            "POST",
            "/executions",
            alice,
            Some(r#"{"workflow_id":"wf-late","workflow_version":"0.1.0","input":{}}"#),
        )
        .await;
        assert_eq!(
            started.status,
            StatusCode::CREATED,
            "{name}: {}",
            started.body
        );
        let run = parse_id(started.body["execution_id"].as_str().unwrap());
        env.complete(&run, "s", "python_tool").await;

        // The tick parks the approval, with a second to decide.
        env.tick_ran("{}").await;
        let requested = env.events(&run).await;
        let context = requested
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolApprovalRequired { context, .. } => Some(context.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: the approval was requested"));
        let digest = context["action_digest"].as_str().unwrap().to_string();

        // Nobody ticks, and the second passes.
        sleep_until(env.deadline(&run).await + chrono::Duration::milliseconds(200)).await;
        let late = send(
            app.clone(),
            "POST",
            &format!("/executions/exec_{}/approve", run.0.simple()),
            Some(&env.bob),
            Some(
                &json!({ "decision": "approved", "node_id": "review", "action_digest": digest })
                    .to_string(),
            ),
        )
        .await;
        assert_eq!(late.status, StatusCode::CONFLICT, "{name}: {}", late.body);
        assert_eq!(
            env.count(&run, |k| matches!(k, EventKind::ApprovalReceived { .. }))
                .await,
            0,
            "{name}: the late decision recorded nothing"
        );

        let report = env.tick_ran("{}").await;
        assert_eq!(report["deadlines"]["expired"], 1, "{name}: {report}");
        let completed = env.completed(&run).await;
        assert_eq!(
            completed,
            ["s", "review", "hold"],
            "{name}: the approval fell back and the fallback ran: {completed:?}"
        );
        assert_eq!(
            env.count(&run, |k| matches!(k, EventKind::ApprovalExpired { .. }))
                .await,
            1,
            "{name}"
        );
        assert_eq!(env.status(&run).await, WorkflowStatus::Completed, "{name}");

        // Another tick does not run it again.
        let log = env.log().await;
        env.tick_ran("{}").await;
        assert_eq!(env.log().await, log, "{name}: nothing is recorded twice");
        env.assert_invariants().await;
    }
}

/// A tick on Postgres seals the audit entries whose inline seal was lost.
#[tokio::test]
async fn a_tick_seals_what_the_audit_outbox_holds() {
    let Some(env) = Env::new(Kind::Postgres, "a_tick_seals_what_the_audit_outbox_holds").await
    else {
        return;
    };
    let pg = env.pg.as_ref().unwrap();
    env.register(&two_steps("wf-outbox")).await;
    let run = env.start("wf-outbox").await;
    // A request appended straight to the store, with no seal after it: what
    // a crash right after the append leaves. It is old enough to be due.
    pg.append_event(Event::new(
        run.clone(),
        0,
        EventKind::ToolApprovalRequired {
            node_id: "s".into(),
            tool_name: "s".into(),
            approver: "human".into(),
            context: json!({}),
        },
    ))
    .await
    .unwrap();
    let rows = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_outbox")
            .fetch_one(&pg.pool())
            .await
            .unwrap()
    };
    assert_eq!(rows().await, 1);
    sqlx::query("UPDATE audit_outbox SET created_at = created_at - interval '1 hour'")
        .execute(&pg.pool())
        .await
        .unwrap();

    let report = env.tick_ran("{}").await;
    assert_eq!(
        report["audit_outbox"],
        json!({ "sealed": 1, "failed": 0 }),
        "{report}"
    );
    assert_eq!(rows().await, 0, "the row is gone with its seal");
    let again = env.tick_ran("{}").await;
    assert_eq!(
        again["audit_outbox"],
        json!({ "sealed": 0, "failed": 0 }),
        "{again}"
    );
}

// ── Stopping ─────────────────────────────────────────────────────────────────

/// A server that is draining starts no tick: `503` with `Retry-After`, and
/// the store is not asked for a lease.
#[tokio::test]
async fn a_tick_while_draining_is_503() {
    let env = Env::new(Kind::Memory, "a_tick_while_draining")
        .await
        .unwrap();

    // The flag `main` sets before it waits for the workers.
    env.draining.0.store(true, Ordering::SeqCst);
    let reply = send(env.app(), "POST", "/ops/tick", Some(&env.ops), Some("{}")).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(reply.body["error"], "shutting down");
    assert_eq!(reply.retry_after.as_deref(), Some("1"));
    assert_eq!(env.probe.begins.load(Ordering::SeqCst), 0);
    assert!(env.row().await.is_none());
    env.draining.0.store(false, Ordering::SeqCst);
    env.tick_ran("{}").await;

    // The stop signal itself, with no flag: the driver refuses too.
    let trigger = env.trigger.lock().unwrap().take().unwrap();
    assert_eq!(trigger.stop_and_drain(Duration::ZERO).await, 0);
    let begins = env.probe.begins.load(Ordering::SeqCst);
    let reply = send(env.app(), "POST", "/ops/tick", Some(&env.ops), Some("{}")).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.body
    );
    assert_eq!(reply.retry_after.as_deref(), Some("1"));
    assert_eq!(
        env.probe.begins.load(Ordering::SeqCst),
        begins,
        "no lease was asked for"
    );
}

/// A stop signal that arrives mid-tick waits for it: the tick counts as
/// running work for the drain, and gives its lease back when it ends.
#[tokio::test]
async fn a_stop_signal_waits_for_a_tick_in_flight() {
    let env = Env::new(Kind::Memory, "a_stop_signal_waits_for_a_tick")
        .await
        .unwrap();
    let env = Arc::new(env);
    env.register(&two_steps("wf-stop")).await;
    env.start("wf-stop").await;
    env.probe.hold_first_list.store(true, Ordering::SeqCst);
    let tick = {
        let env = env.clone();
        tokio::spawn(async move { env.tick("{}").await })
    };
    tokio::time::timeout(Duration::from_secs(20), env.probe.entered.notified())
        .await
        .expect("the tick reached the walk");

    let trigger = env.trigger.lock().unwrap().take().unwrap();
    let still = trigger.stop_and_drain(Duration::from_millis(100)).await;
    assert!(
        still >= 1,
        "the drain did not see the tick running: {still}"
    );

    env.probe.release.add_permits(1);
    let (st, report) = tick.await.unwrap();
    assert_eq!(st, StatusCode::OK, "{report}");
    // Stopping, it advanced no run, and said it did not get through.
    assert_eq!(report["runs"]["visited"], 0, "{report}");
    assert_eq!(report["complete"], false, "{report}");
    assert_eq!(
        env.row().await.unwrap().holder,
        None,
        "the lease was given back"
    );
}

/// A client that gives up on the request does not cut the tick off: it
/// goes on to its end and gives its lease back.
#[tokio::test]
async fn a_tick_goes_on_when_its_caller_goes_away() {
    let env = Env::new(Kind::Memory, "a_tick_goes_on_when_its_caller_goes_away")
        .await
        .unwrap();
    let env = Arc::new(env);
    env.register(&two_steps("wf-gone")).await;
    env.start("wf-gone").await;
    env.probe.hold_first_list.store(true, Ordering::SeqCst);
    let call = {
        let env = env.clone();
        tokio::spawn(async move { env.tick("{}").await })
    };
    tokio::time::timeout(Duration::from_secs(20), env.probe.entered.notified())
        .await
        .expect("the tick reached the walk");
    // The caller hangs up: the request's future is dropped.
    call.abort();
    let _ = call.await;
    assert!(
        env.row().await.unwrap().holder.is_some(),
        "the lease is held"
    );
    env.probe.release.add_permits(1);
    // The tick finishes by itself and frees the lease.
    let began = Instant::now();
    loop {
        let row = env.row().await.unwrap();
        if row.holder.is_none() && row.last_finished_at.is_some() {
            break;
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "the tick never finished"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        env.row().await.unwrap().last_report.unwrap()["runs"]["visited"],
        1
    );
}
