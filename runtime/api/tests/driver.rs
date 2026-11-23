//! The driver advances one run and runs the engine's own steps.
//!
//! With the background loops off, `Driver::advance` is what moves a run after
//! a write: it builds the scheduler's pass for that run and runs the quick
//! engine steps the pass dispatched (a condition, an approval park, a limit),
//! as the engine, until the next step belongs to a worker, a model call or the
//! tick.
//!
//! Every case runs on the memory store, on a SQLite file and, when
//! `JAMJET_TEST_POSTGRES_URL` is set, on Postgres. The race cases run the
//! scheduler and the engine's workers for real: no mock sits between them
//! and the store, only [`Intercept`], which can hold a call.

mod support;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::driver::DriverConfig;
use jamjet_api::driver::{AdvanceReport, Driver, QUICK_KINDS};
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_audit::{
    ActorType, AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery, AuditSigner,
};
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_state::backend::{OpenWorkItem, StateBackend, StateBackendError, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::{
    ClaimFilter, Event, InMemoryBackend, PgBackend, PgSettings, SqliteBackend, DEFAULT_TENANT,
};
use jamjet_testkit::invariants::{self, Store};
use jamjet_testkit::pg::TestDatabase;
use jamjet_testkit::{Hooks, Intercept, Op};
use jamjet_worker::{ConditionNodeExecutor, ExecutionResult, ExecutorError, NodeExecutor};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

/// A budget no case reaches.
const LONG: Duration = Duration::from_secs(60);

// ── The audit log, kept ──────────────────────────────────────────────────────

/// Keeps every appended entry, in the order they were sealed.
#[derive(Default)]
struct CapturingAudit {
    entries: tokio::sync::Mutex<Vec<AuditLogEntry>>,
}

#[async_trait]
impl AuditBackend for CapturingAudit {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.entries.lock().await.push(entry);
        Ok(())
    }
    async fn query(&self, _q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(self.entries.lock().await.iter().rev().cloned().collect())
    }
    async fn count(&self, _q: &AuditQuery) -> Result<u64, AuditError> {
        Ok(self.entries.lock().await.len() as u64)
    }
}

// ── What a test does around the store's calls ────────────────────────────────

/// Holds or fails calls for a case. Does nothing until a case arms it.
#[derive(Default)]
struct Probe {
    /// While set, the first two reads of a log since a sequence wait for
    /// each other: two components that fold the same log reach their
    /// decisions from the same point.
    armed: AtomicBool,
    arrivals: AtomicUsize,
    meeting: Option<tokio::sync::Barrier>,
    /// While set, a claim by run and node fails as a busy store does.
    fail_claims: AtomicBool,
}

impl Probe {
    fn new() -> Self {
        Self {
            meeting: Some(tokio::sync::Barrier::new(2)),
            ..Self::default()
        }
    }
}

#[async_trait]
impl Hooks for Probe {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        if matches!(op, Op::ClaimWorkItemFiltered { .. }) && self.fail_claims.load(Ordering::SeqCst)
        {
            return Some(StateBackendError::Unavailable("injected".into()));
        }
        None
    }

    async fn after(&self, op: &Op<'_>) {
        if let (Op::GetEventsSince(_), Some(meeting)) = (op, &self.meeting) {
            if self.armed.load(Ordering::SeqCst) && self.arrivals.fetch_add(1, Ordering::SeqCst) < 2
            {
                // A hang fails the case instead of eating the run.
                tokio::time::timeout(Duration::from_secs(20), meeting.wait())
                    .await
                    .expect("the other party reached the same point");
            }
        }
    }
}

// ── Executors ────────────────────────────────────────────────────────────────

/// The engine's own condition executor, counting each run of each node.
#[derive(Clone, Default)]
struct Counting {
    runs: Arc<Mutex<HashMap<(String, String), usize>>>,
}

impl Counting {
    fn times(&self, run: &ExecutionId, node: &str) -> usize {
        self.runs
            .lock()
            .unwrap()
            .get(&(run.to_string(), node.to_string()))
            .copied()
            .unwrap_or(0)
    }

    fn total(&self) -> usize {
        self.runs.lock().unwrap().values().sum()
    }
}

#[async_trait]
impl NodeExecutor for Counting {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        *self
            .runs
            .lock()
            .unwrap()
            .entry((item.execution_id.to_string(), item.node_id.clone()))
            .or_default() += 1;
        ConditionNodeExecutor::new().execute(item).await
    }
}

/// Stops inside the step until the case lets it go.
struct Holding {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl NodeExecutor for Holding {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        ConditionNodeExecutor::new().execute(item).await
    }
}

/// A step that panics.
struct Panics;

#[async_trait]
impl NodeExecutor for Panics {
    async fn execute(&self, _item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        panic!("a step that panics, on purpose");
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

struct Env {
    kind: Kind,
    /// The store itself, read by the case without passing the probe.
    inner: Arc<dyn StateBackend>,
    probe: Arc<Probe>,
    state: AppState,
    audit: Arc<CapturingAudit>,
    driver: Arc<Driver>,
    trigger: Mutex<Option<jamjet_worker::ShutdownTrigger>>,
    /// Where `invariants::check` reads the store, when it can.
    store: Option<Store>,
    dir: Option<PathBuf>,
    _db: Option<TestDatabase>,
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
        Self::with_condition(kind, case, None).await
    }

    /// An env whose engine runs conditions with `condition`.
    async fn with_condition(
        kind: Kind,
        case: &str,
        condition: Option<Arc<dyn NodeExecutor>>,
    ) -> Option<Env> {
        let mut dir = None;
        let mut db = None;
        let mut store = None;
        let inner: Arc<dyn StateBackend> = match kind {
            Kind::Memory => Arc::new(InMemoryBackend::new()),
            Kind::Sqlite => {
                let path = std::env::temp_dir().join(format!("jjdriver-{}", uuid::Uuid::new_v4()));
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
                let pg = PgBackend::connect(fresh.url(), PgSettings::default())
                    .await
                    .unwrap_or_else(|e| panic!("connect to the test database: {e}"));
                store = Some(Store::Postgres(fresh.url().to_string()));
                db = Some(fresh);
                Arc::new(pg)
            }
        };
        let probe = Arc::new(Probe::new());
        let backend: Arc<dyn StateBackend> = Arc::new(Intercept::new(inner.clone(), probe.clone()));
        let audit = Arc::new(CapturingAudit::default());
        let audit_dyn: Arc<dyn AuditBackend> = audit.clone();
        let enricher = Arc::new(AuditEnricher::with_signer(
            audit_dyn.clone(),
            AuditSigner::new(b"driver-test-key".to_vec()),
        ));
        let for_routes = backend.clone();
        let state = AppState {
            backend: backend.clone(),
            backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| for_routes.clone()),
            agents: Arc::new(InMemoryAgentRegistry::new()),
            audit: audit_dyn,
            enricher: enricher.clone(),
            protocols: jamjet_api::state::default_protocol_registry(),
            cron_store: None,
            approval_policy: Default::default(),
        };
        // The engine's pool, as the server builds it. A case that brings a
        // condition executor of its own gets the same pool with that one.
        let models = Arc::new(jamjet_models::ModelRegistry::new());
        let (trigger, shutdown) = jamjet_worker::shutdown();
        let driver = match condition {
            // The constructor the server uses.
            None => Driver::new(&state, models, shutdown, DriverConfig::default()),
            Some(condition) => {
                let engine = jamjet_worker::server_pool(backend.clone(), models)
                    .with_sealer(enricher)
                    .with_agents(state.agents.clone())
                    .with_executor("condition", condition);
                Driver::with_engine(&state, engine, shutdown, DriverConfig::default())
            }
        };
        let driver = Arc::new(driver);
        Some(Env {
            kind,
            inner,
            probe,
            state,
            audit,
            driver,
            trigger: Mutex::new(Some(trigger)),
            store,
            dir,
            _db: db,
        })
    }

    fn name(&self) -> &'static str {
        match self.kind {
            Kind::Memory => "memory",
            Kind::Sqlite => "sqlite",
            Kind::Postgres => "postgres",
        }
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
        let (st, body) = support::post(
            &self.state,
            "/executions",
            json!({ "workflow_id": workflow, "workflow_version": "0.1.0", "input": {} }),
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

    /// A run of `workflow` whose start node, `s`, a `python_fn`, is done.
    async fn started_and_s_done(&self, workflow: &str) -> ExecutionId {
        let run = self.start(workflow).await;
        self.complete(&run, "s", "python_tool").await;
        run
    }

    async fn events(&self, run: &ExecutionId) -> Vec<Event> {
        self.inner.get_events(run).await.unwrap()
    }

    /// The nodes that recorded a `NodeCompleted`, in order.
    async fn completed(&self, run: &ExecutionId) -> Vec<String> {
        nodes_of(&self.events(run).await, |k| match k {
            EventKind::NodeCompleted { node_id, .. } => Some(node_id.to_string()),
            _ => None,
        })
    }

    /// The nodes that recorded a `NodeScheduled`, in order.
    async fn scheduled(&self, run: &ExecutionId) -> Vec<String> {
        nodes_of(&self.events(run).await, |k| match k {
            EventKind::NodeScheduled { node_id, .. } => Some(node_id.to_string()),
            _ => None,
        })
    }

    /// The run's items still pending or claimed, by node.
    async fn open(&self, run: &ExecutionId) -> Vec<OpenWorkItem> {
        let mut open = self.inner.open_work_items(run).await.unwrap();
        open.sort_by(|a, b| a.node_id.cmp(&b.node_id));
        open
    }

    /// The nodes that still have an item waiting to be claimed.
    async fn waiting(&self, run: &ExecutionId) -> Vec<String> {
        self.open(run)
            .await
            .into_iter()
            .filter(|i| !i.claimed)
            .map(|i| i.node_id)
            .collect()
    }

    async fn requests(&self, run: &ExecutionId) -> usize {
        self.events(run)
            .await
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
            .count()
    }

    /// The audit entries of one event type.
    async fn sealed(&self, event_type: &str) -> Vec<AuditLogEntry> {
        self.audit
            .entries
            .lock()
            .await
            .iter()
            .filter(|e| e.event_type == event_type)
            .cloned()
            .collect()
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

fn review(id: &str) -> Value {
    review_within(id, None)
}

/// An approval step whose request expires after `secs`.
fn review_within(id: &str, secs: Option<u64>) -> Value {
    node(
        id,
        json!({ "type": "human_approval", "description": "Ship it?", "timeout_secs": secs,
                "fallback_node": null }),
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

/// `s` (python_fn) then `n` conditions, `c1` to `cn`, then `last` (python_fn).
fn chain(id: &str, n: usize) -> Value {
    let mut nodes = vec![py("s")];
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut before = "s".to_string();
    for i in 1..=n {
        let name = format!("c{i}");
        nodes.push(cond(&name));
        edges.push((before, name.clone()));
        before = name;
    }
    nodes.push(py("last"));
    edges.push((before, "last".to_string()));
    let edges: Vec<(&str, &str)> = edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    workflow(id, nodes, &edges)
}

fn queues(open: &[OpenWorkItem]) -> Vec<(String, String, bool)> {
    open.iter()
        .map(|i| (i.node_id.clone(), i.queue_type.clone(), i.claimed))
        .collect()
}

// ── The cases ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn advance_runs_a_condition_chain_to_the_next_worker_step() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_runs_a_condition_chain").await else {
            continue;
        };
        env.register(&chain("wf-chain", 2)).await;
        let run = env.started_and_s_done("wf-chain").await;

        let report = env.driver.advance(&run, LONG).await;

        let name = env.name();
        assert_eq!(report.engine_steps, 2, "{name}: {report:?}");
        assert_eq!(report.dispatched, 3, "{name}: c1, c2 and last: {report:?}");
        assert_eq!(report.errors, 0, "{name}: {report:?}");
        assert!(!report.out_of_budget, "{name}");
        assert!(!report.terminal, "{name}");
        assert_eq!(
            env.completed(&run).await,
            vec!["s", "c1", "c2"],
            "{name}: the two conditions completed, once each"
        );
        assert_eq!(
            queues(&env.open(&run).await),
            vec![("last".to_string(), "python_tool".to_string(), false)],
            "{name}: the last item is the external worker's, still pending"
        );
        // The engine ran them, under a worker id of its own.
        let workers: Vec<String> = env
            .events(&run)
            .await
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NodeStarted { worker_id, .. } if worker_id.starts_with("engine-") => {
                    Some(worker_id.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(workers.len(), 2, "{name}: {workers:?}");
        env.assert_invariants().await;
    }
}

#[tokio::test]
async fn advance_parks_an_approval_and_records_the_request_once() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_parks_an_approval").await else {
            continue;
        };
        let ir = workflow(
            "wf-review",
            vec![py("s"), review("review"), py("publish")],
            &[("s", "review"), ("review", "publish")],
        );
        env.register(&ir).await;
        let run = env.started_and_s_done("wf-review").await;
        let name = env.name();

        let report = env.driver.advance(&run, LONG).await;

        assert_eq!(
            report.engine_steps, 1,
            "{name}: the park is one step: {report:?}"
        );
        assert_eq!(
            env.requests(&run).await,
            1,
            "{name}: one request in the log"
        );
        assert_eq!(
            env.sealed("tool_approval_required").await.len(),
            1,
            "{name}: sealed once"
        );
        assert!(
            env.waiting(&run).await.is_empty(),
            "{name}: the parked step's item is settled, not left to run again"
        );

        // Another advance finds the node held and asks for nothing more.
        let again = env.driver.advance(&run, LONG).await;
        assert_eq!(again.engine_steps, 0, "{name}: {again:?}");
        assert_eq!(env.requests(&run).await, 1, "{name}: still one request");
        assert_eq!(
            env.sealed("tool_approval_required").await.len(),
            1,
            "{name}"
        );
        assert_eq!(
            env.completed(&run).await,
            vec!["s"],
            "{name}: it waits for a person"
        );
        env.assert_invariants_without_audit().await;
    }
}

impl Env {
    /// [`assert_invariants`](Self::assert_invariants) for a case whose seals
    /// went to this test's own audit log, not the store's: every rule but
    /// the audit chain's.
    async fn assert_invariants_without_audit(&self) {
        let Some(store) = &self.store else { return };
        let broken: Vec<_> = invariants::check(store)
            .await
            .into_iter()
            .filter(|v| v.rule != 5)
            .collect();
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

/// A model step and an eval step wait for the tick. The eval node shares the
/// condition's queue, so a claim that ignored the node would take it: ten runs
/// make that near certain.
#[tokio::test]
async fn advance_leaves_model_and_eval_steps_for_the_tick() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_leaves_model_and_eval_steps").await else {
            continue;
        };
        let ir = workflow(
            "wf-slow",
            vec![
                py("s"),
                cond("quick"),
                node(
                    "eval",
                    json!({ "type": "eval", "scorers": [], "on_fail": "halt", "input_expr": null }),
                ),
                node(
                    "think",
                    json!({ "type": "model", "model_ref": "m", "prompt_ref": "", "output_schema": "" }),
                ),
            ],
            &[("s", "quick"), ("s", "eval"), ("s", "think")],
        );
        env.register(&ir).await;
        let name = env.name();

        for round in 0..10 {
            let run = env.started_and_s_done("wf-slow").await;

            let report = env.driver.advance(&run, LONG).await;

            assert_eq!(report.engine_steps, 1, "{name} run {round}: {report:?}");
            assert_eq!(report.dispatched, 3, "{name} run {round}: {report:?}");
            assert_eq!(
                env.completed(&run).await,
                vec!["s", "quick"],
                "{name} run {round}: only the condition ran"
            );
            assert_eq!(
                queues(&env.open(&run).await),
                vec![
                    ("eval".to_string(), "general".to_string(), false),
                    ("think".to_string(), "model".to_string(), false),
                ],
                "{name} run {round}: the eval and model items are still pending"
            );
        }
    }
}

/// A budget the first step already spent still runs that one step, and no
/// more. The budget of zero is the limit case: whatever the store's speed,
/// the clock is past it when the second step is due.
#[tokio::test]
async fn advance_stops_at_its_budget_after_one_step() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_stops_at_its_budget").await else {
            continue;
        };
        env.register(&chain("wf-long", 50)).await;
        let name = env.name();

        let run = env.started_and_s_done("wf-long").await;
        let report = env.driver.advance(&run, Duration::ZERO).await;

        assert_eq!(report.engine_steps, 1, "{name}: {report:?}");
        assert!(report.out_of_budget, "{name}: {report:?}");
        assert_eq!(env.completed(&run).await, vec!["s", "c1"], "{name}");
        assert_eq!(
            queues(&env.open(&run).await),
            vec![("c2".to_string(), "general".to_string(), false)],
            "{name}: the next step waits, dispatched and unrun"
        );

        // With a millisecond the same holds: some steps, not all fifty.
        let run = env.started_and_s_done("wf-long").await;
        let report = env.driver.advance(&run, Duration::from_millis(1)).await;
        assert!(report.out_of_budget, "{name}: {report:?}");
        assert!((1..50).contains(&report.engine_steps), "{name}: {report:?}");
    }
}

/// A budget too large to add to the clock must not panic the request.
#[tokio::test]
async fn an_advance_with_an_enormous_budget_runs_to_the_end_of_what_it_can() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "an_advance_with_an_enormous_budget").await else {
            continue;
        };
        env.register(&chain("wf-huge", 2)).await;
        let run = env.started_and_s_done("wf-huge").await;

        let report = env.driver.advance(&run, Duration::MAX).await;

        assert_eq!(report.engine_steps, 2, "{}: {report:?}", env.name());
        assert!(!report.out_of_budget, "{}", env.name());
    }
}

#[tokio::test]
async fn advance_on_a_finished_run_writes_nothing() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_on_a_finished_run").await else {
            continue;
        };
        env.register(&chain("wf-done", 1)).await;
        let run = env.started_and_s_done("wf-done").await;
        let name = env.name();
        env.inner
            .update_execution_status(&run, WorkflowStatus::Cancelled)
            .await
            .unwrap();
        let events = env.events(&run).await.len();
        let open = queues(&env.open(&run).await);

        let report = env.driver.advance(&run, LONG).await;

        assert_eq!(
            report,
            AdvanceReport {
                terminal: true,
                ..AdvanceReport::default()
            },
            "{name}"
        );
        assert_eq!(env.events(&run).await.len(), events, "{name}: no event");
        assert_eq!(queues(&env.open(&run).await), open, "{name}: no item moved");
    }
}

#[tokio::test]
async fn advance_on_a_run_that_does_not_exist_counts_an_error_and_writes_nothing() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_on_a_missing_run").await else {
            continue;
        };
        let report = env.driver.advance(&ExecutionId::new(), LONG).await;
        assert_eq!(
            report,
            AdvanceReport {
                errors: 1,
                ..AdvanceReport::default()
            },
            "{}",
            env.name()
        );
    }
}

/// Eight runs each hold a condition item the engine could take. Advancing one
/// runs that run's steps and leaves the other seven alone. The other runs'
/// items are older, and the memory store claims in no order at all, so a claim
/// that ignored the run would take another's almost every time.
#[tokio::test]
async fn advance_never_runs_another_runs_step() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "advance_never_runs_another_runs_step").await else {
            continue;
        };
        env.register(&chain("wf-two", 2)).await;
        let name = env.name();
        let mut others = Vec::new();
        for _ in 0..7 {
            let other = env.started_and_s_done("wf-two").await;
            // Dispatch its first condition and stop there: the item waits.
            let pass = env.driver.scheduler().advance_one(&other).await.unwrap();
            assert_eq!(pass.dispatched.len(), 1, "{name}");
            others.push(other);
        }
        let mine = env.started_and_s_done("wf-two").await;

        let report = env.driver.advance(&mine, LONG).await;

        assert_eq!(report.engine_steps, 2, "{name}: {report:?}");
        assert_eq!(env.completed(&mine).await, vec!["s", "c1", "c2"], "{name}");
        for other in &others {
            assert_eq!(
                env.completed(other).await,
                vec!["s"],
                "{name}: another run's step ran"
            );
            assert_eq!(
                env.waiting(other).await,
                vec!["c1"],
                "{name}: its item is still pending"
            );
        }
    }
}

/// An advance and a tick fold the same log and reach the same decision: to
/// dispatch `c1`. The barrier makes them decide from the same point, so the
/// race is open every time. The store takes one pass and refuses the other.
#[tokio::test]
async fn an_advance_and_a_tick_on_one_run_dispatch_once() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "an_advance_and_a_tick_on_one_run").await else {
            continue;
        };
        env.register(&chain("wf-race", 1)).await;
        let run = env.started_and_s_done("wf-race").await;
        let name = env.name();
        env.probe.armed.store(true, Ordering::SeqCst);

        let advance = {
            let driver = env.driver.clone();
            let run = run.clone();
            tokio::spawn(async move { driver.advance(&run, LONG).await })
        };
        let tick = {
            let driver = env.driver.clone();
            tokio::spawn(async move { driver.scheduler().tick().await })
        };
        let report = advance.await.unwrap();
        tick.await.unwrap().unwrap();

        assert!(
            env.probe.arrivals.load(Ordering::SeqCst) >= 2,
            "{name}: both reached the barrier"
        );
        assert!(
            env.driver.scheduler().stale_passes() >= 1,
            "{name}: the race opened: the store refused a pass ({report:?})"
        );
        let scheduled = env.scheduled(&run).await;
        for node in ["s", "c1"] {
            assert_eq!(
                scheduled.iter().filter(|n| *n == node).count(),
                1,
                "{name}: {node} dispatched once: {scheduled:?}"
            );
        }
        let completed = env.completed(&run).await;
        assert!(
            completed.iter().filter(|n| *n == "c1").count() <= 1,
            "{name}: c1 ran at most once: {completed:?}"
        );
        env.assert_invariants().await;
    }
}

/// Two advances on one run, 200 times: whichever dispatches a condition runs
/// it, and nothing runs twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_advances_on_one_run_run_each_engine_step_once() {
    for kind in KINDS {
        let counting = Counting::default();
        let Some(env) = Env::with_condition(
            kind,
            "two_advances_on_one_run",
            Some(Arc::new(counting.clone())),
        )
        .await
        else {
            continue;
        };
        env.register(&chain("wf-twice", 2)).await;
        let name = env.name();
        let mut stale = 0;
        let mut both_worked = 0;
        let began = std::time::Instant::now();

        for round in 0..200 {
            let run = env.started_and_s_done("wf-twice").await;
            let spawn_one = || {
                let driver = env.driver.clone();
                let run = run.clone();
                tokio::spawn(async move { driver.advance(&run, LONG).await })
            };
            let (a, b) = (spawn_one(), spawn_one());
            let (a, b) = (a.await.unwrap(), b.await.unwrap());

            assert_eq!(a.errors + b.errors, 0, "{name} round {round}: {a:?} {b:?}");
            assert_eq!(
                a.engine_steps + b.engine_steps,
                2,
                "{name} round {round}: two steps in all: {a:?} {b:?}"
            );
            stale += a.stale_passes + b.stale_passes;
            both_worked += (a.engine_steps > 0 && b.engine_steps > 0) as u32;
            for step in ["c1", "c2"] {
                assert_eq!(
                    counting.times(&run, step),
                    1,
                    "{name} round {round}: {step} ran once"
                );
            }
            assert_eq!(
                env.completed(&run).await,
                vec!["s", "c1", "c2"],
                "{name} round {round}"
            );
            assert_eq!(
                env.scheduled(&run).await,
                vec!["s", "c1", "c2", "last"],
                "{name} round {round}: each node dispatched once"
            );
        }
        assert_eq!(counting.total(), 400, "{name}");
        eprintln!(
            "{name}: 200 rounds in {:.1} s; stale passes {stale}, rounds both advances ran a step {both_worked}",
            began.elapsed().as_secs_f64()
        );
        assert!(
            stale > 0 || both_worked > 0,
            "{name}: the advances never overlapped, so nothing was raced \
             (stale passes {stale}, rounds both worked {both_worked})"
        );
        env.assert_invariants().await;
    }
}

/// The pass an advance builds also sweeps the run's approval deadlines: a
/// request past its deadline is recorded as expired and sealed, once, by the
/// scheduler the driver holds, however many advances follow.
#[tokio::test]
async fn an_advance_records_and_seals_an_expired_approval_once() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "an_advance_records_and_seals_an_expired_approval").await
        else {
            continue;
        };
        env.register(&workflow(
            "wf-expiry",
            vec![py("s"), review_within("review", Some(1)), py("publish")],
            &[("s", "review"), ("review", "publish")],
        ))
        .await;
        let run = env.started_and_s_done("wf-expiry").await;
        let name = env.name();
        env.driver.advance(&run, LONG).await;
        assert_eq!(env.requests(&run).await, 1, "{name}");
        tokio::time::sleep(Duration::from_millis(1300)).await;

        for _ in 0..3 {
            env.driver.advance(&run, LONG).await;
        }

        let expired = env
            .events(&run)
            .await
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ApprovalExpired { .. }))
            .count();
        assert_eq!(expired, 1, "{name}: recorded once");
        assert_eq!(
            env.sealed("approval_expired").await.len(),
            1,
            "{name}: sealed once"
        );
        assert_eq!(env.requests(&run).await, 1, "{name}: no second request");
    }
}

// ── Who the engine acts as ───────────────────────────────────────────────────

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A developer finishes the start step, and the advance that follows parks
/// the approval. The request is sealed as the engine's, not as the
/// developer's, and the step runs under a worker id of the engine's own.
#[tokio::test]
async fn engine_steps_are_never_attributed_to_the_caller() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "engine_steps_are_never_attributed").await else {
            continue;
        };
        let name = env.name();
        let (alice, token) = env.inner.create_token("alice", "developer").await.unwrap();
        let caller = jamjet_api::auth::principal_of(&token);
        let app = build_router_with_opts(env.state.clone(), false);
        let ir = workflow(
            "wf-attr",
            vec![py("s"), review("review"), py("publish")],
            &[("s", "review"), ("review", "publish")],
        );
        let (st, body) = call(&app, "POST", "/workflows", &alice, json!({ "ir": ir })).await;
        assert!(st.is_success(), "{name}: {st} {body}");
        let (st, body) = call(
            &app,
            "POST",
            "/executions",
            &alice,
            json!({ "workflow_id": "wf-attr", "workflow_version": "0.1.0", "input": {} }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{name}: {body}");
        let run = parse_id(body["execution_id"].as_str().unwrap());

        // The developer's own worker claims the start step and completes it.
        let (st, claim) = call(
            &app,
            "POST",
            "/work-items/claim",
            &alice,
            json!({ "worker_id": "dev-worker", "queue_types": ["python_tool"] }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{name}: {claim}");
        assert_eq!(claim["claimed"], true, "{name}: {claim}");
        let (st, body) = call(
            &app,
            "POST",
            &format!("/work-items/{}/complete", claim["work_item"]["id"].as_str().unwrap()),
            &alice,
            json!({ "output": {}, "state_patch": {}, "lease_fence": claim["work_item"]["lease_fence"] }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");

        let report = env.driver.advance(&run, LONG).await;

        assert_eq!(report.engine_steps, 1, "{name}: {report:?}");
        let sealed = env.sealed("tool_approval_required").await;
        assert_eq!(sealed.len(), 1, "{name}");
        assert_eq!(sealed[0].actor_id, "system", "{name}");
        assert_ne!(sealed[0].actor_id, caller, "{name}");
        assert_eq!(sealed[0].actor_type, ActorType::System, "{name}");
        assert_eq!(
            sealed[0].tenant_id, DEFAULT_TENANT,
            "{name}: the run's tenant"
        );
        let started: Vec<String> = env
            .events(&run)
            .await
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::NodeStarted {
                    node_id, worker_id, ..
                } if node_id.as_str() == "review" => Some(worker_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started.len(), 1, "{name}: {started:?}");
        assert!(started[0].starts_with("engine-"), "{name}: {started:?}");
        assert!(!started[0].contains(&token.id), "{name}: {started:?}");
    }
}

/// An approval step the engine runs inline raises its request once, however
/// many advances reach the run at the same moment: one of them dispatches the
/// step and runs it, and the others find it dispatched. (The worker's own
/// guard holds a policy-gated tool node once and fires it once on approval:
/// `run_item_for_holds_a_gated_node_once_and_fires_it_once_when_approved`.
/// A condition has no name for a policy to match, so `human_approval` is the
/// engine step an advance can gate.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_inline_engine_step_is_gated_once() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "an_inline_engine_step_is_gated").await else {
            continue;
        };
        env.register(&workflow(
            "wf-gated",
            vec![py("s"), review("review"), py("last")],
            &[("s", "review"), ("review", "last")],
        ))
        .await;
        let name = env.name();
        for round in 0..20 {
            let run = env.started_and_s_done("wf-gated").await;
            let handles: Vec<_> = (0..3)
                .map(|_| {
                    let driver = env.driver.clone();
                    let run = run.clone();
                    tokio::spawn(async move { driver.advance(&run, LONG).await })
                })
                .collect();
            let mut steps = 0;
            for handle in handles {
                let report = handle.await.unwrap();
                assert_eq!(report.errors, 0, "{name} round {round}: {report:?}");
                steps += report.engine_steps;
            }

            assert_eq!(steps, 1, "{name} round {round}: one advance ran the step");
            assert_eq!(
                env.requests(&run).await,
                1,
                "{name} round {round}: one request"
            );
            assert_eq!(env.completed(&run).await, vec!["s"], "{name} round {round}");
        }
        assert_eq!(
            env.sealed("tool_approval_required").await.len(),
            20,
            "{name}: each request sealed once"
        );
        env.assert_invariants_without_audit().await;
    }
}

// ── Errors, panics and the stop signal ───────────────────────────────────────

/// A failure inside an advance is counted and logged. The caller gets a
/// report, never an error and never a panic.
#[tokio::test]
async fn a_failing_or_panicking_step_is_counted_and_never_reaches_the_caller() {
    for kind in KINDS {
        // The store fails the engine's claim.
        let Some(env) = Env::new(kind, "a_failing_step_is_counted").await else {
            continue;
        };
        env.register(&chain("wf-fail", 1)).await;
        let run = env.started_and_s_done("wf-fail").await;
        let name = env.name();
        env.probe.fail_claims.store(true, Ordering::SeqCst);

        let report = env.driver.advance(&run, LONG).await;

        assert_eq!(report.errors, 1, "{name}: {report:?}");
        assert_eq!(report.engine_steps, 0, "{name}: {report:?}");
        assert_eq!(
            env.waiting(&run).await,
            vec!["c1"],
            "{name}: the item is still there for the tick"
        );

        // An executor that panics.
        let Some(env) =
            Env::with_condition(kind, "a_panicking_step_is_counted", Some(Arc::new(Panics))).await
        else {
            continue;
        };
        env.register(&chain("wf-panic", 1)).await;
        let run = env.started_and_s_done("wf-panic").await;

        let report = env.driver.advance(&run, LONG).await;

        assert_eq!(report.errors, 1, "{name}: {report:?}");
        assert_eq!(report.engine_steps, 0, "{name}: {report:?}");
    }
}

/// Once the stop signal has gone, an advance starts nothing.
#[tokio::test]
async fn an_advance_after_the_stop_signal_starts_nothing() {
    for kind in KINDS {
        let Some(env) = Env::new(kind, "an_advance_after_the_stop_signal").await else {
            continue;
        };
        env.register(&chain("wf-stopped", 1)).await;
        let run = env.started_and_s_done("wf-stopped").await;
        let name = env.name();
        let events = env.events(&run).await.len();
        let trigger = env.trigger.lock().unwrap().take().unwrap();
        assert_eq!(trigger.stop_and_drain(Duration::from_millis(50)).await, 0);

        let report = env.driver.advance(&run, LONG).await;

        assert_eq!(report, AdvanceReport::default(), "{name}");
        assert_eq!(
            env.events(&run).await.len(),
            events,
            "{name}: nothing written"
        );
    }
}

/// The signal arrives while a step is in hand. The drain waits for the
/// advance, the step finishes, and the other two steps the pass dispatched
/// are not started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_signal_mid_advance_finishes_the_step_in_hand_and_starts_no_more() {
    for kind in KINDS {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let holding = Holding {
            entered: entered.clone(),
            release: release.clone(),
        };
        let Some(env) =
            Env::with_condition(kind, "a_stop_signal_mid_advance", Some(Arc::new(holding))).await
        else {
            continue;
        };
        // Three conditions come out of the start node in one pass.
        env.register(&workflow(
            "wf-fan",
            vec![py("s"), cond("c1"), cond("c2"), cond("c3")],
            &[("s", "c1"), ("s", "c2"), ("s", "c3")],
        ))
        .await;
        let run = env.started_and_s_done("wf-fan").await;
        let name = env.name();
        let advance = {
            let driver = env.driver.clone();
            let run = run.clone();
            tokio::spawn(async move { driver.advance(&run, LONG).await })
        };
        tokio::time::timeout(Duration::from_secs(20), entered.notified())
            .await
            .unwrap_or_else(|_| panic!("{name}: no step began"));

        // The signal, with the step in hand. The drain finds work running
        // and reports it when its grace period ends.
        let trigger = env.trigger.lock().unwrap().take().unwrap();
        let still = trigger.stop_and_drain(Duration::from_millis(200)).await;
        assert!(
            still > 0,
            "{name}: the drain did not see the advance running"
        );
        assert!(!advance.is_finished(), "{name}: the step is still held");

        // Let the step go, and any other that starts, so a bug fails the
        // case instead of hanging it.
        release.add_permits(10);
        let report = advance.await.unwrap();

        assert_eq!(report.engine_steps, 1, "{name}: {report:?}");
        assert_eq!(report.errors, 0, "{name}: {report:?}");
        assert!(!report.out_of_budget, "{name}: {report:?}");
        let done = env.completed(&run).await;
        assert_eq!(done.len(), 2, "{name}: s and one condition: {done:?}");
        assert_eq!(
            env.waiting(&run).await.len(),
            2,
            "{name}: the other two conditions were not started"
        );
    }
}

#[test]
fn the_quick_kinds_are_the_ones_named() {
    assert_eq!(
        QUICK_KINDS,
        &["condition", "human_approval", "limit_exceeded"]
    );
}
