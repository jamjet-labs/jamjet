//! A run's token cap and dollar cap, on both transports.
//!
//! A workflow caps its runs with `token_budget` and `cost_budget_usd`. Until
//! 2026-11 only the in-process worker read them, from the `__budget` key it
//! kept in the run's state, and it checked them after the model call: a trip
//! there threw the paid result away and left the item leased, so the retry
//! paid for the call again, and a failed log read in that check let the turn
//! through. An HTTP worker's reported tokens and cost were stored and never
//! checked. Each test here pins what must hold instead, and
//! `both_transports_fail_at_the_same_step` that both transports stop a run at
//! the same step for the same spend.
//!
//! The tests after it pin the cost a step reports. Over HTTP it must be a
//! number from 0 to 10,000 in USD, or the completion is refused, and under a
//! dollar cap the refused step also fails for good, so it is never paid for
//! again unrecorded. In process, a replayed result keeps the cost it was
//! recorded with. Either way a step is counted once.
//!
//! Runs are driven by hand, on memory and on SQLite: a scheduler tick, then
//! one worker poll (in process) or one claim and completion (HTTP), until the
//! run ends. An item the in-process worker leaves claimed is let go the way
//! time lets it go: its lease runs out and so does the effect reservation its
//! attempt holds, so the next claim takes it back as a retry.

mod support;

use axum::http::StatusCode;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_models::{
    ModelAdapter, ModelError, ModelRegistry, ModelRequest, ModelResponse, StructuredRequest,
};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, StateBackendError, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::{caps, fold_run_usage, RunCaps};
use jamjet_testkit::{Hooks, Intercept, Op};
use jamjet_worker::{ExecutionResult, ExecutorError, ModelNodeExecutor, NodeExecutor, Worker};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::TestBackend;

/// The id every in-process worker here runs under.
const WORKER: &str = "w";

/// How many rounds a run gets to end in.
const ROUNDS: usize = 30;

fn python_fn() -> Value {
    json!({ "type": "python_fn", "module": "app.steps", "function": "run", "output_schema": "" })
}

fn model() -> Value {
    json!({ "type": "model", "model_ref": "fake-model", "prompt_ref": "say hi", "output_schema": "" })
}

/// `ids[0] -> ids[1] -> ... -> end`, every node of `kind`, with `caps`
/// set on the workflow.
fn linear_ir(workflow_id: &str, ids: &[&str], kind: Value, caps: Value) -> Value {
    let nodes: serde_json::Map<String, Value> = ids
        .iter()
        .map(|id| {
            (
                id.to_string(),
                json!({
                    "id": id,
                    "kind": kind,
                    "retry_policy": null,
                    "node_timeout_secs": null,
                    "description": null,
                    "labels": {}
                }),
            )
        })
        .collect();
    let mut edges: Vec<Value> = ids
        .windows(2)
        .map(|w| json!({ "from": w[0], "to": w[1], "condition": null }))
        .collect();
    edges.push(json!({ "from": ids[ids.len() - 1], "to": "end", "condition": null }));
    let mut ir = json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": ids[0],
        "nodes": nodes,
        "edges": edges,
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    });
    for (key, value) in caps.as_object().unwrap() {
        ir[key] = value.clone();
    }
    ir
}

/// The trip a run over its token cap records.
fn token_trip(node: &str, limit: u64, current: u64) -> Value {
    json!({
        "type": "token_budget_exceeded",
        "node_id": node,
        "kind": "total_tokens",
        "limit": limit,
        "current": current
    })
}

/// The lease each in-process call ran under, by item: what the test needs to
/// let the item go after the worker left it claimed.
type Held = Arc<Mutex<HashMap<uuid::Uuid, i64>>>;

/// One started run and the scheduler that drives it.
struct Run {
    name: &'static str,
    tb: TestBackend,
    backend: Arc<dyn StateBackend>,
    state: jamjet_api::state::AppState,
    eid: ExecutionId,
    scheduler: Scheduler,
}

async fn start(tb: TestBackend, ir: Value) -> Run {
    let backend = tb.dyn_backend();
    let state = support::make_state(backend.clone());
    let id = support::register_and_start(&state, ir, json!({})).await;
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap_or(&id)).unwrap());
    Run {
        name: tb.name(),
        scheduler: Scheduler::new(backend.clone()),
        tb,
        backend,
        state,
        eid,
    }
}

impl Run {
    async fn status(&self) -> WorkflowStatus {
        self.backend
            .get_execution(&self.eid)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    async fn ended(&self) -> bool {
        self.status().await.is_terminal()
    }

    async fn events(&self) -> Vec<EventKind> {
        self.backend
            .get_events(&self.eid)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    /// The log as `type(node)` strings, for a failure message.
    async fn log(&self) -> Vec<String> {
        self.events()
            .await
            .iter()
            .map(|k| {
                let v = serde_json::to_value(k).unwrap();
                match v["node_id"].as_str() {
                    Some(n) => format!("{}({n})", v["type"].as_str().unwrap_or("?")),
                    None => v["type"].as_str().unwrap_or("?").to_string(),
                }
            })
            .collect()
    }

    /// Every cap trip in the log, as JSON.
    async fn trips(&self) -> Vec<Value> {
        self.events()
            .await
            .iter()
            .filter(|k| {
                matches!(
                    k,
                    EventKind::TokenBudgetExceeded { .. } | EventKind::CostBudgetExceeded { .. }
                )
            })
            .map(|k| serde_json::to_value(k).unwrap())
            .collect()
    }

    /// The nodes completed, in log order.
    async fn completed(&self) -> Vec<String> {
        self.events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::NodeCompleted { node_id, .. } => Some(node_id),
                _ => None,
            })
            .collect()
    }

    /// The nodes dispatched, in log order.
    async fn scheduled(&self) -> Vec<String> {
        self.events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::NodeScheduled { node_id, .. } => Some(node_id),
                _ => None,
            })
            .collect()
    }

    /// Let go of every item the in-process worker left claimed, as time
    /// does: its lease runs out, and so does the effect reservation its
    /// attempt holds. The next claim then takes the item back as a retry,
    /// and the retry may fire.
    async fn lapse(&self, held: &Held) {
        let open = self.backend.open_work_items(&self.eid).await.unwrap();
        for item in open.into_iter().filter(|i| i.claimed) {
            let Some(fence) = held.lock().unwrap().get(&item.id).copied() else {
                continue;
            };
            self.tb.expire_lease(item.id).await;
            let key = jamjet_state::derive_idempotency_key(
                self.backend.as_ref(),
                &self.eid,
                &item.node_id,
            )
            .await
            .unwrap();
            self.backend
                .release_tool_reservation(&key, WORKER, fence)
                .await
                .unwrap();
        }
    }

    /// Tick and poll `worker` until the run ends, at most [`ROUNDS`] times.
    async fn drive_in_process(&self, worker: &Worker, held: &Held) {
        for _ in 0..ROUNDS {
            self.scheduler.tick().await.expect("scheduler tick");
            if self.ended().await {
                return;
            }
            self.lapse(held).await;
            // A trip, or a refusal, is an error here: the log shows it.
            let _ = worker.poll_and_execute().await;
        }
    }

    /// Tick, then claim one `python_fn` step over HTTP and complete it with
    /// `report(node)` in the body, until the run ends, at most [`ROUNDS`]
    /// times. Returns how many steps were handed out.
    async fn drive_http(&self, report: impl Fn(&str) -> Value) -> usize {
        let mut claims = 0;
        for _ in 0..ROUNDS {
            self.scheduler.tick().await.expect("scheduler tick");
            if self.ended().await {
                break;
            }
            let (st, claimed) = support::post(
                &self.state,
                "/work-items/claim",
                json!({ "worker_id": "http-w", "queue_types": ["python_tool"] }),
            )
            .await;
            assert!(st.is_success(), "claim: {st} {claimed}");
            if claimed["claimed"] != json!(true) {
                continue;
            }
            claims += 1;
            let wi = &claimed["work_item"];
            let mut body = json!({
                "output": {},
                "state_patch": {},
                "lease_fence": wi["lease_fence"],
                "idempotency_key": wi["idempotency_key"]
            });
            for (key, value) in report(wi["node_id"].as_str().unwrap()).as_object().unwrap() {
                body[key] = value.clone();
            }
            let (st, done) = support::post(
                &self.state,
                &format!("/work-items/{}/complete", wi["id"].as_str().unwrap()),
                body,
            )
            .await;
            assert!(st.is_success(), "complete: {st} {done}");
        }
        claims
    }
}

/// What a model step reports: its model and `input_tokens` tokens, no cost.
fn spent(input_tokens: u64) -> ExecutionResult {
    ExecutionResult {
        output: json!({}),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: Some("test".into()),
        gen_ai_model: Some("fake-model".into()),
        input_tokens: Some(input_tokens),
        output_tokens: Some(0),
        finish_reason: Some("stop".into()),
        cost_usd: None,
    }
}

/// Stands in for a paid call: reports `tokens` tokens, counts its calls and
/// remembers the lease each call ran under.
#[derive(Clone)]
struct Spender {
    tokens: u64,
    calls: Arc<AtomicUsize>,
    held: Held,
}

impl Spender {
    fn new(tokens: u64, held: &Held) -> Self {
        Self {
            tokens,
            calls: Arc::new(AtomicUsize::new(0)),
            held: held.clone(),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl NodeExecutor for Spender {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.held.lock().unwrap().insert(item.id, item.lease_fence);
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(spent(self.tokens))
    }
}

/// A model that answers every call with `tokens` input tokens, and counts
/// its calls.
struct FakeModel {
    tokens: u64,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelAdapter for FakeModel {
    fn system_name(&self) -> &'static str {
        "fake"
    }
    fn default_model(&self) -> &str {
        "fake-model"
    }
    async fn chat(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelResponse {
            content: "ok".into(),
            model: "fake-model".into(),
            finish_reason: "stop".into(),
            input_tokens: self.tokens,
            output_tokens: 0,
            structured: None,
            tool_calls: vec![],
            cost_usd: None,
        })
    }
    async fn structured_output(&self, _: StructuredRequest) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }
}

/// The engine's model executor, remembering the lease each call ran under.
struct Leased {
    inner: ModelNodeExecutor,
    held: Held,
}

#[async_trait::async_trait]
impl NodeExecutor for Leased {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.held.lock().unwrap().insert(item.id, item.lease_fence);
        self.inner.execute(item).await
    }
}

fn model_worker(backend: Arc<dyn StateBackend>, executor: Arc<dyn NodeExecutor>) -> Worker {
    Worker::new(WORKER.into(), backend, vec!["model".into()]).register_executor("model", executor)
}

/// A step under `total_tokens: 1` reports 2 tokens. The check after the
/// call used to throw the result away on the trip and leave the item leased.
/// Once the lease and the reservation ran out, the retry made the call again
/// and tripped again, until the node's attempts were spent. The step's spend
/// must be recorded once and the run failed, with no call after the first.
#[tokio::test]
async fn a_trip_after_the_call_does_not_pay_again() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-pay-again",
                &["n1"],
                model(),
                json!({ "token_budget": { "total_tokens": 1 } }),
            ),
        )
        .await;
        let held = Held::default();
        let spender = Spender::new(2, &held);
        let worker = model_worker(run.backend.clone(), Arc::new(spender.clone()));
        run.drive_in_process(&worker, &held).await;

        let name = run.name;
        assert_eq!(
            spender.calls(),
            1,
            "{name}: the paid call was made again after the trip; the log was {:?}",
            run.log().await
        );
        let recorded: Vec<Option<u64>> = run
            .events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::NodeCompleted {
                    node_id,
                    input_tokens,
                    ..
                } if node_id == "n1" => Some(input_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(recorded, vec![Some(2)], "{name}: the paid result is kept");
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
        assert_eq!(run.trips().await, vec![token_trip("n1", 1, 2)], "{name}");
    }
}

/// Refuses every `latest_sequence` the worker makes after its executor was
/// called and before it commits the turn: the read a cap check after the
/// call makes. Counts what it refused.
struct FailReadAfterCall {
    calls: Arc<AtomicUsize>,
    committed: AtomicBool,
    refused: AtomicUsize,
}

#[async_trait::async_trait]
impl Hooks for FailReadAfterCall {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        match op {
            Op::CommitTurn { .. } | Op::CommitTurnUnlessTerminal { .. } => {
                self.committed.store(true, Ordering::SeqCst);
                None
            }
            Op::LatestSequence(_)
                if self.calls.load(Ordering::SeqCst) > 0
                    && !self.committed.load(Ordering::SeqCst) =>
            {
                self.refused.fetch_add(1, Ordering::SeqCst);
                Some(StateBackendError::Database(
                    "the log could not be read (test)".into(),
                ))
            }
            _ => None,
        }
    }
}

/// A step under `total_tokens: 1` reports 2 tokens, and the log read in the
/// worker's post-call check fails. That check read the failure as "within
/// budget": the turn committed and the run completed past its cap. The run
/// must fail. A worker that makes no read between the call and its commit
/// gives the hook nothing to refuse, and the cap must hold all the same.
#[tokio::test]
async fn a_failed_read_at_the_cap_does_not_let_the_turn_through() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-failed-read",
                &["n1"],
                model(),
                json!({ "token_budget": { "total_tokens": 1 } }),
            ),
        )
        .await;
        let held = Held::default();
        let spender = Spender::new(2, &held);
        let hooks = Arc::new(FailReadAfterCall {
            calls: spender.calls.clone(),
            committed: AtomicBool::new(false),
            refused: AtomicUsize::new(0),
        });
        let through: Arc<dyn StateBackend> =
            Arc::new(Intercept::new(run.backend.clone(), hooks.clone()));
        let worker = model_worker(through, Arc::new(spender.clone()));
        run.drive_in_process(&worker, &held).await;

        let name = run.name;
        assert_eq!(
            run.status().await,
            WorkflowStatus::Failed,
            "{name}: the run went on past its cap after {} failed log read(s) in the cap \
             check; the log was {:?}",
            hooks.refused.load(Ordering::SeqCst),
            run.log().await
        );
        assert_eq!(run.trips().await, vec![token_trip("n1", 1, 2)], "{name}");
        assert_eq!(spender.calls(), 1, "{name}");
    }
}

/// `a -> b`, both `python_fn`, under `total_tokens: 5`. An HTTP worker runs
/// `a` and reports 10 tokens. The run must fail before `b` is dispatched.
#[tokio::test]
async fn the_http_path_enforces_the_token_cap() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-http-tokens",
                &["a", "b"],
                python_fn(),
                json!({ "token_budget": { "total_tokens": 5 } }),
            ),
        )
        .await;
        let claims = run
            .drive_http(|_| json!({ "input_tokens": 7, "output_tokens": 3 }))
            .await;

        let name = run.name;
        assert_eq!(
            run.status().await,
            WorkflowStatus::Failed,
            "{name}: the run went on past its token cap; the log was {:?}",
            run.log().await
        );
        assert_eq!(run.trips().await, vec![token_trip("a", 5, 10)], "{name}");
        assert_eq!(
            run.scheduled().await,
            vec!["a"],
            "{name}: b ran past the cap"
        );
        assert_eq!(claims, 1, "{name}");
    }
}

/// `a -> b`, both `python_fn`, under `cost_budget_usd: 5.0`. An HTTP worker
/// runs `a` and reports `cost_usd: 10.0`. The run must fail before `b` is
/// dispatched.
#[tokio::test]
async fn the_http_path_enforces_the_dollar_cap() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-http-dollars",
                &["a", "b"],
                python_fn(),
                json!({ "cost_budget_usd": 5.0 }),
            ),
        )
        .await;
        let claims = run.drive_http(|_| json!({ "cost_usd": 10.0 })).await;

        let name = run.name;
        assert_eq!(
            run.status().await,
            WorkflowStatus::Failed,
            "{name}: the run went on past its dollar cap; the log was {:?}",
            run.log().await
        );
        assert_eq!(
            run.trips().await,
            vec![json!({
                "type": "cost_budget_exceeded",
                "node_id": "a",
                "limit_usd": 5.0,
                "current_usd": 10.0
            })],
            "{name}"
        );
        assert_eq!(
            run.scheduled().await,
            vec!["a"],
            "{name}: b ran past the cap"
        );
        assert_eq!(claims, 1, "{name}");
    }
}

/// Where a run stopped, read from its log, and how many calls (in process)
/// or claims (HTTP) it took.
#[derive(Debug, PartialEq)]
struct Outcome {
    status: WorkflowStatus,
    completed: Vec<String>,
    scheduled: Vec<String>,
    trips: Vec<Value>,
    steps_run: usize,
}

impl Outcome {
    async fn of(run: &Run, steps_run: usize) -> Self {
        Self {
            status: run.status().await,
            completed: run.completed().await,
            scheduled: run.scheduled().await,
            trips: run.trips().await,
            steps_run,
        }
    }
}

/// `s1 -> s2 -> s3 -> s4` under `total_tokens: 10`, each step spending 4
/// tokens: through the engine's model executor over a fake model in
/// process, and as `python_fn` steps over HTTP. `s3` is claimed at 8 and
/// takes the run to 12. Both runs must keep `s3`'s spend, fail there, and
/// never dispatch `s4`.
#[tokio::test]
async fn both_transports_fail_at_the_same_step() {
    let caps = json!({ "token_budget": { "total_tokens": 10 } });
    let steps = ["s1", "s2", "s3", "s4"];
    let in_process_backends = support::backends().await;
    let http_backends = support::backends().await;
    for (in_tb, http_tb) in in_process_backends.into_iter().zip(http_backends) {
        let run = start(in_tb, linear_ir("wf-both", &steps, model(), caps.clone())).await;
        let name = run.name;
        let calls = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(
            ModelRegistry::new()
                .register(Arc::new(FakeModel {
                    tokens: 4,
                    calls: calls.clone(),
                }))
                .with_default("fake"),
        );
        let held = Held::default();
        let worker = model_worker(
            run.backend.clone(),
            Arc::new(Leased {
                inner: ModelNodeExecutor::new(registry),
                held: held.clone(),
            }),
        );
        run.drive_in_process(&worker, &held).await;
        let in_process = Outcome::of(&run, calls.load(Ordering::SeqCst)).await;

        let run = start(
            http_tb,
            linear_ir("wf-both", &steps, python_fn(), caps.clone()),
        )
        .await;
        let claims = run
            .drive_http(|_| json!({ "input_tokens": 4, "output_tokens": 0 }))
            .await;
        let http = Outcome::of(&run, claims).await;

        assert_eq!(
            (&in_process.status, &http.status),
            (&WorkflowStatus::Failed, &WorkflowStatus::Failed),
            "{name}: both runs must fail; in process {in_process:?}, over HTTP {http:?}"
        );
        assert_eq!(
            in_process, http,
            "{name}: the transports stopped the run at different steps"
        );
        assert_eq!(
            in_process,
            Outcome {
                status: WorkflowStatus::Failed,
                completed: vec!["s1".into(), "s2".into(), "s3".into()],
                scheduled: vec!["s1".into(), "s2".into(), "s3".into()],
                trips: vec![token_trip("s3", 10, 12)],
                steps_run: 3,
            },
            "{name}"
        );
    }
}

// ── The cost a step reports ──────────────────────────────────────────────────

impl Run {
    /// Tick, then claim the `python_fn` step that is ready, over HTTP.
    async fn claim_http(&self) -> Value {
        self.scheduler.tick().await.expect("scheduler tick");
        let (st, claimed) = support::post(
            &self.state,
            "/work-items/claim",
            json!({ "worker_id": "http-w", "queue_types": ["python_tool"] }),
        )
        .await;
        assert!(
            st.is_success() && claimed["claimed"] == json!(true),
            "{}: claim: {st} {claimed}",
            self.name
        );
        claimed["work_item"].clone()
    }

    /// Complete the claimed `wi` with the keys of `report` in the body.
    async fn complete_http(&self, wi: &Value, report: Value) -> (StatusCode, Value) {
        let mut body = json!({
            "output": {},
            "state_patch": {},
            "lease_fence": wi["lease_fence"],
            "idempotency_key": wi["idempotency_key"]
        });
        for (key, value) in report.as_object().unwrap() {
            body[key] = value.clone();
        }
        support::post(
            &self.state,
            &format!("/work-items/{}/complete", wi["id"].as_str().unwrap()),
            body,
        )
        .await
    }

    /// Complete `wi` with `body` sent as it is, so it can hold JSON that a
    /// `Value` cannot (a number out of range).
    async fn complete_raw(&self, wi: &Value, body: String) -> StatusCode {
        use tower::ServiceExt;
        jamjet_api::routes::build_router_with_opts(self.state.clone(), true)
            .oneshot(
                axum::http::Request::post(format!(
                    "/work-items/{}/complete",
                    wi["id"].as_str().unwrap()
                ))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body))
                .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    /// The log entries that settle a step: `node_completed(..)` and
    /// `node_failed(..)`.
    async fn settled(&self) -> Vec<String> {
        self.log()
            .await
            .into_iter()
            .filter(|e| e.starts_with("node_completed") || e.starts_with("node_failed"))
            .collect()
    }

    async fn item_status(&self, wi: &Value) -> String {
        self.tb
            .item_status(uuid::Uuid::parse_str(wi["id"].as_str().unwrap()).unwrap())
            .await
    }
}

/// A reported cost above the per-call ceiling of $10,000, or in a currency
/// other than USD, cannot be counted: the completion is 400 and nothing is
/// recorded. Without a dollar cap the item stays the worker's, and a
/// completion with a cost the engine can count is then recorded; the ceiling
/// itself is a cost it can count.
#[tokio::test]
async fn http_cost_above_the_ceiling_or_in_eur_is_400() {
    for tb in support::backends().await {
        let run = start(tb, linear_ir("wf-cost-400", &["a"], python_fn(), json!({}))).await;
        let name = run.name;
        let wi = run.claim_http().await;
        for (report, error) in [
            (
                json!({ "cost_usd": 10000.01 }),
                "cost_usd must be a number from 0 to 10000",
            ),
            (
                json!({ "cost_usd": 0.01, "cost_currency": "EUR" }),
                "cost_currency must be \"USD\"",
            ),
            (
                json!({ "cost_usd": 0.01, "cost_currency": "usd" }),
                "cost_currency must be \"USD\"",
            ),
            (
                json!({ "cost_usd": -1.0 }),
                "cost_usd must be a number from 0 to 10000",
            ),
        ] {
            let (st, body) = run.complete_http(&wi, report.clone()).await;
            assert_eq!(
                (st, body),
                (StatusCode::BAD_REQUEST, json!({ "error": error })),
                "{name}: {report}"
            );
            assert_eq!(
                run.settled().await,
                Vec::<String>::new(),
                "{name}: {report}"
            );
            assert_eq!(run.item_status(&wi).await, "claimed", "{name}: {report}");
        }
        let (st, body) = run
            .complete_http(&wi, json!({ "cost_usd": 10000.0, "cost_currency": "USD" }))
            .await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");
        assert_eq!(run.settled().await, vec!["node_completed(a)"], "{name}");
    }
}

/// A cost sent as a string is refused by the JSON extractor (422) before
/// the route runs, with or without a dollar cap: nothing is recorded and the
/// item stays the worker's.
///
/// GUARD: passes before this change too; it pins that a string is never
/// read as a cost or as no cost.
#[tokio::test]
async fn http_cost_as_a_string_is_422() {
    for caps in [json!({}), json!({ "cost_budget_usd": 1.0 })] {
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir("wf-cost-string", &["a"], python_fn(), caps.clone()),
            )
            .await;
            let name = run.name;
            let wi = run.claim_http().await;
            let (st, body) = run.complete_http(&wi, json!({ "cost_usd": "0.5" })).await;
            assert_eq!(
                st,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{name} {caps}: {body}"
            );
            assert_eq!(run.settled().await, Vec::<String>::new(), "{name} {caps}");
            assert_eq!(run.item_status(&wi).await, "claimed", "{name} {caps}");
        }
    }
}

/// `1e999` is past what an `f64` holds. It is refused, never read as
/// infinity or as no cost: nothing is recorded and the item stays the
/// worker's, with or without a dollar cap.
///
/// GUARD: passes before this change too. The JSON parser refuses the number
/// before the route runs; the route's own range check is the second line if
/// a parser ever reads it as infinity.
#[tokio::test]
async fn http_cost_1e999_is_refused() {
    for caps in [json!({}), json!({ "cost_budget_usd": 1.0 })] {
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir("wf-cost-huge", &["a"], python_fn(), caps.clone()),
            )
            .await;
            let name = run.name;
            let wi = run.claim_http().await;
            let st = run
                .complete_raw(
                    &wi,
                    format!(
                        r#"{{"output": {{}}, "state_patch": {{}}, "lease_fence": {}, "cost_usd": 1e999}}"#,
                        wi["lease_fence"]
                    ),
                )
                .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{name} {caps}");
            assert_eq!(run.settled().await, Vec::<String>::new(), "{name} {caps}");
            assert_eq!(run.item_status(&wi).await, "claimed", "{name} {caps}");
        }
    }
}

/// `a -> b` under `cost_budget_usd: 1.0`. A worker ran `a`, so it may have
/// paid, and reports a cost the engine cannot count. The completion is 400
/// and the step fails for good in the same fenced write: a retry would pay
/// again with nothing recorded. The worker's next completion of the item is
/// refused, the run fails and `b` never runs.
#[tokio::test]
async fn a_malformed_cost_under_a_dollar_cap_fails_the_node_for_good() {
    for (report, problem) in [
        (
            json!({ "cost_usd": -0.5 }),
            "cost_usd must be a number from 0 to 10000",
        ),
        (
            json!({ "cost_usd": 20000.0 }),
            "cost_usd must be a number from 0 to 10000",
        ),
        (
            json!({ "cost_usd": 0.2, "cost_currency": "EUR" }),
            "cost_currency must be \"USD\"",
        ),
    ] {
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir(
                    "wf-bad-cost",
                    &["a", "b"],
                    python_fn(),
                    json!({ "cost_budget_usd": 1.0 }),
                ),
            )
            .await;
            let name = run.name;
            let wi = run.claim_http().await;
            let (st, body) = run.complete_http(&wi, report.clone()).await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{name}: {report}: {body}");
            let failed: Vec<(String, String, bool)> = run
                .events()
                .await
                .into_iter()
                .filter_map(|k| match k {
                    EventKind::NodeFailed {
                        node_id,
                        error,
                        retryable,
                        ..
                    } => Some((node_id, error, retryable)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                failed,
                vec![(
                    "a".to_string(),
                    format!(
                        "the step reported a cost the run's dollar cap cannot count: {problem}"
                    ),
                    false
                )],
                "{name}: {report}"
            );
            assert_eq!(
                body,
                json!({
                    "error": format!("{problem}; the run has a dollar cap, so the step failed")
                }),
                "{name}: {report}"
            );
            assert!(run.completed().await.is_empty(), "{name}: {report}");
            assert_ne!(run.item_status(&wi).await, "claimed", "{name}: {report}");
            let (st, body) = run.complete_http(&wi, json!({ "cost_usd": 0.2 })).await;
            assert_eq!(st, StatusCode::CONFLICT, "{name}: {report}: {body}");
            run.scheduler.tick().await.expect("scheduler tick");
            assert_eq!(
                run.status().await,
                WorkflowStatus::Failed,
                "{name}: {report}"
            );
            assert_eq!(run.scheduled().await, vec!["a"], "{name}: {report}");
            assert!(run.completed().await.is_empty(), "{name}: {report}");
        }
    }
}

/// Under a dollar cap, a completion with a cost the engine cannot count
/// fails the step only for the caller that holds the item, and only for a
/// step whose result it could report. With no fence, with a fence that is
/// not the item's, or for a step the engine runs itself, the answer is 400
/// and nothing is written: the item stays as it was.
#[tokio::test]
async fn a_malformed_cost_fails_only_a_step_the_caller_may_complete() {
    let caps = json!({ "cost_budget_usd": 1.0 });
    let plain = json!({ "error": "cost_usd must be a number from 0 to 10000" });
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir("wf-bad-cost-holder", &["a"], python_fn(), caps.clone()),
        )
        .await;
        let name = run.name;
        let wi = run.claim_http().await;
        let fence = wi["lease_fence"].as_i64().unwrap();
        for (case, body) in [
            (
                "no fence",
                json!({ "output": {}, "state_patch": {}, "cost_usd": -1.0 }),
            ),
            (
                "another fence",
                json!({ "output": {}, "state_patch": {}, "lease_fence": fence + 1, "cost_usd": -1.0 }),
            ),
        ] {
            let (st, answer) = support::post(
                &run.state,
                &format!("/work-items/{}/complete", wi["id"].as_str().unwrap()),
                body,
            )
            .await;
            assert_eq!(
                (st, answer),
                (StatusCode::BAD_REQUEST, plain.clone()),
                "{name}: {case}"
            );
            assert_eq!(run.settled().await, Vec::<String>::new(), "{name}: {case}");
            assert_eq!(run.item_status(&wi).await, "claimed", "{name}: {case}");
        }
    }
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir("wf-bad-cost-engine", &["m"], model(), caps.clone()),
        )
        .await;
        let name = run.name;
        run.scheduler.tick().await.expect("scheduler tick");
        // The engine's own worker holds the step; its fence reaches the route.
        let item = run
            .backend
            .claim_work_item(WORKER, &["model"])
            .await
            .unwrap()
            .expect("the model step is ready");
        let (st, answer) = support::post(
            &run.state,
            &format!("/work-items/{}/complete", item.id),
            json!({ "output": {}, "state_patch": {}, "lease_fence": item.lease_fence, "cost_usd": -1.0 }),
        )
        .await;
        assert_eq!(
            (st, answer),
            (StatusCode::BAD_REQUEST, plain.clone()),
            "{name}"
        );
        assert_eq!(run.settled().await, Vec::<String>::new(), "{name}");
        assert_eq!(run.tb.item_status(item.id).await, "claimed", "{name}");
    }
}

/// A worker completes a step with `cost_usd: 0.4` and the answer is lost on
/// the way back, so it sends the same completion again. The second one is
/// 409 and the step counts once.
///
/// GUARD: passes before this change too; it pins that the fence, not the
/// usage fold, refuses the repeat (on SQLite the repeat derives another
/// idempotency key, so the fold's dedupe would not catch it).
#[tokio::test]
async fn a_completion_retried_after_a_lost_answer_counts_once() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-lost-answer",
                &["a", "b"],
                python_fn(),
                json!({ "cost_budget_usd": 1.0 }),
            ),
        )
        .await;
        let name = run.name;
        let wi = run.claim_http().await;
        let report = json!({ "cost_usd": 0.4, "input_tokens": 3, "output_tokens": 1 });
        let (st, body) = run.complete_http(&wi, report.clone()).await;
        assert_eq!(st, StatusCode::OK, "{name}: {body}");
        let (st, body) = run.complete_http(&wi, report).await;
        assert_eq!(
            (st, body),
            (
                StatusCode::CONFLICT,
                json!({ "completed": false, "reason": "stale or invalid lease fence" })
            ),
            "{name}"
        );
        assert_eq!(run.completed().await, vec!["a"], "{name}");
        let usage = fold_run_usage(run.backend.as_ref(), &run.eid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (usage.billable_steps, usage.cost_nanos, usage.input_tokens),
            (1, 400_000_000, 3),
            "{name}"
        );
    }
}

/// A model that waits for the test before it answers, so a second worker can
/// reach the same step meanwhile. Each call reports 4 tokens and
/// `cost_usd: 0.0123`.
struct GatedModel {
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl ModelAdapter for GatedModel {
    fn system_name(&self) -> &'static str {
        "fake"
    }
    fn default_model(&self) -> &str {
        "fake-model"
    }
    async fn chat(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ModelResponse {
            content: "ok".into(),
            model: "fake-model".into(),
            finish_reason: "stop".into(),
            input_tokens: 4,
            output_tokens: 0,
            structured: None,
            tool_calls: vec![],
            cost_usd: Some(0.0123),
        })
    }
    async fn structured_output(&self, _: StructuredRequest) -> Result<ModelResponse, ModelError> {
        unimplemented!()
    }
}

/// Tells the test when `owner` has found the step's reservation held twice:
/// it then waits for the holder's result instead of calling the model.
struct WaitsOnHolder {
    owner: &'static str,
    held: AtomicUsize,
    waiting: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl Hooks for WaitsOnHolder {
    async fn after(&self, op: &Op<'_>) {
        if let Op::ReserveToolEffect { owner, .. } = op {
            if *owner == self.owner && self.held.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                self.waiting.notify_one();
            }
        }
    }
}

/// Two items for one model step, as a crash window or a hand enqueue makes.
/// The first worker calls the model, which costs $0.0123. The second derives
/// the same idempotency key, finds it reserved, waits, and replays the
/// recorded result instead of calling the model. The replayed completion
/// must keep the cost it was recorded with, and the run's usage must count
/// the step once: under a $0.02 cap, counting it twice would pass the cap.
#[tokio::test]
async fn a_replayed_result_keeps_its_cost_and_counts_once() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-replay",
                &["n1"],
                model(),
                json!({ "cost_budget_usd": 0.02 }),
            ),
        )
        .await;
        let name = run.name;
        run.scheduler.tick().await.expect("scheduler tick");
        let open = run.backend.open_work_items(&run.eid).await.unwrap();
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        let first = run
            .backend
            .get_work_item(open[0].id)
            .await
            .unwrap()
            .unwrap();
        run.backend
            .enqueue_work_item(WorkItem {
                id: uuid::Uuid::new_v4(),
                created_at: chrono::Utc::now(),
                ..first
            })
            .await
            .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let registry = Arc::new(
            ModelRegistry::new()
                .register(Arc::new(GatedModel {
                    calls: calls.clone(),
                    entered: entered.clone(),
                    release: release.clone(),
                }))
                .with_default("fake"),
        );
        let executor: Arc<dyn NodeExecutor> = Arc::new(ModelNodeExecutor::new(registry));
        let hooks = Arc::new(WaitsOnHolder {
            owner: "w2",
            held: AtomicUsize::new(0),
            waiting: Arc::new(tokio::sync::Notify::new()),
        });
        let first_worker = Worker::new("w1".into(), run.backend.clone(), vec!["model".into()])
            .register_executor("model", executor.clone());
        let second_worker = Worker::new(
            "w2".into(),
            Arc::new(Intercept::new(run.backend.clone(), hooks.clone())),
            vec!["model".into()],
        )
        .register_executor("model", executor);

        let (one, two) = tokio::join!(first_worker.poll_and_execute(), async {
            entered.notified().await;
            let (two, ()) = tokio::join!(second_worker.poll_and_execute(), async {
                hooks.waiting.notified().await;
                release.notify_one();
            });
            two
        });
        assert!(
            matches!(one, Ok(true)) && matches!(two, Ok(true)),
            "{name}: {one:?} {two:?}"
        );

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the replay called the model"
        );
        let recorded: Vec<(Option<String>, Option<f64>)> = run
            .events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::NodeCompleted {
                    idempotency_key,
                    cost_usd,
                    ..
                } => Some((idempotency_key, cost_usd)),
                _ => None,
            })
            .collect();
        assert_eq!(recorded.len(), 2, "{name}: {:?}", run.log().await);
        assert!(recorded[0].0.is_some(), "{name}: {recorded:?}");
        assert_eq!(
            recorded[1], recorded[0],
            "{name}: the replay must carry the key and the cost it was recorded with"
        );
        assert_eq!(recorded[0].1, Some(0.0123), "{name}");

        let usage = fold_run_usage(run.backend.as_ref(), &run.eid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (usage.billable_steps, usage.cost_nanos, usage.input_tokens),
            (1, 12_300_000, 4),
            "{name}"
        );
        let cap = RunCaps {
            token_budget: None,
            cost_budget_usd: Some(0.02),
        };
        assert_eq!(caps::past_cap(&cap, &usage), None, "{name}");
    }
}

// ── A spent run at claim ─────────────────────────────────────────────────────

/// A claim that names its run, on the `python_tool` queue.
async fn claim_named(run: &Run) -> (StatusCode, Value) {
    support::post(
        &run.state,
        "/work-items/claim",
        json!({
            "worker_id": "http-w",
            "queue_types": ["python_tool"],
            "execution_id": run.eid.to_string()
        }),
    )
    .await
}

/// A claim of the oldest item on the `python_tool` queue.
async fn claim_oldest(state: &jamjet_api::state::AppState) -> (StatusCode, Value) {
    support::post(
        state,
        "/work-items/claim",
        json!({ "worker_id": "http-w", "queue_types": ["python_tool"] }),
    )
    .await
}

impl Run {
    /// Each `NodeFailed` as `(node, error, retryable)`.
    async fn failures(&self) -> Vec<(String, String, bool)> {
        self.events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::NodeFailed {
                    node_id,
                    error,
                    retryable,
                    ..
                } => Some((node_id, error, retryable)),
                _ => None,
            })
            .collect()
    }

    /// `a -> b` over HTTP: `a` is claimed and completed with `report`, then
    /// the scheduler dispatches `b`.
    async fn spend_a(&self, report: Value) {
        let wi = self.claim_http().await;
        let (st, body) = self.complete_http(&wi, report).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", self.name);
        self.scheduler.tick().await.expect("scheduler tick");
        assert_eq!(self.scheduled().await, vec!["a", "b"], "{}", self.name);
    }
}

/// `a -> b`, both `python_fn`, under `total_tokens: 5`, and again under
/// `cost_budget_usd: 1.0`. A worker completes `a` with 5 tokens (or $1.00):
/// the run is at its cap, not past it, so `b` is dispatched. The claim for
/// `b` gives nothing out: a claim that names the run is told `budget`; the
/// oldest-first claim gets the answer an empty queue gets, as for every
/// other refusal. The trip names `b`, and a dollar trip is in dollars.
#[tokio::test]
async fn a_spent_run_refuses_the_next_python_fn_claim() {
    let cases = [
        (
            json!({ "token_budget": { "total_tokens": 5 } }),
            json!({ "input_tokens": 5, "output_tokens": 0 }),
            token_trip("b", 5, 5),
        ),
        (
            json!({ "cost_budget_usd": 1.0 }),
            json!({ "input_tokens": 5, "output_tokens": 0, "cost_usd": 1.0 }),
            json!({
                "type": "cost_budget_exceeded",
                "node_id": "b",
                "limit_usd": 1.0,
                "current_usd": 1.0
            }),
        ),
    ];
    for (caps, report, trip) in cases {
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir("wf-spent-named", &["a", "b"], python_fn(), caps.clone()),
            )
            .await;
            let name = run.name;
            run.spend_a(report.clone()).await;
            assert_eq!(
                claim_named(&run).await,
                (
                    StatusCode::OK,
                    json!({ "claimed": false, "reason": "budget" })
                ),
                "{name} {caps}"
            );
            assert_eq!(run.trips().await, vec![trip.clone()], "{name} {caps}");
        }
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir("wf-spent-oldest", &["a", "b"], python_fn(), caps.clone()),
            )
            .await;
            let name = run.name;
            run.spend_a(report.clone()).await;
            assert_eq!(
                claim_oldest(&run.state).await,
                (StatusCode::OK, json!({ "claimed": false })),
                "{name} {caps}"
            );
            assert_eq!(run.trips().await, vec![trip.clone()], "{name} {caps}");
        }
    }
}

/// `a -> b` under `total_tokens: 5`: `a` a plain `python_fn`, `b` an agent
/// tool dispatch node. The claim route reads the caps for a dispatch node on
/// its own path, apart from every other kind. With `a` completed at the cap,
/// the claim for `b` gives nothing out on that path too: oldest-first gets
/// `{"claimed": false}`, a claim that names the run is told `budget`, and
/// one trip names `b`.
#[tokio::test]
async fn a_spent_run_refuses_an_agent_tool_dispatch_claim() {
    let dispatch = json!({
        "type": "python_fn",
        "module": "jamjet.agents.tool_runtime",
        "function": "dispatch_tool_calls",
        "output_schema": "",
        "agent_tool_dispatch": true
    });
    let caps = json!({ "token_budget": { "total_tokens": 5 } });
    let report = json!({ "input_tokens": 5, "output_tokens": 0 });
    for named in [false, true] {
        for tb in support::backends().await {
            let mut ir = linear_ir("wf-spent-dispatch", &["a", "b"], python_fn(), caps.clone());
            ir["nodes"]["b"]["kind"] = dispatch.clone();
            let run = start(tb, ir).await;
            let name = run.name;
            run.spend_a(report.clone()).await;
            let (want, got) = if named {
                (
                    json!({ "claimed": false, "reason": "budget" }),
                    claim_named(&run).await,
                )
            } else {
                (json!({ "claimed": false }), claim_oldest(&run.state).await)
            };
            assert_eq!(got, (StatusCode::OK, want), "{name} named={named}");
            assert_eq!(
                run.trips().await,
                vec![token_trip("b", 5, 5)],
                "{name} named={named}: {:?}",
                run.log().await
            );
        }
    }
}

/// The refused step fails for good, once, on both transports: one
/// `NodeFailed { retryable: false }` naming the cap, nothing left to claim,
/// and the run fails on the next pass with `b` never run.
#[tokio::test]
async fn a_spent_refusal_fails_the_node_for_good_and_the_run_fails() {
    let caps = json!({ "token_budget": { "total_tokens": 5 } });
    let refused = vec![(
        "b".to_string(),
        "token budget reached: total_tokens 5 of 5".to_string(),
        false,
    )];
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir("wf-spent-http", &["a", "b"], python_fn(), caps.clone()),
        )
        .await;
        let name = run.name;
        run.spend_a(json!({ "input_tokens": 5, "output_tokens": 0 }))
            .await;
        let b = run.backend.open_work_items(&run.eid).await.unwrap();
        assert_eq!(b.len(), 1, "{name}: {b:?}");
        assert_eq!(
            claim_oldest(&run.state).await,
            (StatusCode::OK, json!({ "claimed": false })),
            "{name}"
        );
        assert_eq!(run.failures().await, refused, "{name}");
        assert!(
            run.tb.is_settled(b[0].id).await,
            "{name}: b is left to retry"
        );
        assert_eq!(
            claim_oldest(&run.state).await,
            (StatusCode::OK, json!({ "claimed": false })),
            "{name}"
        );
        run.scheduler.tick().await.expect("scheduler tick");
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
        assert_eq!(run.completed().await, vec!["a"], "{name}");
        assert_eq!(run.failures().await, refused, "{name}");
    }
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir("wf-spent-in-process", &["a", "b"], model(), caps.clone()),
        )
        .await;
        let name = run.name;
        let held = Held::default();
        let spender = Spender::new(5, &held);
        let worker = model_worker(run.backend.clone(), Arc::new(spender.clone()));
        run.drive_in_process(&worker, &held).await;
        assert_eq!(spender.calls(), 1, "{name}: {:?}", run.log().await);
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
        assert_eq!(run.completed().await, vec!["a"], "{name}");
        assert_eq!(run.failures().await, refused, "{name}");
        assert_eq!(run.trips().await, vec![token_trip("b", 5, 5)], "{name}");
    }
}

/// Fails every `get_events` of `run` once `on` is set.
struct FailEvents {
    run: ExecutionId,
    on: AtomicBool,
}

#[async_trait::async_trait]
impl Hooks for FailEvents {
    async fn before(&self, op: &Op<'_>) -> Option<StateBackendError> {
        match op {
            Op::GetEvents(id) if **id == self.run && self.on.load(Ordering::SeqCst) => Some(
                StateBackendError::Database("the log could not be read (test)".into()),
            ),
            _ => None,
        }
    }
}

/// Under a cap, a step fires only once the run's usage is known. When the
/// log cannot be read, or holds an event this build cannot read, nothing
/// fires and nothing is failed or recorded: the claim gets nothing (over
/// HTTP the item stays leased until its lease runs out; in process the
/// attempt fails and is retried), and the step decides again later.
#[tokio::test]
async fn an_unreadable_usage_fold_holds_the_step() {
    let caps = json!({ "token_budget": { "total_tokens": 5 } });
    for unknown_event in [true, false] {
        let case = if unknown_event {
            "unknown event"
        } else {
            "read error"
        };
        // Over HTTP.
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir("wf-unreadable-http", &["a"], python_fn(), caps.clone()),
            )
            .await;
            let name = run.name;
            run.scheduler.tick().await.expect("scheduler tick");
            let hooks = Arc::new(FailEvents {
                run: run.eid.clone(),
                on: AtomicBool::new(!unknown_event),
            });
            if unknown_event {
                run.backend
                    .append_event(jamjet_state::Event::new(
                        run.eid.clone(),
                        0,
                        EventKind::Unknown,
                    ))
                    .await
                    .unwrap();
            }
            let state =
                support::make_state(Arc::new(Intercept::new(run.backend.clone(), hooks.clone())));
            assert_eq!(
                claim_oldest(&state).await,
                (StatusCode::OK, json!({ "claimed": false })),
                "{name}: {case}"
            );
            hooks.on.store(false, Ordering::SeqCst);
            assert_eq!(run.settled().await, Vec::<String>::new(), "{name}: {case}");
            assert_eq!(run.trips().await, Vec::<Value>::new(), "{name}: {case}");
            let open = run.backend.open_work_items(&run.eid).await.unwrap();
            assert!(
                open.len() == 1 && open[0].claimed,
                "{name}: {case}: the item stays leased: {open:?}"
            );
        }
        // In process.
        for tb in support::backends().await {
            let run = start(
                tb,
                linear_ir("wf-unreadable-in-process", &["a"], model(), caps.clone()),
            )
            .await;
            let name = run.name;
            run.scheduler.tick().await.expect("scheduler tick");
            let hooks = Arc::new(FailEvents {
                run: run.eid.clone(),
                on: AtomicBool::new(!unknown_event),
            });
            if unknown_event {
                run.backend
                    .append_event(jamjet_state::Event::new(
                        run.eid.clone(),
                        0,
                        EventKind::Unknown,
                    ))
                    .await
                    .unwrap();
            }
            let held = Held::default();
            let spender = Spender::new(1, &held);
            let worker = model_worker(
                Arc::new(Intercept::new(run.backend.clone(), hooks.clone())),
                Arc::new(spender.clone()),
            );
            let answer = worker.poll_and_execute().await;
            hooks.on.store(false, Ordering::SeqCst);
            assert!(answer.is_err(), "{name}: {case}: {answer:?}");
            assert_eq!(spender.calls(), 0, "{name}: {case}");
            assert_eq!(run.settled().await, Vec::<String>::new(), "{name}: {case}");
            assert_eq!(run.trips().await, Vec::<Value>::new(), "{name}: {case}");
        }
    }
}

/// `a -> b` under `total_tokens: 1` with `continue_as_new` set, in process.
/// `a` reports 2 tokens. A budget trip used to roll such a run over to a
/// new segment, which inherited the spend and paid for one more call per
/// segment. The run must fail instead: `a`'s spend is kept, `b` never runs,
/// and no second segment exists.
#[tokio::test]
async fn a_budget_trip_fails_the_run_and_never_rolls_over() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir(
                "wf-no-rollover",
                &["a", "b"],
                model(),
                json!({ "token_budget": { "total_tokens": 1 }, "continue_as_new": true }),
            ),
        )
        .await;
        let name = run.name;
        let held = Held::default();
        let spender = Spender::new(2, &held);
        let worker = model_worker(run.backend.clone(), Arc::new(spender.clone()));
        run.drive_in_process(&worker, &held).await;

        assert_eq!(
            run.status().await,
            WorkflowStatus::Failed,
            "{name}: {:?}",
            run.log().await
        );
        assert_eq!(spender.calls(), 1, "{name}");
        assert_eq!(run.completed().await, vec!["a"], "{name}");
        assert!(
            !run.events()
                .await
                .iter()
                .any(|k| matches!(k, EventKind::SegmentBoundary { .. })),
            "{name}: the run rolled over: {:?}",
            run.log().await
        );
        let next = jamjet_state::segment::segment_execution_id(&run.eid, 1);
        assert!(
            run.backend.get_execution(&next).await.unwrap().is_none(),
            "{name}: a second segment was started"
        );
    }
}

/// `GET /executions/:id` quotes what the run has spent, counted from its log
/// by the same fold the caps use. Three steps: one reports tokens and a cost,
/// one tokens only (its cost is unknown), one nothing (not a billable step).
/// The answer carries these eight keys, and `usage` is null when the log
/// holds an event this build cannot read, so a reader never takes a partial
/// count for the whole.
#[tokio::test]
async fn usage_on_the_run_matches_the_fold() {
    for tb in support::backends().await {
        let run = start(
            tb,
            linear_ir("wf-usage", &["a", "b", "c"], python_fn(), json!({})),
        )
        .await;
        let name = run.name;
        let (_, before) = support::get(&run.state, &format!("/executions/{}", run.eid)).await;
        assert_eq!(
            before["usage"],
            json!({
                "input_tokens": 0, "output_tokens": 0, "cost_usd": 0.0,
                "billable_steps": 0, "cost_unknown_steps": 0, "tokens_unknown_steps": 0,
                "input_tokens_unknown_steps": 0, "output_tokens_unknown_steps": 0
            }),
            "{name}: a run that has spent nothing"
        );
        for report in [
            json!({ "gen_ai_model": "m", "input_tokens": 120, "output_tokens": 30, "cost_usd": 0.25 }),
            json!({ "gen_ai_model": "m", "input_tokens": 10, "output_tokens": 5 }),
            json!({}),
        ] {
            let wi = run.claim_http().await;
            let (st, body) = run.complete_http(&wi, report).await;
            assert_eq!(st, StatusCode::OK, "{name}: {body}");
        }
        let (st, got) = support::get(&run.state, &format!("/executions/{}", run.eid)).await;
        assert_eq!(st, StatusCode::OK, "{name}");
        assert_eq!(
            got["usage"],
            json!({
                "input_tokens": 130, "output_tokens": 35, "cost_usd": 0.25,
                "billable_steps": 2, "cost_unknown_steps": 1, "tokens_unknown_steps": 0,
                "input_tokens_unknown_steps": 0, "output_tokens_unknown_steps": 0
            }),
            "{name}"
        );
        let fold = fold_run_usage(run.backend.as_ref(), &run.eid)
            .await
            .unwrap()
            .expect("readable");
        assert_eq!(
            got["usage"]["input_tokens"],
            json!(fold.input_tokens),
            "{name}"
        );
        assert_eq!(
            got["usage"]["output_tokens"],
            json!(fold.output_tokens),
            "{name}"
        );
        assert_eq!(got["usage"]["cost_usd"], json!(fold.cost_usd()), "{name}");
        assert_eq!(
            got["usage"]["billable_steps"],
            json!(fold.billable_steps),
            "{name}"
        );

        run.backend
            .append_event(jamjet_state::Event::new(
                run.eid.clone(),
                0,
                EventKind::Unknown,
            ))
            .await
            .unwrap();
        let (_, unread) = support::get(&run.state, &format!("/executions/{}", run.eid)).await;
        assert!(
            unread.as_object().unwrap().contains_key("usage") && unread["usage"].is_null(),
            "{name}: an unreadable log reads null, not a partial count: {unread}"
        );
    }
}
