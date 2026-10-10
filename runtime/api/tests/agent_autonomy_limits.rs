//! An agent card's autonomy limits are enforced on every node acting for
//! the agent, at the shared guard, from the run's durable event log.
//!
//! Before this, `max_iterations` and `max_tool_calls` were compared with
//! counters nothing incremented, `time_budget_secs` was never read, the
//! circuit breaker never tripped, and the token and cost budgets were checked
//! only for a `bounded_autonomous` card on an `agent` node in the in-process
//! worker. Every test here drives both transports (the HTTP claim route and
//! the in-process worker) on both backends (memory and SQLite).

use async_trait::async_trait;
use jamjet_agents::{InMemoryAgentRegistry, SqliteAgentRegistry};
use jamjet_api::state::AppState;
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_worker::executor::{ExecutionResult, ExecutorError, NodeExecutor};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod support;

use support::hooked::{AfterWrite, HookedBackend, Write};

const AGENT: &str = "jamjet://acme/researcher";
const QUEUES: [&str; 4] = ["python_tool", "tool", "general", "model"];

fn card(constraints: Value) -> Value {
    json!({
        "id": AGENT,
        "uri": AGENT,
        "name": "researcher",
        "description": "research agent",
        "version": "1.0.0",
        "capabilities": { "skills": [], "protocols": [], "tools_provided": [], "tools_consumed": [] },
        // The limits apply at every level; this one used to switch them off.
        "autonomy": "fully_autonomous",
        "constraints": constraints,
        "auth": { "type": "none" }
    })
}

fn mcp_tool() -> Value {
    json!({ "type": "mcp_tool", "server": "srv", "tool": "search_web", "input_mapping": {}, "output_schema": "" })
}

fn model_node() -> Value {
    json!({ "type": "model", "model_ref": "fake", "prompt_ref": "", "output_schema": "" })
}

fn adk_dispatch() -> Value {
    json!({
        "type": "python_fn",
        "module": "jamjet.agents.tool_runtime",
        "function": "dispatch_tool_calls",
        "output_schema": "",
        "agent_tool_dispatch": true
    })
}

fn calls(n: usize) -> Value {
    let tool_calls: Vec<Value> = (0..n)
        .map(|i| json!({ "id": format!("c{i}"), "name": "search_web", "arguments": { "i": i } }))
        .collect();
    json!({ "tool_calls": tool_calls, "tools": {} })
}

/// A one-node workflow. `workflow_agent` / `node_agent` attribute it.
fn ir(
    workflow_id: &str,
    kind: Value,
    workflow_agent: Option<&str>,
    node_agent: Option<&str>,
    policy: Option<Value>,
) -> Value {
    let mut node = json!({
        "id": "n1",
        "kind": kind,
        "retry_policy": null,
        "node_timeout_secs": null,
        "description": null,
        "labels": {}
    });
    if let Some(a) = node_agent {
        node["agent_ref"] = json!(a);
    }
    let mut ir = json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "n1",
        "nodes": { "n1": node },
        "edges": [ { "from": "n1", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    });
    if let Some(a) = workflow_agent {
        ir["agent_ref"] = json!(a);
    }
    if let Some(p) = policy {
        ir["policy"] = p;
    }
    ir
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Transport {
    Claim,
    Worker,
}

const TRANSPORTS: [Transport; 2] = [Transport::Claim, Transport::Worker];

/// How the step's work ends.
#[derive(Clone, Copy, Debug)]
enum Act {
    /// Completes, reporting these tokens and (over HTTP only) this cost.
    Done { tokens: u64, cost: Option<f64> },
    /// Fails.
    Fail,
}

const DONE: Act = Act::Done {
    tokens: 0,
    cost: None,
};

/// Runs the node as told, counting runs.
#[derive(Clone)]
struct Scripted {
    act: Act,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl NodeExecutor for Scripted {
    async fn execute(&self, _item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        match self.act {
            Act::Done { tokens, .. } => Ok(ExecutionResult {
                output: json!({}),
                state_patch: json!({}),
                duration_ms: 0,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: Some(tokens),
                output_tokens: Some(0),
                finish_reason: None,
            }),
            Act::Fail => Err(ExecutorError::Fatal("tool down".into())),
        }
    }
}

/// One started run and what is needed to give its node more work.
struct Run {
    state: AppState,
    backend: Arc<dyn StateBackend>,
    eid: ExecutionId,
    ir: Value,
    wf: String,
    input: Value,
    /// The start item has been taken; later steps enqueue their own.
    started: bool,
}

/// A state over `backend`, with an agent registry of `tb`'s kind.
async fn state_for(tb: &support::TestBackend, backend: Arc<dyn StateBackend>) -> AppState {
    let mut state = support::make_state(backend);
    if let support::TestBackend::Sqlite { url, .. } = tb {
        state.agents = Arc::new(SqliteAgentRegistry::connect(url).await.unwrap());
    } else {
        state.agents = Arc::new(InMemoryAgentRegistry::new());
    }
    state
}

async fn register(state: &AppState, cards: &[Value]) {
    for c in cards {
        let (st, body) = support::post(state, "/agents", c.clone()).await;
        assert!(st.is_success(), "register agent: {st} {body}");
    }
}

async fn start_on(
    tb: &support::TestBackend,
    backend: Arc<dyn StateBackend>,
    cards: &[Value],
    ir: Value,
    input: Value,
) -> Run {
    let state = state_for(tb, backend.clone()).await;
    register(&state, cards).await;
    let wf = ir["workflow_id"].as_str().unwrap().to_string();
    let eid = support::register_and_start(&state, ir.clone(), input.clone()).await;
    let eid =
        ExecutionId(uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(&eid)).unwrap());
    Run {
        state,
        backend,
        eid,
        ir,
        wf,
        input,
        started: false,
    }
}

async fn start(tb: &support::TestBackend, cards: &[Value], ir: Value, input: Value) -> Run {
    start_on(tb, tb.dyn_backend(), cards, ir, input).await
}

impl Run {
    fn eid_path(&self) -> String {
        format!("exec_{}", self.eid.0)
    }

    /// Give the node one more item, as the scheduler would on a loop.
    async fn enqueue(&self) {
        let ir_t: jamjet_ir::WorkflowIr = serde_json::from_value(self.ir.clone()).unwrap();
        let payload = jamjet_scheduler::build_work_item_payload(
            &ir_t,
            "n1",
            ir_t.node("n1").unwrap(),
            self.input.as_object().unwrap(),
            &self.wf,
            "0.1.0",
        );
        let queue = ir_t
            .node("n1")
            .unwrap()
            .kind
            .queue_type()
            .as_str()
            .to_string();
        let (st, body) = support::post(
            &self.state,
            "/work-items",
            json!({ "execution_id": self.eid_path(), "node_id": "n1", "queue_type": queue, "payload": payload }),
        )
        .await;
        assert!(st.is_success(), "enqueue: {st} {body}");
    }

    /// One step of the node over `t`: true if its work was handed out
    /// (claim) or executed (worker).
    async fn step(&mut self, t: Transport, act: Act) -> bool {
        if self.started {
            self.enqueue().await;
        }
        self.started = true;
        match t {
            Transport::Claim => {
                let (_, claimed) = support::post(
                    &self.state,
                    "/work-items/claim",
                    json!({ "worker_id": "w", "queue_types": QUEUES }),
                )
                .await;
                if claimed["claimed"] != json!(true) {
                    return false;
                }
                let wi = &claimed["work_item"];
                let id = wi["id"].as_str().unwrap();
                let fence = wi["lease_fence"].clone();
                match act {
                    Act::Done { tokens, cost } => {
                        let mut body = json!({
                            "output": {}, "state_patch": {}, "lease_fence": fence,
                            "input_tokens": tokens, "output_tokens": 0
                        });
                        if let Some(c) = cost {
                            body["cost_usd"] = json!(c);
                        }
                        let (st, body) =
                            support::post(&self.state, &format!("/work-items/{id}/complete"), body)
                                .await;
                        assert!(st.is_success(), "complete: {st} {body}");
                        assert_eq!(body["completed"], json!(true), "{body}");
                    }
                    Act::Fail => {
                        let (st, body) = support::post(
                            &self.state,
                            &format!("/work-items/{id}/fail"),
                            json!({ "error": "tool down", "lease_fence": fence }),
                        )
                        .await;
                        assert!(st.is_success(), "fail: {st} {body}");
                        // The engine's retry of a failed item: the test gives
                        // the node its next item itself, so drop the retry.
                        self.drop_pending().await;
                    }
                }
                true
            }
            Transport::Worker => {
                let exec = Scripted {
                    act,
                    runs: Arc::new(AtomicUsize::new(0)),
                };
                let worker = jamjet_worker::Worker::new(
                    "w".into(),
                    self.backend.clone(),
                    QUEUES.iter().map(|q| q.to_string()).collect(),
                )
                .with_agents(self.state.agents.clone())
                .register_executor("mcp_tool", Arc::new(exec.clone()))
                .register_executor("python_fn", Arc::new(exec.clone()))
                .register_executor("model", Arc::new(exec.clone()))
                .register_executor("agent", Arc::new(exec.clone()));
                let _ = worker.poll_and_execute().await;
                exec.runs.load(Ordering::SeqCst) > 0
            }
        }
    }

    /// Settle every pending item of the run (the retry a failed item left).
    async fn drop_pending(&self) {
        loop {
            let Some(wi) = self
                .backend
                .claim_work_item_with_lease("sweeper", &QUEUES, Some(30))
                .await
                .unwrap()
            else {
                return;
            };
            self.backend
                .complete_work_item_fenced(wi.id, wi.lease_fence)
                .await
                .unwrap();
        }
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

    /// `(limit_type, limit_value, actual_value)` of every `AutonomyLimitReached`.
    async fn limits(&self) -> Vec<(String, Value, Value)> {
        self.events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::AutonomyLimitReached {
                    limit_type,
                    limit_value,
                    actual_value,
                    agent_ref,
                    ..
                } => {
                    assert_eq!(agent_ref, AGENT);
                    Some((limit_type, limit_value, actual_value))
                }
                _ => None,
            })
            .collect()
    }

    async fn admitted(&self) -> usize {
        self.events()
            .await
            .iter()
            .filter(|k| matches!(k, EventKind::AgentStepAdmitted { .. }))
            .count()
    }

    /// The last `NodeFailed`'s error, if it failed for good.
    async fn failed_for_good(&self) -> Option<String> {
        self.events().await.into_iter().rev().find_map(|k| match k {
            EventKind::NodeFailed {
                error,
                retryable: false,
                ..
            } => Some(error),
            _ => None,
        })
    }
}

/// Every backend and transport, each with a fresh pair of backends: the
/// SQLite one holds the registered cards, and URIs are unique.
async fn each() -> Vec<(String, support::TestBackend, Transport, String)> {
    let mut out = Vec::new();
    for t in TRANSPORTS {
        for tb in support::backends().await {
            let ctx = format!("{} over {t:?}", tb.name());
            let wf = format!("wf-{}-{t:?}", tb.name()).to_lowercase();
            out.push((ctx, tb, t, wf));
        }
    }
    out
}

/// The node was refused by `limit`, recorded, and failed for good.
async fn assert_limit(run: &Run, ran: bool, limit: &str, value: Value, actual: Value, ctx: &str) {
    assert!(!ran, "{ctx}: the step past the limit ran");
    let limits = run.limits().await;
    assert!(
        limits.contains(&(limit.to_string(), value.clone(), actual.clone())),
        "{ctx}: no {limit} = {value} (actual {actual}) refusal: {limits:?}"
    );
    let error = run.failed_for_good().await;
    assert!(
        error.as_deref().is_some_and(|e| e.contains(limit)),
        "{ctx}: the node did not fail for good on {limit}: {error:?}"
    );
}

// ── max_iterations ──────────────────────────────────────────────────────────

#[tokio::test]
async fn iterations_trip_at_the_boundary() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_iterations": 3 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        for i in 1..=3 {
            assert!(run.step(t, DONE).await, "{ctx}: step {i} of 3 did not run");
        }
        assert!(run.limits().await.is_empty(), "{ctx}");
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_iterations", json!(3), json!(3), &ctx).await;
        assert_eq!(run.admitted().await, 3, "{ctx}");
    }
}

/// A node's own `acts_for` (`agent_ref` on the node) attributes it too.
#[tokio::test]
async fn a_node_acting_for_the_agent_is_counted() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_iterations": 1 }))],
            ir(&wf, mcp_tool(), None, Some(AGENT), None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_iterations", json!(1), json!(1), &ctx).await;
    }
}

fn agent_node(target: &str) -> Value {
    json!({ "type": "agent", "agent_ref": target, "input_mapping": {}, "output_schema": "" })
}

/// An `agent` node is a step of the agent it calls, attributed or not.
#[tokio::test]
async fn an_agent_node_charges_the_agent_it_calls() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_iterations": 1 }))],
            ir(&wf, agent_node(AGENT), None, None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_iterations", json!(1), json!(1), &ctx).await;
    }
}

/// A callee with no registered card has no limits to apply.
#[tokio::test]
async fn an_agent_node_calling_an_unregistered_agent_runs() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[],
            ir(&wf, agent_node("jamjet://acme/nobody"), None, None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}");
        assert_eq!(run.admitted().await, 0, "{ctx}");
    }
}

// ── max_tool_calls ──────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_calls_trip_at_the_boundary() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_tool_calls": 2 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}");
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_tool_calls", json!(2), json!(3), &ctx).await;
    }
}

/// An ADK dispatch batch counts each of its calls.
#[tokio::test]
async fn a_dispatch_batch_counts_each_call() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_tool_calls": 4 }))],
            ir(&wf, adk_dispatch(), Some(AGENT), None, None),
            calls(2),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}: 2 of 4");
        assert!(run.step(t, DONE).await, "{ctx}: 4 of 4");
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_tool_calls", json!(4), json!(6), &ctx).await;
    }
}

/// A model step calls no tool, so a spent tool call budget does not stop it.
#[tokio::test]
async fn a_model_step_is_not_a_tool_call() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_tool_calls": 0 }))],
            ir(&wf, model_node(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}");
        assert_eq!(run.admitted().await, 2, "{ctx}");
    }
}

// ── token_budget and cost_budget_usd ────────────────────────────────────────

#[tokio::test]
async fn the_token_budget_trips_once_reached() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "token_budget": 100 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        let spend = |tokens| Act::Done { tokens, cost: None };
        assert!(run.step(t, spend(60)).await, "{ctx}");
        assert!(run.step(t, spend(39)).await, "{ctx}: 60 of 100");
        // 99 of 100: one more step may run.
        assert!(run.step(t, spend(1)).await, "{ctx}: 99 of 100");
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "token_budget", json!(100), json!(100), &ctx).await;
    }
}

/// Cost is recorded only when a worker reports it on an HTTP completion; it
/// is enforced on both transports.
#[tokio::test]
async fn the_cost_budget_trips_once_reached() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "cost_budget_usd": 0.5 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        let spend = |cost| Act::Done {
            tokens: 0,
            cost: Some(cost),
        };
        // Binary-exact amounts, so the boundary is exact.
        assert!(run.step(Transport::Claim, spend(0.25)).await, "{ctx}");
        assert!(run.step(Transport::Claim, spend(0.125)).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}: 0.375 of 0.5");
        assert!(
            run.step(Transport::Claim, spend(0.125)).await,
            "{ctx}: 0.375 of 0.5"
        );
        let ran = run.step(t, DONE).await;
        assert!(!ran, "{ctx}: ran past the cost budget");
        let limits = run.limits().await;
        assert_eq!(limits.len(), 1, "{ctx}: {limits:?}");
        assert_eq!(limits[0].0, "cost_budget", "{ctx}");
        assert_eq!(limits[0].1, json!(0.5), "{ctx}");
        assert_eq!(limits[0].2, json!(0.5), "{ctx}");
    }
}

/// A reported cost is a sum the budget reads: a negative one would refund
/// it.
#[tokio::test]
async fn a_negative_cost_is_refused() {
    for tb in support::backends().await {
        let mut run = start(
            &tb,
            &[card(json!({ "cost_budget_usd": 0.5 }))],
            ir("wf-neg", mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        run.started = true;
        let (_, claimed) = support::post(
            &run.state,
            "/work-items/claim",
            json!({ "worker_id": "w", "queue_types": QUEUES }),
        )
        .await;
        let wi = &claimed["work_item"];
        let (st, body) = support::post(
            &run.state,
            &format!("/work-items/{}/complete", wi["id"].as_str().unwrap()),
            json!({ "output": {}, "state_patch": {}, "lease_fence": wi["lease_fence"], "cost_usd": -1.0 }),
        )
        .await;
        assert_eq!(
            st,
            axum::http::StatusCode::BAD_REQUEST,
            "{}: {body}",
            tb.name()
        );
        assert!(
            !run.events()
                .await
                .iter()
                .any(|k| matches!(k, EventKind::NodeCompleted { .. })),
            "{}",
            tb.name()
        );
    }
}

// ── time_budget_secs ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_time_budget_runs_from_the_first_step() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "time_budget_secs": 1 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        // The clock runs from the FIRST step, not the latest: 600 ms after
        // the first step is inside the budget, 600 ms after that is not,
        // though it is only 600 ms after the step before it.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(run.step(t, DONE).await, "{ctx}: inside the budget");
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let ran = run.step(t, DONE).await;
        assert!(!ran, "{ctx}: ran past the time budget");
        let limits = run.limits().await;
        assert_eq!(limits.len(), 1, "{ctx}: {limits:?}");
        assert_eq!(limits[0].0, "time_budget", "{ctx}");
        assert_eq!(limits[0].1, json!(1), "{ctx}");
        assert!(run.failed_for_good().await.is_some(), "{ctx}");
    }
}

// ── circuit breaker ─────────────────────────────────────────────────────────

fn tripped(events: &[EventKind]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|k| match k {
            EventKind::CircuitBreakerTripped {
                consecutive_errors,
                threshold,
                agent_ref,
                ..
            } => {
                assert_eq!(agent_ref, AGENT);
                Some((*consecutive_errors, *threshold))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn consecutive_failures_trip_the_breaker() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_consecutive_errors": 2 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, Act::Fail).await, "{ctx}");
        assert!(run.step(t, Act::Fail).await, "{ctx}: 1 of 2 errors");
        let ran = run.step(t, DONE).await;
        assert!(!ran, "{ctx}: ran with the breaker tripped");
        assert_eq!(tripped(&run.events().await), vec![(2, 2)], "{ctx}");
        assert!(
            run.failed_for_good()
                .await
                .is_some_and(|e| e.contains("circuit breaker")),
            "{ctx}"
        );
    }
}

/// A completed step resets the count: errors must be consecutive.
#[tokio::test]
async fn a_completed_step_resets_the_breaker() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_consecutive_errors": 2 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, Act::Fail).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(run.step(t, Act::Fail).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}: one error since the reset");
        assert!(tripped(&run.events().await).is_empty(), "{ctx}");
    }
}

/// With no threshold on the card, the breaker trips at three.
#[tokio::test]
async fn the_breaker_defaults_to_three() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({}))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        for i in 1..=3 {
            assert!(run.step(t, Act::Fail).await, "{ctx}: failure {i}");
        }
        assert!(!run.step(t, DONE).await, "{ctx}");
        assert_eq!(tripped(&run.events().await), vec![(3, 3)], "{ctx}");
    }
}

// ── durability ──────────────────────────────────────────────────────────────

/// The counters are folded from the event log, so a new engine over the
/// same store (a restart) still refuses.
#[tokio::test]
async fn the_counters_survive_a_restart() {
    for (ctx, tb, t, wf) in each().await {
        let c = card(json!({ "max_iterations": 2 }));
        let mut run = start(
            &tb,
            std::slice::from_ref(&c),
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(run.step(t, DONE).await, "{ctx}");

        // A new engine: a new state and worker over the same store. SQLite
        // is reopened from its file; memory keeps its backend (it is the
        // store) and gets a new registry, so the card is registered again.
        let backend: Arc<dyn StateBackend> = match &tb {
            support::TestBackend::Sqlite { url, .. } => {
                Arc::new(jamjet_state::SqliteBackend::open(url).await.unwrap())
            }
            support::TestBackend::Memory(b) => b.clone(),
        };
        let state = state_for(&tb, backend.clone()).await;
        if matches!(tb, support::TestBackend::Memory(_)) {
            register(&state, std::slice::from_ref(&c)).await;
        }
        run.state = state;
        run.backend = backend;
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_iterations", json!(2), json!(2), &ctx).await;
    }
}

/// A continue-as-new segment is the same run: it does not start the
/// agent's counters again.
#[tokio::test]
async fn a_new_segment_keeps_the_counters() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_iterations": 1 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");

        let carried = jamjet_state::materializer::materialize(run.backend.as_ref(), &run.eid)
            .await
            .unwrap();
        let next = jamjet_state::segment::start_next_segment(
            run.backend.as_ref(),
            &run.eid,
            &carried,
            &wf,
            "0.1.0",
            1,
            "n1",
            "tool",
            "default",
        )
        .await
        .unwrap();
        // The parent is closed, as a rollover leaves it.
        run.backend
            .update_execution_status(&run.eid, jamjet_core::workflow::WorkflowStatus::Completed)
            .await
            .unwrap();
        run.eid = next;
        let ran = run.step(t, DONE).await;
        assert_limit(&run, ran, "max_iterations", json!(1), json!(1), &ctx).await;
    }
}

// ── escalation ──────────────────────────────────────────────────────────────

/// `on_limit_exceeded: escalate_to_human` holds the step for an ordinary
/// approval instead of failing it, and an approval releases exactly that
/// step.
#[tokio::test]
async fn escalate_routes_the_step_to_an_approval() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(
                json!({ "max_iterations": 1, "on_limit_exceeded": "escalate_to_human" }),
            )],
            ir(
                &wf,
                mcp_tool(),
                Some(AGENT),
                None,
                Some(json!({ "approval_timeout_secs": 600 })),
            ),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(
            !run.step(t, DONE).await,
            "{ctx}: ran past the limit unapproved"
        );

        let events = run.events().await;
        let requests: Vec<&Value> = events
            .iter()
            .filter_map(|k| match k {
                EventKind::ToolApprovalRequired { context, .. } => Some(context),
                _ => None,
            })
            .collect();
        assert_eq!(requests.len(), 1, "{ctx}: {events:?}");
        let request = requests[0].clone();
        assert_eq!(
            request["autonomy_limit"]["limit_type"],
            json!("max_iterations"),
            "{ctx}: {request}"
        );
        assert_eq!(
            request["autonomy_limit"]["agent_ref"],
            json!(AGENT),
            "{ctx}"
        );
        // The same binding and deadline as any other request.
        assert!(request["action_digest"].is_string(), "{ctx}: {request}");
        assert!(request["expires_at"].is_string(), "{ctx}: {request}");
        assert!(
            events.iter().any(|k| matches!(
                k,
                EventKind::EscalationRequired { escalation_target, .. } if escalation_target == "human_approval"
            )),
            "{ctx}"
        );
        // A duplicate item while the request is pending is held without a
        // second record of the stop.
        assert!(!run.step(t, DONE).await, "{ctx}");
        assert_eq!(run.limits().await.len(), 1, "{ctx}");
        assert_eq!(
            run.events()
                .await
                .iter()
                .filter(|k| matches!(k, EventKind::EscalationRequired { .. }))
                .count(),
            1,
            "{ctx}"
        );
        assert!(
            run.failed_for_good().await.is_none(),
            "{ctx}: failed instead of escalating"
        );

        let (st, body) = support::post(
            &run.state,
            &format!("/executions/{}/approve", run.eid_path()),
            json!({ "decision": "approved", "node_id": "n1", "action_digest": request["action_digest"] }),
        )
        .await;
        assert!(st.is_success(), "{ctx}: approve: {st} {body}");
        assert!(
            run.step(t, DONE).await,
            "{ctx}: the approved step did not run"
        );

        // The next step past the limit needs its own approval.
        assert!(!run.step(t, DONE).await, "{ctx}");
        let requests = run
            .events()
            .await
            .into_iter()
            .filter(|k| matches!(k, EventKind::ToolApprovalRequired { .. }))
            .count();
        assert_eq!(requests, 2, "{ctx}");
    }
}

/// RFC-005's `escalate_to_supervisor` has no implementation: a card asking
/// for it is refused rather than treated as `fail`.
#[tokio::test]
async fn a_card_asking_for_a_supervisor_is_refused() {
    for tb in support::backends().await {
        let state = state_for(&tb, tb.dyn_backend()).await;
        let (st, body) = support::post(
            &state,
            "/agents",
            card(json!({ "max_iterations": 1, "on_limit_exceeded": "escalate_to_supervisor" })),
        )
        .await;
        assert!(!st.is_success(), "{}: {st} {body}", tb.name());
    }
}

/// When the node acts for two agents and one card escalates while the other
/// fails, the node is refused: a refusal is the stricter answer.
#[tokio::test]
async fn a_failing_card_wins_over_an_escalating_one() {
    const OTHER: &str = "jamjet://acme/reviewer";
    for (ctx, tb, t, wf) in each().await {
        let mut other = card(json!({ "max_iterations": 1 }));
        other["id"] = json!(OTHER);
        other["uri"] = json!(OTHER);
        let mut run = start(
            &tb,
            &[
                card(json!({ "max_iterations": 1, "on_limit_exceeded": "escalate_to_human" })),
                other,
            ],
            ir(&wf, mcp_tool(), Some(AGENT), Some(OTHER), None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(!run.step(t, DONE).await, "{ctx}");
        let events = run.events().await;
        assert!(
            !events
                .iter()
                .any(|k| matches!(k, EventKind::ToolApprovalRequired { .. })),
            "{ctx}: escalated although one card fails"
        );
        assert!(run.failed_for_good().await.is_some(), "{ctx}");
    }
}

/// A log holding an event this build cannot read may hold a step: the
/// counts cannot be known, so the step is withheld.
#[tokio::test]
async fn an_unreadable_log_withholds_the_step() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_iterations": 5 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        run.backend
            .append_event(jamjet_state::Event::new(
                run.eid.clone(),
                0,
                EventKind::Unknown,
            ))
            .await
            .unwrap();
        assert!(!run.step(t, DONE).await, "{ctx}: ran on an unreadable log");
        assert_eq!(run.admitted().await, 0, "{ctx}");
    }
}

// ── unaffected runs ─────────────────────────────────────────────────────────

/// A run whose nodes act for no agent is not counted or limited.
#[tokio::test]
async fn a_run_without_a_card_is_unaffected() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({ "max_iterations": 0, "max_tool_calls": 0 }))],
            ir(&wf, mcp_tool(), None, None, None),
            json!({}),
        )
        .await;
        for i in 1..=4 {
            assert!(run.step(t, Act::Fail).await, "{ctx}: step {i}");
        }
        assert!(run.step(t, DONE).await, "{ctx}");
        assert_eq!(run.admitted().await, 0, "{ctx}");
        assert!(run.limits().await.is_empty(), "{ctx}");
    }
}

// ── the completion gate ─────────────────────────────────────────────────────

/// A result over HTTP is recorded only for an item the guard admitted: an
/// item completed without a claim would be a step no limit saw.
#[tokio::test]
async fn a_result_for_an_unadmitted_item_is_refused() {
    for tb in support::backends().await {
        let run = start(
            &tb,
            &[card(json!({ "max_iterations": 5 }))],
            ir("wf-unadmitted", mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        // The start item, never claimed.
        let items = run
            .backend
            .claim_work_item_with_lease("peek", &QUEUES, Some(30))
            .await
            .unwrap()
            .expect("start item");
        // Hand it back unclaimed in the engine's eyes: complete it over
        // HTTP without the claim route.
        let (st, body) = support::post(
            &run.state,
            &format!("/work-items/{}/complete", items.id),
            json!({ "output": {}, "state_patch": {}, "lease_fence": items.lease_fence }),
        )
        .await;
        assert_eq!(
            st,
            axum::http::StatusCode::CONFLICT,
            "{}: {body}",
            tb.name()
        );
        assert!(
            !run.events()
                .await
                .iter()
                .any(|k| matches!(k, EventKind::NodeCompleted { .. })),
            "{}",
            tb.name()
        );
    }
}

// ── concurrency ─────────────────────────────────────────────────────────────

/// Another claim admits a step between this claim's check and its record.
struct RaceAdmission {
    inner: Arc<dyn StateBackend>,
    armed: AtomicBool,
    eid: Mutex<Option<ExecutionId>>,
    /// The agents the racing step charges.
    agents: Vec<String>,
    /// The approval the racing step used, if any.
    approval_sequence: Mutex<Option<i64>>,
}

#[async_trait]
impl AfterWrite for RaceAdmission {
    async fn after(&self, write: Write) {
        if let Write::BeforeAppendEventIf(eid) = write {
            if Some(&eid) == self.eid.lock().unwrap().as_ref()
                && self.armed.swap(false, Ordering::SeqCst)
            {
                let approval_sequence = *self.approval_sequence.lock().unwrap();
                self.inner
                    .append_event(jamjet_state::Event::new(
                        eid,
                        0,
                        EventKind::AgentStepAdmitted {
                            node_id: "other".into(),
                            work_item_id: None,
                            agents: self.agents.clone(),
                            tool_calls: 1,
                            approval_sequence,
                        },
                    ))
                    .await
                    .unwrap();
            }
        }
    }
}

/// The last step cannot be taken twice: the step is recorded only if the
/// limits still allow it in the same compare-and-set.
#[tokio::test]
async fn two_claims_cannot_both_take_the_last_step() {
    for (ctx, tb, t, wf) in each().await {
        let inner = tb.dyn_backend();
        let hook = Arc::new(RaceAdmission {
            inner: inner.clone(),
            armed: AtomicBool::new(false),
            eid: Mutex::new(None),
            agents: vec![AGENT.into()],
            approval_sequence: Mutex::new(None),
        });
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(inner.clone(), hook.clone()));
        let mut run = start_on(
            &tb,
            hooked,
            &[card(json!({ "max_iterations": 1 }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        *hook.eid.lock().unwrap() = Some(run.eid.clone());
        hook.armed.store(true, Ordering::SeqCst);
        let ran = run.step(t, DONE).await;
        assert!(!ran, "{ctx}: both claims took the last step");
        assert_eq!(run.admitted().await, 1, "{ctx}: only the racing step");
    }
}

// ── escalation approvals are single-use ─────────────────────────────────────

async fn approve_latest(run: &Run, ctx: &str) {
    let digest = run
        .events()
        .await
        .into_iter()
        .rev()
        .find_map(|k| match k {
            EventKind::ToolApprovalRequired { context, .. } => {
                Some(context["action_digest"].clone())
            }
            _ => None,
        })
        .expect("a request");
    let (st, body) = support::post(
        &run.state,
        &format!("/executions/{}/approve", run.eid_path()),
        json!({ "decision": "approved", "node_id": "n1", "action_digest": digest }),
    )
    .await;
    assert!(st.is_success(), "{ctx}: approve: {st} {body}");
}

async fn requests(run: &Run) -> Vec<Value> {
    run.events()
        .await
        .into_iter()
        .filter_map(|k| match k {
            EventKind::ToolApprovalRequired { context, .. } => Some(context),
            _ => None,
        })
        .collect()
}

/// An approval releases ONE step past the limit. A step that fails does not
/// advance the node's occurrence, so its retry carries the same digest; it
/// must still ask again, or one approval would release every retry (and
/// silence the circuit breaker).
#[tokio::test]
async fn one_escalation_approval_releases_one_step_even_when_it_fails() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(json!({
                "max_iterations": 1,
                "max_consecutive_errors": 10,
                "on_limit_exceeded": "escalate_to_human"
            }))],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(!run.step(t, DONE).await, "{ctx}");
        approve_latest(&run, &ctx).await;
        assert!(run.step(t, Act::Fail).await, "{ctx}: the approved step");
        assert!(
            !run.step(t, Act::Fail).await,
            "{ctx}: a retry ran on the same approval"
        );
        assert_eq!(requests(&run).await.len(), 2, "{ctx}: no fresh request");
        assert_eq!(run.admitted().await, 2, "{ctx}");
    }
}

/// An approval given before the limit was reached (a policy gate's, which
/// showed the reviewer no limit) does not release a step past the limit:
/// the step asks again, with the limit in the request.
#[tokio::test]
async fn an_approval_granted_before_the_limit_does_not_cover_it() {
    for (ctx, tb, t, wf) in each().await {
        let mut run = start(
            &tb,
            &[card(
                json!({ "max_iterations": 1, "on_limit_exceeded": "escalate_to_human" }),
            )],
            ir(
                &wf,
                mcp_tool(),
                Some(AGENT),
                None,
                Some(json!({ "require_approval_for": ["search_web"] })),
            ),
            json!({}),
        )
        .await;
        assert!(!run.step(t, DONE).await, "{ctx}: policy gate");
        assert!(
            requests(&run).await[0].get("autonomy_limit").is_none(),
            "{ctx}"
        );
        // A parallel branch of the same agent takes its last step.
        run.backend
            .append_event(jamjet_state::Event::new(
                run.eid.clone(),
                0,
                EventKind::AgentStepAdmitted {
                    node_id: "other".into(),
                    work_item_id: None,
                    agents: vec![AGENT.into()],
                    tool_calls: 1,
                    approval_sequence: None,
                },
            ))
            .await
            .unwrap();
        approve_latest(&run, &ctx).await;
        assert!(
            !run.step(t, DONE).await,
            "{ctx}: ran past the limit unasked"
        );
        let reqs = requests(&run).await;
        assert_eq!(reqs.len(), 2, "{ctx}");
        assert_eq!(
            reqs[1]["autonomy_limit"]["limit_type"],
            json!("max_iterations"),
            "{ctx}"
        );
    }
}

/// An approved escalation for one agent does not excuse another agent's
/// `fail` limit reached while the step was being decided.
#[tokio::test]
async fn an_approved_escalation_still_rechecks_failing_cards() {
    const OTHER: &str = "jamjet://acme/reviewer";
    for (ctx, tb, t, wf) in each().await {
        let inner = tb.dyn_backend();
        let hook = Arc::new(RaceAdmission {
            inner: inner.clone(),
            armed: AtomicBool::new(false),
            eid: Mutex::new(None),
            agents: vec![OTHER.into()],
            approval_sequence: Mutex::new(None),
        });
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(inner.clone(), hook.clone()));
        let mut other = card(json!({ "max_iterations": 2 }));
        other["id"] = json!(OTHER);
        other["uri"] = json!(OTHER);
        let mut run = start_on(
            &tb,
            hooked,
            &[
                card(json!({ "max_iterations": 1, "on_limit_exceeded": "escalate_to_human" })),
                other,
            ],
            ir(&wf, mcp_tool(), Some(AGENT), Some(OTHER), None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(!run.step(t, DONE).await, "{ctx}: escalated");
        approve_latest(&run, &ctx).await;
        *hook.eid.lock().unwrap() = Some(run.eid.clone());
        hook.armed.store(true, Ordering::SeqCst);
        assert!(
            !run.step(t, DONE).await,
            "{ctx}: ran although the other card's limit was reached"
        );
    }
}

/// One admitted item backs one result. SQLite's legacy unfenced completion
/// settles an already-settled item again; the second result must not be
/// recorded, or it would end a newer step of the node and reset the breaker.
#[tokio::test]
async fn one_admission_backs_one_result() {
    for tb in support::backends().await {
        let run = start(
            &tb,
            &[card(json!({ "max_iterations": 5 }))],
            ir("wf-one", mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        let (_, claimed) = support::post(
            &run.state,
            "/work-items/claim",
            json!({ "worker_id": "w", "queue_types": QUEUES }),
        )
        .await;
        let id = claimed["work_item"]["id"].as_str().unwrap().to_string();
        for _ in 0..3 {
            let _ = support::post(
                &run.state,
                &format!("/work-items/{id}/complete"),
                json!({ "output": {}, "state_patch": {} }),
            )
            .await;
        }
        let completed = run
            .events()
            .await
            .iter()
            .filter(|k| matches!(k, EventKind::NodeCompleted { .. }))
            .count();
        assert_eq!(completed, 1, "{}", tb.name());
    }
}

/// Two items holding the same approved escalation: only one may use it, also
/// when the other uses it between this one's check and its record.
#[tokio::test]
async fn an_escalation_approval_cannot_be_used_twice_in_a_race() {
    for (ctx, tb, t, wf) in each().await {
        let inner = tb.dyn_backend();
        let hook = Arc::new(RaceAdmission {
            inner: inner.clone(),
            armed: AtomicBool::new(false),
            eid: Mutex::new(None),
            agents: vec![AGENT.into()],
            approval_sequence: Mutex::new(None),
        });
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(inner.clone(), hook.clone()));
        let mut run = start_on(
            &tb,
            hooked,
            &[card(
                json!({ "max_iterations": 1, "on_limit_exceeded": "escalate_to_human" }),
            )],
            ir(&wf, mcp_tool(), Some(AGENT), None, None),
            json!({}),
        )
        .await;
        assert!(run.step(t, DONE).await, "{ctx}");
        assert!(!run.step(t, DONE).await, "{ctx}");
        approve_latest(&run, &ctx).await;
        let seq = run
            .backend
            .get_events(&run.eid)
            .await
            .unwrap()
            .into_iter()
            .rev()
            .find(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
            .unwrap()
            .sequence;
        *hook.approval_sequence.lock().unwrap() = Some(seq);
        *hook.eid.lock().unwrap() = Some(run.eid.clone());
        hook.armed.store(true, Ordering::SeqCst);
        assert!(
            !run.step(t, DONE).await,
            "{ctx}: the approval was used twice"
        );
    }
}
