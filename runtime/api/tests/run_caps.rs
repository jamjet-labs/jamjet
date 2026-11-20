//! A run's token cap and dollar cap, on both transports.
//!
//! A workflow caps its runs with `token_budget` and `cost_budget_usd`. Until
//! 2026-11 only the in-process worker read them, from the `__budget` key it
//! kept in the run's state, and it checked them after the model call: a trip
//! there threw the paid result away and left the item leased, so the retry
//! paid for the call again, and a failed log read in that check let the turn
//! through. An HTTP worker's reported tokens and cost were stored and never
//! checked. Each test here pins what must hold instead, and the last one
//! that both transports stop a run at the same step for the same spend.
//!
//! Runs are driven by hand, on memory and on SQLite: a scheduler tick, then
//! one worker poll (in process) or one claim and completion (HTTP), until the
//! run ends. An item the in-process worker leaves claimed is let go the way
//! time lets it go: its lease runs out and so does the effect reservation its
//! attempt holds, so the next claim takes it back as a retry.

mod support;

use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_models::{
    ModelAdapter, ModelError, ModelRegistry, ModelRequest, ModelResponse, StructuredRequest,
};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, StateBackendError, WorkItem};
use jamjet_state::event::EventKind;
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
