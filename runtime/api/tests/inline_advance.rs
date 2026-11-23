//! Inline advance: what a write does for its run before it answers.
//!
//! With the background loops off, nothing in the process moves a run on its
//! own, and on a request-billed platform the process may get no CPU once it
//! has answered. So a request that changes a run (a start, a completion, a
//! failure, a decision, a cancel) takes that run one step further before it
//! answers: it builds the scheduler's pass for the run and runs the engine's
//! own quick steps, as `Driver::advance` does for a tick.
//!
//! The cases drive the routes through the router, the way a caller does, on
//! the memory store, a SQLite file and, when `JAMJET_TEST_POSTGRES_URL` is
//! set, Postgres. What matters most is what the advance must never do: run
//! before the write it follows has committed, change the answer, run with the
//! loops on, or run twice with a tick.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::driver::Driver;
use jamjet_api::routes::{build_router_with_opts, refuse_claims_while_draining, Draining};
use jamjet_api::startup::{Background, DriverConfig};
use jamjet_api::state::AppState;
use jamjet_audit::{AuditBackend, AuditEnricher, AuditSigner, NoopAuditBackend, PgAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_models::ModelRegistry;
use jamjet_state::backend::{StateBackend, StateBackendError};
use jamjet_state::event::EventKind;
use jamjet_state::faults::{Fault, Scope};
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

/// Counts, holds, fails and panics calls for a case. Does nothing until a case
/// arms it.
struct Probe {
    /// The run of each read of a log since a sequence: what a scheduler pass
    /// starts with, so one entry is one pass begun.
    reads: Mutex<Vec<String>>,
    /// The run of each `get_execution`.
    gets: Mutex<Vec<String>>,
    /// While set, the commit of a scheduler pass fails, as a store that lost
    /// its connection does. Counted in `failed_passes`.
    fail_passes: AtomicBool,
    failed_passes: AtomicUsize,
    /// While set, a claim by run and node fails: an engine step cannot take
    /// its item.
    fail_claims: AtomicBool,
    /// While set, the read of a log since a sequence panics.
    panic_reads: AtomicBool,
    /// Every read of a log since a sequence waits this long first: a wide
    /// window between a pass's read and its commit.
    read_delay_ms: AtomicU64,
    /// The first read of a log since a sequence says so on `entered` and
    /// waits for a permit on `release`.
    hold_read: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    /// Two reads of one run wait for each other, once.
    meeting: Mutex<Option<Meeting>>,
    /// Reads that waited for a partner and got none.
    missed: AtomicUsize,
    /// A turn committed (a worker's completion, through the route).
    committed: tokio::sync::Notify,
}

struct Meeting {
    run: String,
    barrier: Arc<tokio::sync::Barrier>,
    arrivals: usize,
}

impl Probe {
    fn new() -> Self {
        Self {
            reads: Mutex::new(Vec::new()),
            gets: Mutex::new(Vec::new()),
            fail_passes: AtomicBool::new(false),
            failed_passes: AtomicUsize::new(0),
            fail_claims: AtomicBool::new(false),
            panic_reads: AtomicBool::new(false),
            read_delay_ms: AtomicU64::new(0),
            hold_read: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
            meeting: Mutex::new(None),
            missed: AtomicUsize::new(0),
            committed: tokio::sync::Notify::new(),
        }
    }

    /// Passes begun on `run` so far.
    fn reads_of(&self, run: &ExecutionId) -> usize {
        let run = run.0.to_string();
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|r| **r == run)
            .count()
    }

    fn gets_of(&self, run: &ExecutionId) -> usize {
        let run = run.0.to_string();
        self.gets
            .lock()
            .unwrap()
            .iter()
            .filter(|r| **r == run)
            .count()
    }

    fn total_reads(&self) -> usize {
        self.reads.lock().unwrap().len()
    }

    /// Have the next two reads of `run`'s log wait for each other.
    fn meet_on(&self, run: &ExecutionId) {
        *self.meeting.lock().unwrap() = Some(Meeting {
            run: run.0.to_string(),
            barrier: Arc::new(tokio::sync::Barrier::new(2)),
            arrivals: 0,
        });
    }
}

#[async_trait]
impl Hooks for Probe {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        match op {
            Op::GetEventsSince(run) => {
                self.reads.lock().unwrap().push(run.0.to_string());
                if self.panic_reads.load(Ordering::SeqCst) {
                    panic!("a store call that panics, on purpose");
                }
                if self.hold_read.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.acquire().await.unwrap().forget();
                }
                let ms = self.read_delay_ms.load(Ordering::SeqCst);
                if ms > 0 {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
            Op::GetExecution(run) => self.gets.lock().unwrap().push(run.0.to_string()),
            Op::CommitSchedulerPass(_) if self.fail_passes.load(Ordering::SeqCst) => {
                self.failed_passes.fetch_add(1, Ordering::SeqCst);
                return Some(StateBackendError::Database("injected".into()));
            }
            Op::ClaimWorkItemFiltered { .. } if self.fail_claims.load(Ordering::SeqCst) => {
                return Some(StateBackendError::Unavailable("injected".into()));
            }
            _ => {}
        }
        None
    }

    async fn after(&self, op: &Op<'_>) {
        match op {
            Op::CommitTurnUnlessTerminal { .. } | Op::CommitTurn { .. } => {
                self.committed.notify_one();
            }
            Op::GetEventsSince(run) => {
                let barrier = {
                    let mut meeting = self.meeting.lock().unwrap();
                    match meeting.as_mut() {
                        Some(m) if m.run == run.0.to_string() => {
                            m.arrivals += 1;
                            let barrier = m.barrier.clone();
                            if m.arrivals >= 2 {
                                *meeting = None;
                            }
                            Some(barrier)
                        }
                        _ => None,
                    }
                };
                if let Some(barrier) = barrier {
                    // A partner that never comes fails the case, not the run.
                    if tokio::time::timeout(Duration::from_secs(20), barrier.wait())
                        .await
                        .is_err()
                    {
                        self.missed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
            _ => {}
        }
    }
}

// ── A store, an engine on it, and a router ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Memory,
    Sqlite,
    Postgres,
}

const KINDS: [Kind; 3] = [Kind::Memory, Kind::Sqlite, Kind::Postgres];

/// The stores a case runs on: all, or the ones `INLINE_ADVANCE_KINDS` names
/// (`memory,sqlite,postgres`), to run a case on one.
fn kinds() -> Vec<Kind> {
    let wanted = std::env::var("INLINE_ADVANCE_KINDS").unwrap_or_default();
    KINDS
        .into_iter()
        .filter(|k| wanted.is_empty() || wanted.contains(&format!("{k:?}").to_lowercase()))
        .collect()
}

/// What a case may change about the engine it runs.
#[derive(Clone, Copy)]
struct Options {
    /// Whether the engine runs its own loops. The tests build none: this is
    /// what the driver reads to decide whether to advance inline.
    background: Background,
    /// Whether the router carries the driver, as `main`'s does. Every other
    /// test of this crate builds a router without it.
    driver_extension: bool,
    config: DriverConfig,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            background: Background::Off,
            driver_extension: true,
            config: DriverConfig {
                // Roomy, so a slow shared database cannot cut a case that is
                // not about budgets.
                inline_budget: Duration::from_secs(30),
                tick_budget: Duration::from_secs(60),
                engine_concurrency: 2,
                tick_interval: Duration::from_secs(60),
            },
        }
    }
}

struct Env {
    kind: Kind,
    /// The store itself, read by the case without passing the probe.
    inner: Arc<dyn StateBackend>,
    probe: Arc<Probe>,
    state: AppState,
    driver: Arc<Driver>,
    draining: Draining,
    trigger: Mutex<Option<jamjet_worker::ShutdownTrigger>>,
    options: Options,
    store: Option<Store>,
    dir: Option<PathBuf>,
    _db: Option<TestDatabase>,
    /// Tokens: an operator (the scheduler's), a developer (the caller that
    /// starts runs and works their steps) and a reviewer (who decides).
    ops: String,
    alice: String,
    bob: String,
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
        let mut pg = None;
        let inner: Arc<dyn StateBackend> = match kind {
            Kind::Memory => Arc::new(InMemoryBackend::new()),
            Kind::Sqlite => {
                let path = std::env::temp_dir().join(format!("jjinline-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&path).unwrap();
                let file = path.join("runtime.db");
                let backend = SqliteBackend::open(&format!("sqlite://{}", file.display()))
                    .await
                    .expect("open the test database");
                dir = Some(path);
                store = Some(Store::SqliteFile(file));
                Arc::new(backend)
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
        // On Postgres the audit log is the database's, so the invariants
        // check the chain the engine wrote.
        let (audit, enricher): (Arc<dyn AuditBackend>, Arc<AuditEnricher>) = match &pg {
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
        let mut driver = Driver::new(
            &state,
            Arc::new(ModelRegistry::new()),
            shutdown,
            options.config,
        )
        .with_background(options.background);
        if let Some(pg) = &pg {
            driver = driver.with_outbox(pg.clone());
        }
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
        Some(Env {
            kind,
            inner,
            probe,
            state,
            driver: Arc::new(driver),
            draining: Draining::default(),
            trigger: Mutex::new(Some(trigger)),
            options,
            store,
            dir,
            _db: db,
            ops,
            alice,
            bob,
        })
    }

    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Memory => "memory",
            Kind::Sqlite => "sqlite",
            Kind::Postgres => "postgres",
        }
    }

    /// The server's router outside dev mode, behind the drain flag as `main`
    /// runs it, with the driver unless this engine's options leave it out.
    fn app(&self) -> axum::Router {
        let router = build_router_with_opts(self.state.clone(), false);
        let router = if self.options.driver_extension {
            router.layer(axum::Extension(self.driver.clone()))
        } else {
            router
        };
        router.layer(axum::middleware::from_fn_with_state(
            self.draining.clone(),
            refuse_claims_while_draining,
        ))
    }

    async fn call(&self, method: &str, uri: &str, token: &str, body: Option<Value>) -> Reply {
        send(self.app(), method, uri, token, body).await
    }

    /// `POST /ops/tick` with the operator's token.
    async fn tick(&self) -> Value {
        let reply = self
            .call("POST", "/ops/tick", &self.ops, Some(json!({})))
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}: {}",
            self.name(),
            reply.body
        );
        assert_eq!(reply.body["ran"], true, "{}: {}", self.name(), reply.body);
        reply.body
    }

    async fn register(&self, ir: &Value) {
        let reply = self
            .call("POST", "/workflows", &self.alice, Some(json!({ "ir": ir })))
            .await;
        assert!(
            reply.status.is_success(),
            "{}: register failed: {} {}",
            self.name(),
            reply.status,
            reply.body
        );
    }

    /// Start a run of `workflow` through the route, as a developer does.
    async fn start(&self, workflow: &str) -> ExecutionId {
        let reply = self.start_reply(workflow).await;
        assert_eq!(
            reply.status,
            StatusCode::CREATED,
            "{}: start failed: {}",
            self.name(),
            reply.body
        );
        parse_id(
            reply.body["execution_id"]
                .as_str()
                .expect("an execution_id"),
        )
    }

    async fn start_reply(&self, workflow: &str) -> Reply {
        self.call(
            "POST",
            "/executions",
            &self.alice,
            Some(json!({ "workflow_id": workflow, "workflow_version": "0.1.0", "input": {} })),
        )
        .await
    }

    /// A worker's claim on `queue`, through the route: the oldest item there.
    async fn claim(&self, queue: &str) -> Value {
        let reply = self
            .call(
                "POST",
                "/work-items/claim",
                &self.alice,
                Some(json!({ "worker_id": "w1", "queue_types": [queue] })),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}: {}",
            self.name(),
            reply.body
        );
        reply.body
    }

    /// A claim that must hand out the item of `node`; returns the item.
    async fn claimed(&self, queue: &str, node: &str) -> Value {
        let body = self.claim(queue).await;
        assert_eq!(
            body["claimed"],
            true,
            "{}: nothing to claim for {node}: {body}",
            self.name()
        );
        assert_eq!(
            body["work_item"]["node_id"],
            node,
            "{}: {body}",
            self.name()
        );
        body["work_item"].clone()
    }

    /// The worker finishes the item it claimed, through the route.
    async fn complete_item(&self, item: &Value) -> Reply {
        let id = item["id"].as_str().unwrap();
        self.call(
            "POST",
            &format!("/work-items/{id}/complete"),
            &self.alice,
            Some(json!({
                "output": {}, "state_patch": {}, "duration_ms": 1,
                "lease_fence": item["lease_fence"],
            })),
        )
        .await
    }

    /// An external worker takes `node`'s item of `run` straight from the
    /// store, by run and node, so other runs' items never get in the way.
    async fn claim_item(&self, run: &ExecutionId, node: &str) -> jamjet_state::WorkItem {
        self.inner
            .claim_work_item_filtered(
                "race-worker",
                &["python_tool"],
                None,
                &ClaimFilter {
                    execution_id: Some(run.clone()),
                    node_id: Some(node.to_string()),
                },
            )
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{}: no item for {node} of {run}", self.name()))
    }

    /// The worker finishes an item [`claim_item`](Self::claim_item) took,
    /// through the route.
    async fn complete_by_route(&self, item: &jamjet_state::WorkItem) -> Reply {
        self.call(
            "POST",
            &format!("/work-items/{}/complete", item.id),
            &self.alice,
            Some(json!({
                "output": {}, "state_patch": {}, "duration_ms": 1,
                "lease_fence": item.lease_fence,
            })),
        )
        .await
    }

    async fn fail_item(&self, item: &Value) -> Reply {
        let id = item["id"].as_str().unwrap();
        self.call(
            "POST",
            &format!("/work-items/{id}/fail"),
            &self.alice,
            Some(json!({ "error": "boom", "lease_fence": item["lease_fence"] })),
        )
        .await
    }

    /// Claim `node`'s item on `queue` and complete it through the route.
    async fn work(&self, queue: &str, node: &str) -> Reply {
        let item = self.claimed(queue, node).await;
        let reply = self.complete_item(&item).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}: {}",
            self.name(),
            reply.body
        );
        reply
    }

    /// The reviewer approves `run`'s pending `node`.
    async fn approve(&self, run: &ExecutionId, node: &str) -> Reply {
        let digest = self.digest(run).await;
        self.call(
            "POST",
            &format!("/executions/{run}/approve"),
            &self.bob,
            Some(json!({ "decision": "approved", "node_id": node, "action_digest": digest })),
        )
        .await
    }

    /// The digest the run's one approval request carries.
    async fn digest(&self, run: &ExecutionId) -> String {
        self.events(run)
            .await
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolApprovalRequired { context, .. } => {
                    context["action_digest"].as_str().map(str::to_string)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}: the run has no approval request", self.name()))
    }

    async fn events(&self, run: &ExecutionId) -> Vec<Event> {
        self.inner.get_events(run).await.unwrap()
    }

    /// The nodes with a `NodeCompleted`, in order.
    async fn completed(&self, run: &ExecutionId) -> Vec<String> {
        nodes_of(&self.events(run).await, |k| match k {
            EventKind::NodeCompleted { node_id, .. } => Some(node_id.to_string()),
            _ => None,
        })
    }

    /// The nodes with a `NodeScheduled`, in order.
    async fn scheduled(&self, run: &ExecutionId) -> Vec<String> {
        nodes_of(&self.events(run).await, |k| match k {
            EventKind::NodeScheduled { node_id, .. } => Some(node_id.to_string()),
            _ => None,
        })
    }

    async fn requests(&self, run: &ExecutionId) -> usize {
        self.events(run)
            .await
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
            .count()
    }

    async fn status(&self, run: &ExecutionId) -> WorkflowStatus {
        self.inner
            .get_execution(run)
            .await
            .unwrap()
            .expect("the run exists")
            .status
    }

    /// The nodes whose item waits for a worker.
    async fn waiting(&self, run: &ExecutionId) -> Vec<String> {
        let mut nodes: Vec<String> = self
            .inner
            .open_work_items(run)
            .await
            .unwrap()
            .into_iter()
            .filter(|i| !i.claimed)
            .map(|i| i.node_id)
            .collect();
        nodes.sort();
        nodes
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
}

fn nodes_of(events: &[Event], pick: impl Fn(&EventKind) -> Option<String>) -> Vec<String> {
    events.iter().filter_map(|e| pick(&e.kind)).collect()
}

fn parse_id(api: &str) -> ExecutionId {
    ExecutionId(uuid::Uuid::parse_str(api.trim_start_matches("exec_")).unwrap())
}

/// What a request got back.
struct Reply {
    status: StatusCode,
    body: Value,
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"));
    if body.is_some() {
        request = request.header("content-type", "application/json");
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
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

/// A `python_fn` node with no retries: its first failure is its last.
fn py_once(id: &str) -> Value {
    let mut step = py(id);
    step["retry_policy"] = json!("no_retry");
    step
}

/// A condition node: the engine's own quick step.
fn cond(id: &str) -> Value {
    node(id, json!({ "type": "condition", "branches": [] }))
}

/// An approval step with no deadline.
fn review(id: &str) -> Value {
    node(
        id,
        json!({ "type": "human_approval", "description": "Ship it?", "timeout_secs": null,
                "fallback_node": null }),
    )
}

/// The workflow `id` with start node `s`, these nodes and these edges, and an
/// edge from every node nothing leads out of to the end.
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

/// `s` (a worker's step), a condition `c`, then `p` (a worker's step).
fn through_a_condition(id: &str) -> Value {
    workflow(
        id,
        vec![py("s"), cond("c"), py("p")],
        &[("s", "c"), ("c", "p")],
    )
}

/// `s` (a worker's step), an approval, then `p` (a worker's step).
fn through_an_approval(id: &str) -> Value {
    workflow(
        id,
        vec![py("s"), review("review"), py("p")],
        &[("s", "review"), ("review", "p")],
    )
}

// ── The advance follows the write ────────────────────────────────────────────

/// A start with the loops off takes its run through one pass before it
/// answers. The start node's item is the one the start wrote, so the first
/// worker step is claimable at once; what the advance adds is the pass, and it
/// writes nothing the start did not.
#[tokio::test]
async fn a_run_started_with_loops_off_reaches_its_first_worker_step_without_a_tick() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_run_started_with_loops_off").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-start")).await;

        let reply = env.start_reply("wf-start").await;
        assert_eq!(reply.status, StatusCode::CREATED, "{name}: {}", reply.body);
        let run = parse_id(reply.body["execution_id"].as_str().unwrap());

        assert_eq!(
            env.probe.reads_of(&run),
            1,
            "{name}: the start took its run through one pass"
        );
        // Straight away, with no tick: the first worker step.
        env.claimed("python_tool", "s").await;
        assert_eq!(
            env.scheduled(&run).await,
            ["s"],
            "{name}: nothing was dispatched twice"
        );
        assert_eq!(env.status(&run).await, WorkflowStatus::Running, "{name}");
        env.assert_invariants().await;
    }
}

/// A start node of the engine's own (a condition, an approval) is queued by
/// the start's own write, and no pass dispatched it, so the advance does not
/// run it: the tick does. This pins that, so a change to it is a decision.
#[tokio::test]
async fn a_quick_start_node_is_left_for_the_tick() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_quick_start_node_is_left").await else {
            continue;
        };
        let name = env.name();
        env.register(&workflow(
            "wf-quick-start",
            vec![cond("s"), py("p")],
            &[("s", "p")],
        ))
        .await;

        let run = env.start("wf-quick-start").await;

        assert_eq!(env.probe.reads_of(&run), 1, "{name}: the advance ran");
        assert_eq!(env.completed(&run).await, Vec::<String>::new(), "{name}");
        assert_eq!(env.claim("python_tool").await["claimed"], false, "{name}");

        env.tick().await;
        assert_eq!(env.completed(&run).await, ["s"], "{name}: the tick ran it");
        env.tick().await;
        assert_eq!(env.scheduled(&run).await, ["s", "p"], "{name}");
    }
}

/// A completion takes the run to its next worker step, through the engine's
/// quick steps, before it answers: a claim right after the 200 gets the item.
#[tokio::test]
async fn a_completion_dispatches_the_next_step_before_it_answers() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_completion_dispatches_the_next_step").await else {
            continue;
        };
        let name = env.name();
        env.register(&through_a_condition("wf-next")).await;
        let run = env.start("wf-next").await;

        let reply = env.work("python_tool", "s").await;
        assert_eq!(
            reply.body,
            json!({ "completed": true, "work_item_id": reply.body["work_item_id"] }),
            "{name}"
        );

        // No tick in between.
        let next = env.claimed("python_tool", "p").await;
        assert_eq!(next["execution_id"], run.to_string(), "{name}");
        assert_eq!(
            env.completed(&run).await,
            ["s", "c"],
            "{name}: the condition ran inline, as the engine"
        );
        assert_eq!(env.scheduled(&run).await, ["s", "c", "p"], "{name}");
        env.assert_invariants().await;
    }
}

/// A decision takes the run past its approval before it answers: the approval
/// node completes and the next worker step is dispatched, with no tick.
#[tokio::test]
async fn an_approval_resumes_the_run_before_it_answers() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "an_approval_resumes_the_run").await else {
            continue;
        };
        let name = env.name();
        env.register(&through_an_approval("wf-approve")).await;
        let run = env.start("wf-approve").await;
        env.work("python_tool", "s").await;

        // The completion's advance parked the approval and asked once.
        assert_eq!(env.requests(&run).await, 1, "{name}: the request is raised");
        assert_eq!(env.scheduled(&run).await, ["s", "review"], "{name}");
        assert_eq!(env.claim("python_tool").await["claimed"], false, "{name}");

        let reply = env.approve(&run, "review").await;
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(reply.body["accepted"], true, "{name}: {}", reply.body);

        // No tick in between.
        assert_eq!(env.completed(&run).await, ["s", "review"], "{name}");
        env.claimed("python_tool", "p").await;
        assert_eq!(
            env.requests(&run).await,
            1,
            "{name}: the request was not raised again"
        );
        env.assert_invariants().await;
    }
}

/// A failure the worker reports is recorded, and the run is taken on before
/// the answer: with no retry left it fails now, not at the next tick.
#[tokio::test]
async fn a_failed_step_is_advanced_before_it_answers() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_failed_step_is_advanced").await else {
            continue;
        };
        let name = env.name();
        env.register(&workflow(
            "wf-fail",
            vec![py_once("s"), py("p")],
            &[("s", "p")],
        ))
        .await;
        let run = env.start("wf-fail").await;
        let item = env.claimed("python_tool", "s").await;

        let reply = env.fail_item(&item).await;

        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(reply.body["failed"], true, "{name}: {}", reply.body);
        assert_eq!(reply.body["retryable"], false, "{name}: {}", reply.body);
        assert_eq!(
            env.status(&run).await,
            WorkflowStatus::Failed,
            "{name}: the run failed with the answer"
        );
        env.assert_invariants().await;
    }
}

/// Every route that writes to a run takes it through one pass, and a request
/// that wrote nothing does not.
#[tokio::test]
async fn an_external_event_and_a_cancel_are_followed_by_an_advance() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "an_external_event_and_a_cancel").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-misc")).await;
        let run = env.start("wf-misc").await;
        let after_start = env.probe.reads_of(&run);

        let event = env
            .call(
                "POST",
                &format!("/executions/{run}/external-event"),
                &env.alice,
                Some(json!({ "correlation_key": "k", "payload": {} })),
            )
            .await;
        assert_eq!(event.status, StatusCode::OK, "{name}: {}", event.body);
        assert_eq!(
            env.probe.reads_of(&run),
            after_start + 1,
            "{name}: one pass after the event"
        );

        let gets = env.probe.gets_of(&run);
        let cancel = env
            .call(
                "POST",
                &format!("/executions/{run}/cancel"),
                &env.alice,
                None,
            )
            .await;
        assert_eq!(cancel.status, StatusCode::OK, "{name}: {}", cancel.body);
        // The route reads the run once; the advance reads it once more, and
        // finds it finished, and writes nothing.
        assert_eq!(
            env.probe.gets_of(&run),
            gets + 2,
            "{name}: the advance looked"
        );
        assert_eq!(env.status(&run).await, WorkflowStatus::Cancelled, "{name}");
        assert_eq!(
            env.scheduled(&run).await,
            ["s"],
            "{name}: nothing was dispatched"
        );
    }
}

/// A request that wrote nothing has nothing to follow: a start that named a
/// run already started, a decision sent twice, a completion with a stale
/// fence, a failure the engine refused.
#[tokio::test]
async fn a_request_that_wrote_nothing_does_not_advance() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_request_that_wrote_nothing").await else {
            continue;
        };
        let name = env.name();
        env.register(&through_an_approval("wf-nothing")).await;

        // A keyed start: the second names the run the first started.
        let keyed = json!({
            "workflow_id": "wf-nothing", "workflow_version": "0.1.0", "input": {},
            "run_key": "crew-7",
        });
        let first = env
            .call("POST", "/executions", &env.alice, Some(keyed.clone()))
            .await;
        assert_eq!(first.status, StatusCode::CREATED, "{name}: {}", first.body);
        let run = parse_id(first.body["execution_id"].as_str().unwrap());
        assert_eq!(
            env.probe.reads_of(&run),
            1,
            "{name}: the first start advanced"
        );
        let again = env
            .call("POST", "/executions", &env.alice, Some(keyed))
            .await;
        assert_eq!(again.status, StatusCode::OK, "{name}: {}", again.body);
        assert_eq!(
            env.probe.reads_of(&run),
            1,
            "{name}: the second wrote nothing"
        );

        // A completion with a fence the item never had.
        let item = env.claimed("python_tool", "s").await;
        let mut stale = item.clone();
        stale["lease_fence"] = json!(item["lease_fence"].as_i64().unwrap() + 1000);
        let refused = env.complete_item(&stale).await;
        assert_eq!(
            refused.status,
            StatusCode::CONFLICT,
            "{name}: {}",
            refused.body
        );
        assert_eq!(
            env.probe.reads_of(&run),
            1,
            "{name}: a refused completion wrote nothing"
        );
        assert_eq!(env.completed(&run).await, Vec::<String>::new(), "{name}");

        // The real one advances; the decision after it, twice.
        let done = env.complete_item(&item).await;
        assert_eq!(done.status, StatusCode::OK, "{name}: {}", done.body);
        assert!(
            env.probe.reads_of(&run) > 1,
            "{name}: the completion advanced"
        );
        let decided = env.approve(&run, "review").await;
        assert_eq!(decided.body["repeat"], false, "{name}: {}", decided.body);
        let reads = env.probe.reads_of(&run);
        let repeated = env.approve(&run, "review").await;
        assert_eq!(repeated.status, StatusCode::OK, "{name}: {}", repeated.body);
        assert_eq!(repeated.body["repeat"], true, "{name}: {}", repeated.body);
        assert_eq!(
            env.probe.reads_of(&run),
            reads,
            "{name}: a repeat wrote nothing"
        );
    }
}

// ── The approvals a run lists ────────────────────────────────────────────────

impl Env {
    /// The reviewer decides `run`'s pending `node`: `approved` or `rejected`.
    async fn decide(&self, run: &ExecutionId, node: &str, decision: &str) -> Reply {
        let digest = self.digest(run).await;
        self.call(
            "POST",
            &format!("/executions/{run}/approve"),
            &self.bob,
            Some(json!({ "decision": decision, "node_id": node, "action_digest": digest })),
        )
        .await
    }

    /// `GET /executions/:id/approvals` as the reviewer: the nodes pending,
    /// and each decided node with its status.
    async fn listed(&self, run: &ExecutionId) -> (Vec<String>, Vec<(String, String)>) {
        let reply = self
            .call("GET", &format!("/executions/{run}/approvals"), &self.bob, None)
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}: {}",
            self.name(),
            reply.body
        );
        let node = |row: &Value| row["node_id"].as_str().unwrap_or_default().to_string();
        let pending = reply.body["pending"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: {}", self.name(), reply.body))
            .iter()
            .map(node)
            .collect();
        let decided = reply.body["decided"]
            .as_array()
            .unwrap_or_else(|| panic!("{}: {}", self.name(), reply.body))
            .iter()
            .map(|row| (node(row), row["status"].as_str().unwrap_or_default().to_string()))
            .collect();
        (pending, decided)
    }

    /// The stored copy of `run`'s approvals, as the projector left it: each
    /// node with its status.
    async fn stored(&self, run: &ExecutionId) -> Vec<(String, String)> {
        self.inner
            .get_approval_projection(run)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.node_id, row.status))
            .collect()
    }
}

fn decided(node: &str, status: &str) -> Vec<(String, String)> {
    vec![(node.to_string(), status.to_string())]
}

/// A tick has stored `review` as pending. The reviewer rejects it, and the
/// decision's advance fails the node and the run before the answer. No
/// projector visits a finished run again, so the stored copy says pending
/// for good. Read right after the decision, and again after another tick,
/// the run lists the rejection and nothing pending.
#[tokio::test]
async fn a_run_that_ends_inside_its_advance_lists_its_decision() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_run_that_ends_inside_its_advance").await else {
            continue;
        };
        let name = env.name();
        env.register(&workflow(
            "wf-ends",
            vec![py("s"), review("review")],
            &[("s", "review")],
        ))
        .await;
        let run = env.start("wf-ends").await;
        env.work("python_tool", "s").await;
        env.tick().await;
        let pending = vec!["review".to_string()];
        assert_eq!(
            env.stored(&run).await,
            decided("review", "pending"),
            "{name}: the tick stored the request"
        );
        assert_eq!(env.listed(&run).await, (pending, vec![]), "{name}");

        let reply = env.decide(&run, "review", "rejected").await;
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(
            env.status(&run).await,
            WorkflowStatus::Failed,
            "{name}: the run ended inside the advance"
        );

        // No tick in between.
        assert_eq!(
            env.listed(&run).await,
            (vec![], decided("review", "rejected")),
            "{name}: right after the decision"
        );
        env.tick().await;
        assert_eq!(
            env.stored(&run).await,
            decided("review", "pending"),
            "{name}: no projector visits a finished run"
        );
        assert_eq!(
            env.listed(&run).await,
            (vec![], decided("review", "rejected")),
            "{name}: after the next tick"
        );
    }
}

/// A tick has stored `review1` as pending. The reviewer approves it, and the
/// decision's advance dispatches `p`. Read right after, `review1` is decided
/// and nothing is pending. The worker completes `p`, and the completion's
/// advance raises `review2`. Read right after, `review2` is pending. No tick
/// runs in between, so the stored copy still says `review1` is pending.
#[tokio::test]
async fn a_request_raised_or_decided_by_an_advance_is_listed_at_once() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_request_raised_or_decided").await else {
            continue;
        };
        let name = env.name();
        env.register(&workflow(
            "wf-two-reviews",
            vec![py("s"), review("review1"), py("p"), review("review2")],
            &[("s", "review1"), ("review1", "p"), ("p", "review2")],
        ))
        .await;
        let run = env.start("wf-two-reviews").await;
        env.work("python_tool", "s").await;
        env.tick().await;
        assert_eq!(
            env.stored(&run).await,
            decided("review1", "pending"),
            "{name}: the tick stored the request"
        );

        let reply = env.decide(&run, "review1", "approved").await;
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(
            env.listed(&run).await,
            (vec![], decided("review1", "approved")),
            "{name}: right after the decision"
        );

        env.work("python_tool", "p").await;
        assert_eq!(
            env.requests(&run).await,
            2,
            "{name}: the completion's advance raised review2"
        );
        assert_eq!(
            env.listed(&run).await,
            (vec!["review2".to_string()], decided("review1", "approved")),
            "{name}: right after the completion"
        );
        assert_eq!(
            env.stored(&run).await,
            decided("review1", "pending"),
            "{name}: no tick ran, so the stored copy is behind"
        );
    }
}

// ── What the advance must never do ───────────────────────────────────────────

/// A pass that fails inside the advance changes nothing the caller is told:
/// the route answers as it would have, the write stands, and a later tick
/// finishes the run. The same for an engine step that cannot take its item.
#[tokio::test]
async fn a_failing_advance_never_changes_the_answer() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_failing_advance_never_changes").await else {
            continue;
        };
        let name = env.name();
        env.register(&through_a_condition("wf-failing")).await;
        let run = env.start("wf-failing").await;
        let item = env.claimed("python_tool", "s").await;
        let work_item_id = item["id"].clone();

        // The pass that would dispatch `c` cannot commit.
        env.probe.fail_passes.store(true, Ordering::SeqCst);
        let reply = env.complete_item(&item).await;
        assert!(
            env.probe.failed_passes.load(Ordering::SeqCst) >= 1,
            "{name}: the advance reached the commit that fails"
        );
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(
            reply.body,
            json!({ "completed": true, "work_item_id": work_item_id }),
            "{name}"
        );
        assert_eq!(env.completed(&run).await, ["s"], "{name}: the write stands");
        assert_eq!(
            env.scheduled(&run).await,
            ["s"],
            "{name}: nothing was dispatched"
        );

        // The store is back; the tick dispatches `c` and runs it, and the next
        // tick dispatches the worker's step.
        env.probe.fail_passes.store(false, Ordering::SeqCst);
        env.tick().await;
        env.tick().await;
        assert_eq!(env.completed(&run).await, ["s", "c"], "{name}");
        let last = env.claimed("python_tool", "p").await;
        let reply = env.complete_item(&last).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(env.status(&run).await, WorkflowStatus::Completed, "{name}");
        let scheduled = env.scheduled(&run).await;
        for step in ["s", "c", "p"] {
            assert_eq!(
                scheduled.iter().filter(|n| *n == step).count(),
                1,
                "{name}: {step} dispatched once: {scheduled:?}"
            );
        }
        env.assert_invariants().await;
    }
}

/// An engine step that cannot take its item is a counted failure inside the
/// advance. The caller is told what it would have been, and the tick runs the
/// step.
#[tokio::test]
async fn a_step_the_advance_cannot_take_never_changes_the_answer() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_step_the_advance_cannot_take").await else {
            continue;
        };
        let name = env.name();
        env.register(&through_a_condition("wf-claim")).await;
        let run = env.start("wf-claim").await;
        let item = env.claimed("python_tool", "s").await;

        env.probe.fail_claims.store(true, Ordering::SeqCst);
        let reply = env.complete_item(&item).await;
        env.probe.fail_claims.store(false, Ordering::SeqCst);

        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(reply.body["completed"], true, "{name}: {}", reply.body);
        // `c` was dispatched by the pass, and its step could not start.
        assert_eq!(env.scheduled(&run).await, ["s", "c"], "{name}");
        assert_eq!(env.completed(&run).await, ["s"], "{name}");
        env.tick().await;
        assert_eq!(
            env.completed(&run).await,
            ["s", "c"],
            "{name}: the tick ran it"
        );
    }
}

/// A panic inside the advance is a log line, not a failed request: the answer
/// is the one the route would have given.
#[tokio::test]
async fn a_panicking_advance_never_changes_the_answer() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_panicking_advance_never_changes").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-panic")).await;
        let run = env.start("wf-panic").await;
        let item = env.claimed("python_tool", "s").await;

        env.probe.panic_reads.store(true, Ordering::SeqCst);
        let reply = env.complete_item(&item).await;
        env.probe.panic_reads.store(false, Ordering::SeqCst);

        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(reply.body["completed"], true, "{name}: {}", reply.body);
        assert_eq!(env.completed(&run).await, ["s"], "{name}");
        env.tick().await;
        assert_eq!(
            env.scheduled(&run).await,
            ["s", "p"],
            "{name}: the tick goes on"
        );
    }
}

/// With the loops on, a write advances nothing: the scheduler does that, and
/// a second writer would only race it. The same with no driver at all, as
/// every other test of this crate builds its router. With the loops off the
/// same write does advance, so the cases above can fail.
#[tokio::test]
async fn no_inline_advance_with_loops_on() {
    for kind in kinds() {
        let engines = [
            (
                "loops on, driver present",
                Options {
                    background: Background::On,
                    ..Options::default()
                },
            ),
            (
                "no driver",
                Options {
                    driver_extension: false,
                    ..Options::default()
                },
            ),
            ("loops off", Options::default()),
        ];
        for (what, options) in engines {
            let Some(env) = Env::with(kind, "no_inline_advance_with_loops_on", options).await
            else {
                continue;
            };
            let name = format!("{} ({what})", env.name());
            env.register(&through_a_condition("wf-loops")).await;
            let run = env.start("wf-loops").await;
            env.work("python_tool", "s").await;
            let advanced = what == "loops off";

            // Passes begun by the start and the completion.
            assert_eq!(env.probe.total_reads() > 1, advanced, "{name}");
            assert!(advanced || env.probe.total_reads() == 0, "{name}");
            assert_eq!(
                env.scheduled(&run).await,
                if advanced {
                    vec!["s", "c", "p"]
                } else {
                    vec!["s"]
                },
                "{name}"
            );
            assert_eq!(
                env.claim("python_tool").await["claimed"],
                advanced,
                "{name}: the next step"
            );
        }
    }
}

/// A server that is stopping advances nothing: the drain flag `main` sets
/// before it waits for the workers, and the stop signal itself. The write
/// stands and the next engine's tick catches up.
#[tokio::test]
async fn a_write_while_draining_skips_the_advance() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_write_while_draining").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-drain")).await;
        let run = env.start("wf-drain").await;
        let item = env.claimed("python_tool", "s").await;
        let reads = env.probe.total_reads();

        // The flag.
        env.draining.0.store(true, Ordering::SeqCst);
        let reply = env.complete_item(&item).await;
        env.draining.0.store(false, Ordering::SeqCst);
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(reply.body["completed"], true, "{name}: {}", reply.body);
        assert_eq!(env.probe.total_reads(), reads, "{name}: no pass was begun");
        assert_eq!(
            env.scheduled(&run).await,
            ["s"],
            "{name}: nothing was dispatched"
        );
        assert_eq!(env.claim("python_tool").await["claimed"], false, "{name}");

        // The tick catches up.
        env.tick().await;
        assert_eq!(env.scheduled(&run).await, ["s", "p"], "{name}");

        // The stop signal alone, with no flag: the advance refuses too.
        env.work("python_tool", "p").await;
        let second = env.start("wf-drain").await;
        let item = env.claimed("python_tool", "s").await;
        let trigger = env.trigger.lock().unwrap().take().unwrap();
        assert_eq!(trigger.stop_and_drain(Duration::ZERO).await, 0, "{name}");
        let reads = env.probe.total_reads();
        let reply = env.complete_item(&item).await;
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(
            env.probe.total_reads(),
            reads,
            "{name}: stopping, no pass was begun"
        );
        assert_eq!(env.scheduled(&second).await, ["s"], "{name}");
    }
}

/// The advance runs after the write has committed, never before it: the
/// seam between them (what a kill there would leave) sees the completion on
/// record and the next step not yet dispatched. An engine that stops there
/// leaves what the tick finishes.
#[tokio::test]
async fn the_advance_follows_the_commit_and_the_seam_sits_between() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "the_advance_follows_the_commit").await else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-seam")).await;
        let run = env.start("wf-seam").await;
        let item = env.claimed("python_tool", "s").await;

        let scope = Scope::new();
        scope.arm("inline.after_write", 1, Fault::Error);
        let reply = scope.run(env.complete_item(&item)).await;

        assert_eq!(
            scope.fired("inline.after_write", 1),
            1,
            "{name}: the seam was reached"
        );
        assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
        assert_eq!(reply.body["completed"], true, "{name}: {}", reply.body);
        assert_eq!(
            env.completed(&run).await,
            ["s"],
            "{name}: the write was on record"
        );
        assert_eq!(
            env.probe.reads_of(&run),
            1,
            "{name}: only the start's pass: the completion's advance never began"
        );
        assert_eq!(env.scheduled(&run).await, ["s"], "{name}");

        env.tick().await;
        assert_eq!(
            env.scheduled(&run).await,
            ["s", "p"],
            "{name}: the tick finishes it"
        );
        env.assert_invariants().await;
    }
}

/// A client that hangs up while its request's advance is running does not cut
/// the advance off: the write committed, and the advance goes on to the end
/// of its pass, as the tick would have.
#[tokio::test]
async fn a_client_that_hangs_up_does_not_cut_the_advance_off() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "a_client_that_hangs_up").await else {
            continue;
        };
        let env = Arc::new(env);
        let name = env.name();
        env.register(&two_steps("wf-hangup")).await;
        let run = env.start("wf-hangup").await;
        let item = env.claimed("python_tool", "s").await;

        env.probe.hold_read.store(true, Ordering::SeqCst);
        let call = {
            let env = env.clone();
            tokio::spawn(async move { env.complete_item(&item).await })
        };
        tokio::time::timeout(Duration::from_secs(20), env.probe.entered.notified())
            .await
            .expect("the advance began");
        // The client goes away: the request's future is dropped.
        call.abort();
        let _ = call.await;
        env.probe.release.add_permits(1);

        let began = Instant::now();
        while env.scheduled(&run).await != ["s", "p"] {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "{name}: the advance was cut off with its request"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        env.assert_invariants().await;
    }
}

// ── The advance and the tick ─────────────────────────────────────────────────

/// An inline advance and a tick on one run fold the same log and decide to
/// dispatch the same step; the store takes one pass and refuses the other.
/// Twenty runs make them decide from the same point, so the race is open
/// every time and the store refuses a pass every time. Then the volume: a
/// ticker posting every 50 ms while 100 completions arrive through the route
/// eight at a time, with a wide window between each pass's read and its
/// commit. The volume may or may not open the race (it prints how often it
/// did); the twenty are what prove it. Either way: one `NodeScheduled` per
/// node, and one open item for the next step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_advance_racing_the_tick_dispatches_once() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "inline_advance_racing_the_tick").await else {
            continue;
        };
        let env = Arc::new(env);
        let name = env.name();
        env.register(&two_steps("wf-race")).await;

        // Each run: the completion and a tick, both held at the read of the
        // run's log until both have it. The completion commits first, so
        // what both read has the step done, and both decide to dispatch `p`.
        for round in 0..20 {
            let run = env.start("wf-race").await;
            let item = env.claim_item(&run, "s").await;
            let stale = env.driver.scheduler().stale_passes();
            env.probe.meet_on(&run);
            let completion = {
                let env = env.clone();
                tokio::spawn(async move { env.complete_by_route(&item).await })
            };
            tokio::time::timeout(Duration::from_secs(20), env.probe.committed.notified())
                .await
                .expect("the completion committed");
            let tick = {
                let env = env.clone();
                tokio::spawn(async move { env.tick().await })
            };
            let completion = completion.await.unwrap();
            tick.await.unwrap();
            assert_eq!(
                completion.status,
                StatusCode::OK,
                "{name}: {}",
                completion.body
            );
            assert_eq!(
                env.probe.missed.load(Ordering::SeqCst),
                0,
                "{name} round {round}: both parties reached the read"
            );
            assert!(
                env.driver.scheduler().stale_passes() > stale,
                "{name} round {round}: the race opened: the store refused a pass"
            );
            assert_eq!(
                env.scheduled(&run).await,
                ["s", "p"],
                "{name} round {round}: p dispatched once"
            );
            assert_eq!(
                env.waiting(&run).await,
                ["p"],
                "{name} round {round}: one item"
            );
        }

        // The volume: runs completed through the route while a ticker ticks.
        let stale_before = env.driver.scheduler().stale_passes();
        env.probe.read_delay_ms.store(10, Ordering::SeqCst);
        let runs = if env.kind == Kind::Postgres { 30 } else { 100 };
        let mut started = Vec::new();
        for _ in 0..runs {
            started.push(env.start("wf-race").await);
        }
        // Eight workers at a time, so completions overlap each other and
        // the tick's walk.
        let completer = {
            let env = env.clone();
            let started = started.clone();
            tokio::spawn(async move {
                let lanes = Arc::new(tokio::sync::Semaphore::new(8));
                let mut done = Vec::new();
                for run in started {
                    let lane = lanes.clone().acquire_owned().await.unwrap();
                    let env = env.clone();
                    done.push(tokio::spawn(async move {
                        let item = env.claim_item(&run, "s").await;
                        let reply = env.complete_by_route(&item).await;
                        assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
                        drop(lane);
                    }));
                }
                for one in done {
                    one.await.unwrap();
                }
            })
        };
        let mut ticks = Vec::new();
        while !completer.is_finished() {
            // A tick every 50 ms whether or not the last one is done: one
            // that finds another running answers busy at once.
            let env = env.clone();
            ticks.push(tokio::spawn(async move {
                env.call(
                    "POST",
                    "/ops/tick",
                    &env.ops,
                    Some(json!({ "budget_ms": 5000 })),
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        completer.await.unwrap();
        let sent = ticks.len();
        let mut ran = 0;
        for tick in ticks {
            let reply = tick.await.unwrap();
            assert_eq!(reply.status, StatusCode::OK, "{name}: {}", reply.body);
            ran += (reply.body["ran"] == true) as usize;
        }
        env.probe.read_delay_ms.store(0, Ordering::SeqCst);

        for run in &started {
            assert_eq!(
                env.scheduled(run).await,
                ["s", "p"],
                "{name}: each node once: {run}"
            );
            assert_eq!(
                env.waiting(run).await,
                ["p"],
                "{name}: one item for p: {run}"
            );
        }
        println!(
            "{name}: {runs} completions, {ran} of {sent} ticks ran, {} stale passes in the volume",
            env.driver.scheduler().stale_passes() - stale_before
        );
        env.assert_invariants().await;
    }
}

// ── The answers ──────────────────────────────────────────────────────────────

/// `body` with the ids of this run replaced by names, so two engines'
/// answers to the same script can be compared as the literals they are.
fn scrub(body: &Value, ids: &[(String, &str)]) -> Value {
    let mut text = body.to_string();
    for (id, name) in ids {
        text = text.replace(id.as_str(), name);
    }
    serde_json::from_str(&text).unwrap()
}

/// The same script against an engine with the loops off and one with the
/// loops on: each route answers with the same body, whatever the advance did
/// after the write.
#[tokio::test]
async fn answers_are_the_same_with_loops_on_and_off() {
    for kind in kinds() {
        let mut answers: Vec<(String, Vec<Value>)> = Vec::new();
        for (what, options) in [
            ("loops off", Options::default()),
            (
                "loops on",
                Options {
                    background: Background::On,
                    ..Options::default()
                },
            ),
        ] {
            let Some(env) = Env::with(kind, "answers_are_the_same", options).await else {
                continue;
            };
            let name = format!("{} ({what})", env.name());
            // After each write the engines are made alike: an engine with the
            // loops on has nobody advancing it here, so a tick does what its
            // scheduler would.
            let settle = || async {
                env.tick().await;
                env.tick().await;
            };
            env.register(&through_an_approval("wf-answers")).await;
            let mut bodies = Vec::new();

            // start
            let start = env.start_reply("wf-answers").await;
            assert_eq!(start.status, StatusCode::CREATED, "{name}");
            let run = parse_id(start.body["execution_id"].as_str().unwrap());
            let ids = |run: &ExecutionId, item: &str| {
                vec![
                    (run.to_string(), "<run>"),
                    (run.0.to_string(), "<run>"),
                    (item.to_string(), "<item>"),
                ]
            };
            bodies.push(scrub(&start.body, &ids(&run, "-")));
            settle().await;

            // complete
            let item = env.claimed("python_tool", "s").await;
            let done = env.complete_item(&item).await;
            assert_eq!(done.status, StatusCode::OK, "{name}");
            bodies.push(scrub(&done.body, &ids(&run, item["id"].as_str().unwrap())));
            settle().await;

            // approve
            let approved = env.approve(&run, "review").await;
            assert_eq!(approved.status, StatusCode::OK, "{name}: {}", approved.body);
            bodies.push(scrub(&approved.body, &ids(&run, "-")));
            settle().await;

            // fail
            let item = env.claimed("python_tool", "p").await;
            let failed = env.fail_item(&item).await;
            assert_eq!(failed.status, StatusCode::OK, "{name}: {}", failed.body);
            bodies.push(scrub(
                &failed.body,
                &ids(&run, item["id"].as_str().unwrap()),
            ));

            // external event and cancel, on a second run
            let second = env.start("wf-answers").await;
            let event = env
                .call(
                    "POST",
                    &format!("/executions/{second}/external-event"),
                    &env.alice,
                    Some(json!({ "correlation_key": "k", "payload": {} })),
                )
                .await;
            assert_eq!(event.status, StatusCode::OK, "{name}");
            bodies.push(scrub(&event.body, &ids(&second, "-")));
            let cancel = env
                .call(
                    "POST",
                    &format!("/executions/{second}/cancel"),
                    &env.alice,
                    None,
                )
                .await;
            assert_eq!(cancel.status, StatusCode::OK, "{name}");
            bodies.push(scrub(&cancel.body, &ids(&second, "-")));
            answers.push((name, bodies));
        }

        assert!(answers.len() >= 2, "{kind:?}: engines to compare");
        let expected = vec![
            json!({ "execution_id": "<run>" }),
            json!({ "completed": true, "work_item_id": "<item>" }),
            json!({
                "execution_id": "<run>", "node_id": "review", "accepted": true,
                "repeat": false, "sequence": 7, "waiting_for": "engine",
            }),
            json!({ "failed": true, "work_item_id": "<item>", "retryable": true, "attempt": 1 }),
            json!({ "execution_id": "<run>", "accepted": true }),
            json!({ "execution_id": "<run>", "status": "cancelled" }),
        ];
        for (name, bodies) in &answers {
            assert_eq!(bodies, &expected, "{name}");
        }
    }
}

// ── The MCP bridge ───────────────────────────────────────────────────────────

impl Env {
    /// One MCP tool call, with this token.
    async fn mcp(&self, token: &str, tool: &str, arguments: Value) -> Value {
        let reply = self
            .call(
                "POST",
                "/mcp",
                token,
                Some(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": tool, "arguments": arguments },
                })),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}: {}",
            self.name(),
            reply.body
        );
        assert_ne!(
            reply.body["result"]["isError"],
            true,
            "{}: {tool} failed: {}",
            self.name(),
            reply.body
        );
        let text = reply.body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: {tool} answered no text: {}", self.name(), reply.body));
        serde_json::from_str(text).unwrap()
    }
}

/// The MCP bridge writes runs the way the routes do, and takes them on the
/// same way: a start, a decision, a cancel.
#[tokio::test]
async fn an_mcp_start_a_decision_and_a_cancel_are_advanced_before_they_answer() {
    for kind in kinds() {
        let Some(env) = Env::new(kind, "an_mcp_start_a_decision_and_a_cancel").await else {
            continue;
        };
        let name = env.name();
        env.register(&through_an_approval("wf-mcp")).await;

        let started = env
            .mcp(
                &env.alice,
                "jamjet_run_workflow",
                json!({ "workflow_id": "wf-mcp", "workflow_version": "0.1.0", "input": {} }),
            )
            .await;
        let run = parse_id(started["execution_id"].as_str().unwrap());
        assert_eq!(
            env.probe.reads_of(&run),
            1,
            "{name}: the start took its run through a pass"
        );

        // The completion's advance raises the request; the decision's
        // advance takes the approval and the run past it.
        env.work("python_tool", "s").await;
        assert_eq!(env.requests(&run).await, 1, "{name}");
        let digest = env.digest(&run).await;
        let decided = env
            .mcp(
                &env.bob,
                "jamjet_approve",
                json!({
                    "execution_id": run.to_string(), "decision": "approved",
                    "node_id": "review", "action_digest": digest,
                }),
            )
            .await;
        assert_eq!(decided["accepted"], true, "{name}: {decided}");
        assert_eq!(
            env.completed(&run).await,
            ["s", "review"],
            "{name}: no tick in between"
        );
        env.claimed("python_tool", "p").await;

        let gets = env.probe.gets_of(&run);
        env.mcp(
            &env.alice,
            "jamjet_cancel_execution",
            json!({ "execution_id": run.to_string() }),
        )
        .await;
        assert_eq!(
            env.probe.gets_of(&run),
            gets + 2,
            "{name}: the cancel's advance looked"
        );
        assert_eq!(env.status(&run).await, WorkflowStatus::Cancelled, "{name}");
        env.assert_invariants().await;
    }
}

/// With the loops on, an MCP write advances nothing either.
#[tokio::test]
async fn an_mcp_write_with_loops_on_advances_nothing() {
    for kind in kinds() {
        let Some(env) = Env::with(
            kind,
            "an_mcp_write_with_loops_on",
            Options {
                background: Background::On,
                ..Options::default()
            },
        )
        .await
        else {
            continue;
        };
        let name = env.name();
        env.register(&two_steps("wf-mcp-on")).await;
        let started = env
            .mcp(
                &env.alice,
                "jamjet_run_workflow",
                json!({ "workflow_id": "wf-mcp-on", "workflow_version": "0.1.0", "input": {} }),
            )
            .await;
        let run = parse_id(started["execution_id"].as_str().unwrap());
        assert_eq!(env.probe.reads_of(&run), 0, "{name}");
        env.mcp(
            &env.alice,
            "jamjet_cancel_execution",
            json!({ "execution_id": run.to_string() }),
        )
        .await;
        assert_eq!(env.probe.reads_of(&run), 0, "{name}");
    }
}
