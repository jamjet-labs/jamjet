//! A `model` node with `executor: "worker"` is run by the run's own worker.
//!
//! The engine hands the step, by execution, to the worker that started the
//! run: the resolved request, the idempotency key and what the run's caps
//! leave. The worker makes the model call and reports the answer. The engine
//! still decides what may run and what is recorded. It checks the model
//! against the allow list at claim and the model the worker reports at
//! completion, holds the step to the run's caps, its approvals and its agent
//! card, refuses an answer that is late, repeated or malformed, and builds the
//! step's output and state from the answer itself, so a worker never writes a
//! state key. A tool call the model asks for still passes the dispatch guard.
//!
//! A test marked GUARD passes before the feature exists: it pins behaviour
//! the feature must not change.

mod support;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_api::state::AppState;
use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_models::adapter::ToolCall;
use jamjet_models::{
    ModelAdapter, ModelError, ModelRegistry, ModelRequest, ModelResponse, StructuredRequest,
};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use jamjet_state::{ClaimFilter, InMemoryBackend, SqliteBackend};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use support::TestBackend;
use tower::ServiceExt;

/// The queue a worker model step is dispatched to.
const QUEUE: &str = "model_worker";

/// The model the IR's `planner` alias resolves to.
const MODEL: &str = "vertex_ai/gemini-2.5-pro";

/// The agent a workflow acts for in the autonomy test.
const AGENT: &str = "jamjet://acme/planner";

// ── Workflows ────────────────────────────────────────────────────────────────

fn node(id: &str, kind: Value) -> Value {
    json!({
        "id": id, "kind": kind,
        "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
    })
}

/// An OpenAI-format function schema, as the SDK compiles a tool.
fn tool_schema(name: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": format!("the {name} tool"),
            "parameters": { "type": "object", "properties": {} }
        }
    })
}

/// A `model` node the engine runs, offering `tools`.
fn engine_model(tools: &[&str]) -> Value {
    json!({
        "type": "model",
        "model_ref": "planner",
        "prompt_ref": "Find a venue in {{ state.city }}",
        "output_schema": "answer",
        "system_prompt": "You plan events.",
        "tools": tools.iter().map(|t| tool_schema(t)).collect::<Vec<_>>()
    })
}

/// The same node, run by the run's own worker.
fn worker_model(tools: &[&str]) -> Value {
    let mut kind = engine_model(tools);
    kind["executor"] = json!("worker");
    kind
}

fn python_fn(function: &str) -> Value {
    json!({ "type": "python_fn", "module": "app.steps", "function": function, "output_schema": "" })
}

/// `nodes` run in the order given, then `end`, with every key of `extra`
/// (policy, caps, agent) set on the workflow.
fn ir(workflow_id: &str, nodes: &[(&str, Value)], extra: Value) -> Value {
    let mut map = serde_json::Map::new();
    let mut edges = Vec::new();
    for (i, (id, kind)) in nodes.iter().enumerate() {
        map.insert(id.to_string(), node(id, kind.clone()));
        let to = nodes.get(i + 1).map(|(n, _)| *n).unwrap_or("end");
        edges.push(json!({ "from": id, "to": to, "condition": null }));
    }
    let mut ir = json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": nodes[0].0,
        "nodes": map, "edges": edges,
        "retry_policies": {}, "timeouts": {},
        "models": {
            "planner": {
                "provider": "vertex_ai", "model": "gemini-2.5-pro",
                "temperature": 0.5, "max_tokens": 512
            }
        },
        "tools": {}, "mcp_servers": {}, "remote_agents": {}, "labels": {}
    });
    for (key, value) in extra.as_object().unwrap() {
        ir[key] = value.clone();
    }
    ir
}

/// One worker model step, `think`.
fn one_step(workflow_id: &str, tools: &[&str], extra: Value) -> Value {
    ir(workflow_id, &[("think", worker_model(tools))], extra)
}

fn policy(model_allowlist: &[&str]) -> Value {
    json!({ "blocked_tools": [], "require_approval_for": [], "model_allowlist": model_allowlist })
}

fn input() -> Value {
    json!({ "city": "Lisbon" })
}

// ── Answers ──────────────────────────────────────────────────────────────────

/// A model answer that asks for one call to `search`.
fn answer() -> Value {
    json!({
        "content": "",
        "finish_reason": "tool_calls",
        "model": MODEL,
        "tool_calls": [ { "id": "c1", "name": "search", "arguments": { "q": "venues" } } ],
        "input_tokens": 812,
        "output_tokens": 95,
        "cost_usd": 0.0031
    })
}

/// An answer with text and no tool calls, reporting `input` and `output`
/// tokens and no cost.
fn text_answer(input: u64, output: u64) -> Value {
    json!({
        "content": "Book the riverside hall.",
        "finish_reason": "stop",
        "model": MODEL,
        "tool_calls": [],
        "input_tokens": input,
        "output_tokens": output
    })
}

/// [`answer`] with `key` set to `value`.
fn answer_with(key: &str, value: Value) -> Value {
    let mut a = answer();
    a[key] = value;
    a
}

/// [`answer`] without `keys`.
fn answer_without(keys: &[&str]) -> Value {
    let mut a = answer();
    for key in keys {
        a.as_object_mut().unwrap().remove(*key);
    }
    a
}

/// A completion of the claimed `wi` carrying `model_result`, as a worker
/// sends it.
fn result_of(wi: &Value, model_result: Value) -> Value {
    json!({
        "lease_fence": wi["lease_fence"],
        "idempotency_key": wi["idempotency_key"],
        "duration_ms": 1840,
        "model_result": model_result
    })
}

// ── Requests ─────────────────────────────────────────────────────────────────

/// A request through the router, with authentication on (`token`) or in dev
/// mode (`None`).
async fn send(
    state: &AppState,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (u16, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = build_router_with_opts(state.clone(), token.is_none())
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A claim of `execution_id`'s worker model step by `worker`.
fn claim_request(worker: &str, execution_id: &str) -> Value {
    json!({ "worker_id": worker, "queue_types": [QUEUE], "execution_id": execution_id })
}

fn parse_id(raw: &str) -> ExecutionId {
    ExecutionId(uuid::Uuid::parse_str(raw.trim_start_matches("exec_")).unwrap())
}

// ── A run, driven by hand ────────────────────────────────────────────────────

/// One started run and the scheduler that drives it.
struct Run {
    name: &'static str,
    tb: TestBackend,
    backend: Arc<dyn StateBackend>,
    state: AppState,
    /// The `exec_…` id the API hands out.
    id: String,
    eid: ExecutionId,
    scheduler: Scheduler,
}

async fn start(tb: TestBackend, ir: Value) -> Run {
    let backend = tb.dyn_backend();
    let state = support::make_state(backend.clone());
    start_on(tb, state, ir).await
}

async fn start_on(tb: TestBackend, state: AppState, ir: Value) -> Run {
    let backend = tb.dyn_backend();
    let id = support::register_and_start(&state, ir, input()).await;
    Run {
        name: tb.name(),
        eid: parse_id(&id),
        id,
        scheduler: Scheduler::new(backend.clone()),
        tb,
        backend,
        state,
    }
}

impl Run {
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        send(&self.state, "POST", path, None, body).await
    }

    /// A claim of this run's worker model step by `worker`.
    async fn claim_as(&self, worker: &str) -> Value {
        let (st, body) = self
            .post("/work-items/claim", claim_request(worker, &self.id))
            .await;
        assert_eq!(st, 200, "{}: claim: {body}", self.name);
        body
    }

    /// Claim this run's worker model step; it must be handed out.
    async fn step(&self) -> Value {
        self.step_as("router").await
    }

    async fn step_as(&self, worker: &str) -> Value {
        let body = self.claim_as(worker).await;
        assert_eq!(
            body["claimed"],
            json!(true),
            "{}: the run's worker got no model step: {body}; the log was {:?}",
            self.name,
            self.log().await
        );
        body["work_item"].clone()
    }

    async fn complete(&self, wi: &Value, body: Value) -> (u16, Value) {
        self.post(
            &format!("/work-items/{}/complete", wi["id"].as_str().unwrap()),
            body,
        )
        .await
    }

    async fn tick(&self) {
        self.scheduler.tick().await.expect("scheduler tick");
    }

    async fn status(&self) -> WorkflowStatus {
        self.backend
            .get_execution(&self.eid)
            .await
            .unwrap()
            .unwrap()
            .status
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

    /// Every `NodeCompleted` of `node`, as JSON.
    async fn completions(&self, node: &str) -> Vec<Value> {
        self.events()
            .await
            .into_iter()
            .filter(|k| matches!(k, EventKind::NodeCompleted { node_id, .. } if node_id == node))
            .map(|k| serde_json::to_value(k).unwrap())
            .collect()
    }

    /// The error `node` failed with for good, if it did.
    async fn failed_for_good(&self, node: &str) -> Option<String> {
        self.events().await.into_iter().find_map(|k| match k {
            EventKind::NodeFailed {
                node_id,
                error,
                retryable: false,
                ..
            } if node_id == node => Some(error),
            _ => None,
        })
    }

    /// The `rule` of every `PolicyViolation` recorded for `node`.
    async fn violations(&self, node: &str) -> Vec<String> {
        self.events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::PolicyViolation { node_id, rule, .. } if node_id == node => Some(rule),
                _ => None,
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

    /// The nodes dispatched, in log order, with their queue.
    async fn scheduled(&self) -> Vec<(String, String)> {
        self.events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::NodeScheduled {
                    node_id,
                    queue_type,
                } => Some((node_id, queue_type)),
                _ => None,
            })
            .collect()
    }

    /// Claim the `python_fn` step that is ready over HTTP, oldest first,
    /// and complete it reporting `report`.
    async fn http_step(&self, report: Value) {
        let (st, claimed) = self
            .post(
                "/work-items/claim",
                json!({ "worker_id": "py", "queue_types": ["python_tool"] }),
            )
            .await;
        assert_eq!(st, 200, "{}: {claimed}", self.name);
        assert_eq!(claimed["claimed"], json!(true), "{}: {claimed}", self.name);
        let wi = &claimed["work_item"];
        let mut body = json!({
            "output": {}, "state_patch": {},
            "lease_fence": wi["lease_fence"], "idempotency_key": wi["idempotency_key"]
        });
        for (key, value) in report.as_object().unwrap() {
            body[key] = value.clone();
        }
        let (st, done) = self.complete(wi, body).await;
        assert_eq!(st, 200, "{}: complete: {done}", self.name);
    }

    /// Assert that the claimed step `wi` was left as it was: nothing added
    /// to the log since it held `before` events, and the item still leased.
    async fn unchanged(&self, wi: &Value, before: usize, what: &str) {
        let name = self.name;
        assert_eq!(
            self.events().await.len(),
            before,
            "{name}: {what} recorded something: {:?}",
            self.log().await
        );
        let id = uuid::Uuid::parse_str(wi["id"].as_str().unwrap()).unwrap();
        assert_eq!(
            self.tb.item_status(id).await,
            "claimed",
            "{name}: {what} settled the item"
        );
    }
}

/// A model that answers every call with `answer` and counts its calls.
struct Scripted {
    answer: ModelResponse,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelAdapter for Scripted {
    fn system_name(&self) -> &'static str {
        "scripted"
    }
    fn default_model(&self) -> &str {
        MODEL
    }
    async fn chat(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.clone())
    }
    async fn structured_output(&self, _: StructuredRequest) -> Result<ModelResponse, ModelError> {
        Err(ModelError::Network("not used here".into()))
    }
}

/// [`answer`] as the engine's model adapter returns it.
fn scripted_answer() -> ModelResponse {
    ModelResponse {
        content: String::new(),
        model: MODEL.into(),
        finish_reason: "tool_calls".into(),
        input_tokens: 812,
        output_tokens: 95,
        structured: None,
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "search".into(),
            arguments: json!({ "q": "venues" }),
        }],
        cost_usd: Some(0.0031),
    }
}

/// A registry whose only adapter answers [`scripted_answer`], and its call
/// count.
fn scripted_registry() -> (Arc<ModelRegistry>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = ModelRegistry::new()
        .register(Arc::new(Scripted {
            answer: scripted_answer(),
            calls: calls.clone(),
        }))
        .with_default("scripted");
    (Arc::new(registry), calls)
}

/// Memory, SQLite and SQLite scoped to the default tenant.
async fn three_backends() -> Vec<(&'static str, Arc<dyn StateBackend>)> {
    let path = |n: &str| {
        std::env::temp_dir().join(format!("jjwm-{n}-{}.db", uuid::Uuid::new_v4().simple()))
    };
    let plain = format!("sqlite://{}", path("plain").display());
    let scoped = format!("sqlite://{}", path("scoped").display());
    vec![
        ("memory", Arc::new(InMemoryBackend::new())),
        (
            "sqlite",
            Arc::new(SqliteBackend::open(&plain).await.unwrap()),
        ),
        (
            "tenant_scoped",
            Arc::new(
                SqliteBackend::open(&scoped)
                    .await
                    .unwrap()
                    .for_tenant(jamjet_state::TenantId::from("default")),
            ),
        ),
    ]
}

/// Set the default tenant's policy: through `PUT /tenants` where the
/// backend manages tenants, else in its table (the plain SQLite backend does
/// not; its `default` tenant row comes from the migrations).
async fn set_tenant_policy(run: &Run, p: Value) {
    let name = run.name;
    let default = jamjet_state::TenantId::from("default");
    if run.backend.get_tenant(&default).await.unwrap().is_none() {
        let (st, body) = run
            .post("/tenants", json!({ "id": "default", "name": "default" }))
            .await;
        assert!((200..300).contains(&st), "{name}: {st} {body}");
    }
    let (st, body) = send(
        &run.state,
        "PUT",
        "/tenants/default",
        None,
        json!({ "policy": p.clone() }),
    )
    .await;
    if (200..300).contains(&st) {
        return;
    }
    assert!(
        body.to_string().contains("not supported"),
        "{name}: {st} {body}"
    );
    let TestBackend::Sqlite { backend, .. } = &run.tb else {
        panic!("{name}: the backend took no tenant policy: {st} {body}");
    };
    sqlx::query("UPDATE tenants SET policy_json = ? WHERE id = 'default'")
        .bind(p.to_string())
        .execute(&backend.pool())
        .await
        .unwrap();
}

// ── The IR, the queue, routing ───────────────────────────────────────────────

/// GUARD: passes before `model` takes `executor`. A model node without it
/// serializes as it did, so its definition hash, which an approval in flight
/// binds, is unchanged.
#[tokio::test]
async fn a_model_node_without_executor_keeps_its_definition_hash() {
    let plain = node(
        "think",
        json!({ "type": "model", "model_ref": "planner", "prompt_ref": "", "output_schema": "" }),
    );
    let with_tools = node("think", engine_model(&["search"]));
    for (node, serialized, hash) in [
        (
            plain,
            r#"{"description":null,"id":"think","kind":{"model_ref":"planner","output_schema":"","prompt_ref":"","system_prompt":null,"tools":[],"type":"model"},"labels":{},"node_timeout_secs":null,"retry_policy":null}"#,
            "1b47a34af87dfde27498efa5fed1d3747e09e282bb64e0260183249767b22a62",
        ),
        (
            with_tools,
            r#"{"description":null,"id":"think","kind":{"model_ref":"planner","output_schema":"answer","prompt_ref":"Find a venue in {{ state.city }}","system_prompt":"You plan events.","tools":[{"function":{"description":"the search tool","name":"search","parameters":{"properties":{},"type":"object"}},"type":"function"}],"type":"model"},"labels":{},"node_timeout_secs":null,"retry_policy":null}"#,
            "2eaf29f310911b438694f9585282ec27f750daff7063ba2d0adb2042deae765e",
        ),
    ] {
        let def: jamjet_ir::workflow::NodeDef = serde_json::from_value(node).unwrap();
        let value = serde_json::to_value(&def).unwrap();
        assert_eq!(jamjet_state::canonical_json(&value), serialized);
        assert_eq!(
            jamjet_state::approvals::definition_hash(&value),
            hash,
            "{serialized}"
        );
    }
}

/// The step goes to the `model_worker` queue, marked for its execution: the
/// oldest-first claim never hands it out, nor does a claim that names only
/// the node, nor the claim the engine's own workers make. A claim naming the
/// run does.
#[tokio::test]
async fn a_worker_model_step_is_never_given_to_the_oldest_first_claim() {
    for (name, backend) in three_backends().await {
        let state = support::make_state(backend.clone());
        let id =
            support::register_and_start(&state, one_step("wf-own-model", &[], json!({})), input())
                .await;
        for queues in [
            json!([QUEUE]),
            json!([QUEUE, "python_tool", "model", "general"]),
        ] {
            let (st, body) = send(
                &state,
                "POST",
                "/work-items/claim",
                None,
                json!({ "worker_id": "other", "queue_types": queues }),
            )
            .await;
            assert_eq!(st, 200, "{name}: {body}");
            assert_eq!(
                body,
                json!({ "claimed": false }),
                "{name}: the oldest-first claim took a worker model step from {queues}"
            );
        }
        let by_node = backend
            .claim_work_item_filtered(
                "other",
                &[QUEUE],
                None,
                &ClaimFilter {
                    execution_id: None,
                    node_id: Some("think".into()),
                },
            )
            .await;
        assert!(
            !matches!(by_node, Ok(Some(_))),
            "{name}: a node-only filter took a worker model step"
        );
        let engine = backend
            .claim_work_item("engine", &["model", "general", QUEUE])
            .await
            .unwrap();
        assert!(
            engine.is_none(),
            "{name}: the engine's own claim took the step: {engine:?}"
        );

        let (_, body) = send(
            &state,
            "POST",
            "/work-items/claim",
            None,
            claim_request("router", &id),
        )
        .await;
        assert_eq!(body["claimed"], json!(true), "{name}: {body}");
        assert_eq!(body["work_item"]["queue_type"], QUEUE, "{name}: {body}");
        assert_eq!(
            body["work_item"]["payload"]["claim"], "by_execution",
            "{name}: {body}"
        );
    }
}

/// The engine's in-process pool, with a model adapter that would answer,
/// never runs the step: even polling `model_worker`, its claim skips an item
/// marked for its execution. The step stays pending for the run's worker.
#[tokio::test]
async fn a_worker_model_step_is_never_run_by_the_in_process_pool() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-not-in-process", &[], json!({}))).await;
        let name = run.name;
        let (registry, calls) = scripted_registry();
        let pool = jamjet_worker::server_pool(run.backend.clone(), registry).build_worker(
            "engine".into(),
            ["general", "tool", "retrieval", "model", "privileged", QUEUE]
                .iter()
                .map(|q| q.to_string())
                .collect(),
        );
        for _ in 0..4 {
            let _ = pool.poll_and_execute().await;
            run.tick().await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{name}: the engine called the model for a worker model step; the log was {:?}",
            run.log().await
        );
        let open = run.backend.open_work_items(&run.eid).await.unwrap();
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        assert_eq!(
            (
                open[0].node_id.as_str(),
                open[0].queue_type.as_str(),
                open[0].claimed
            ),
            ("think", QUEUE, false),
            "{name}: the step is not pending for the run's worker"
        );
        assert_eq!(run.status().await, WorkflowStatus::Running, "{name}");
    }
}

/// GUARD: the engine's own `model` queue is still never served to an
/// external claim, oldest first or by execution.
#[tokio::test]
async fn the_engine_model_queue_is_still_never_served() {
    for tb in support::backends().await {
        let run = start(
            tb,
            ir(
                "wf-engine-model",
                &[("think", engine_model(&[]))],
                json!({}),
            ),
        )
        .await;
        let name = run.name;
        let (_, body) = run
            .post(
                "/work-items/claim",
                json!({ "worker_id": "other", "queue_types": ["model"] }),
            )
            .await;
        assert_eq!(body, json!({ "claimed": false }), "{name}");
        let (_, body) = run
            .post(
                "/work-items/claim",
                json!({ "worker_id": "router", "queue_types": ["model"], "execution_id": run.id }),
            )
            .await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "not_found" }),
            "{name}"
        );
        let body = run.claim_as("router").await;
        assert_eq!(body["claimed"], json!(false), "{name}: {body}");
        let open = run.backend.open_work_items(&run.eid).await.unwrap();
        assert_eq!(open.len(), 1, "{name}: {open:?}");
        assert_eq!(
            (open[0].queue_type.as_str(), open[0].claimed),
            ("model", false),
            "{name}"
        );
    }
}

/// Queue `node`'s item for `run` directly in the store, on `queue`, with the
/// payload the scheduler builds for it: an item an operator wrote by hand.
/// `marked`: with the by-execution marker, which the payload builder writes
/// for a node claimed by its execution; else without it.
async fn hand_enqueue(run: &Run, ir: &Value, node_id: &str, queue: &str, marked: bool) {
    let parsed: jamjet_ir::WorkflowIr = serde_json::from_value(ir.clone()).unwrap();
    let mut payload = jamjet_scheduler::build_work_item_payload(
        &parsed,
        node_id,
        parsed.node(node_id).unwrap(),
        input().as_object().unwrap(),
        &parsed.workflow_id,
        "0.1.0",
    );
    let obj = payload.as_object_mut().unwrap();
    if marked {
        obj.insert(
            jamjet_core::node::CLAIM_PAYLOAD_KEY.into(),
            jamjet_core::node::CLAIM_BY_EXECUTION.into(),
        );
    } else {
        obj.remove(jamjet_core::node::CLAIM_PAYLOAD_KEY);
    }
    let seq = run.backend.latest_sequence(&run.eid).await.unwrap() + 1;
    run.backend
        .append_event(jamjet_state::Event::new(
            run.eid.clone(),
            seq,
            EventKind::NodeScheduled {
                node_id: node_id.into(),
                queue_type: queue.into(),
            },
        ))
        .await
        .unwrap();
    run.backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: run.eid.clone(),
            node_id: node_id.into(),
            queue_type: queue.into(),
            payload,
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            tenant_id: jamjet_state::DEFAULT_TENANT.into(),
            lease_fence: 0,
        })
        .await
        .unwrap();
}

/// GUARD: an item of a node the engine runs, put on `model_worker` by hand,
/// is withheld and its node failed, whichever claim reaches it.
#[tokio::test]
async fn an_engine_model_node_enqueued_on_model_worker_fails_closed() {
    for by_execution in [false, true] {
        for tb in support::backends().await {
            let workflow = ir(
                "wf-misrouted",
                &[("prep", python_fn("prep")), ("think", engine_model(&[]))],
                json!({}),
            );
            let run = start(tb, workflow.clone()).await;
            let name = run.name;
            hand_enqueue(&run, &workflow, "think", QUEUE, false).await;
            let body = if by_execution {
                run.claim_as("router").await
            } else {
                run.post(
                    "/work-items/claim",
                    json!({ "worker_id": "other", "queue_types": [QUEUE] }),
                )
                .await
                .1
            };
            let expected = if by_execution {
                json!({ "claimed": false, "reason": "blocked" })
            } else {
                json!({ "claimed": false })
            };
            assert_eq!(body, expected, "{name}, by execution {by_execution}");
            let error = run.failed_for_good("think").await;
            assert!(
                error
                    .as_deref()
                    .is_some_and(|e| e.contains("a kind the engine runs itself")),
                "{name}, by execution {by_execution}: {error:?}"
            );
        }
    }
}

/// The gate tells the two apart by the node's `executor` in the registered
/// IR, not by the queue: a worker model node's item on `model_worker`,
/// marked as the payload builder marks it, is handed to its run's worker.
#[tokio::test]
async fn a_worker_model_node_queued_on_model_worker_by_hand_is_handed_out() {
    for tb in support::backends().await {
        let workflow = ir(
            "wf-hand-queued",
            &[("prep", python_fn("prep")), ("think", worker_model(&[]))],
            json!({}),
        );
        let run = start(tb, workflow.clone()).await;
        let name = run.name;
        hand_enqueue(&run, &workflow, "think", QUEUE, true).await;
        let body = run.claim_as("router").await;
        assert_eq!(
            body["claimed"],
            json!(true),
            "{name}: {body}; the log was {:?}",
            run.log().await
        );
        assert_eq!(body["work_item"]["node_id"], "think", "{name}");
        assert!(run.failed_for_good("think").await.is_none(), "{name}");
    }
}

/// GUARD: a worker model node's item put on the engine's `model` queue by an
/// operator, without the marker, is run by the engine with its own model
/// adapter. Safe either way: the engine makes the call under its own guard,
/// and no external worker gets the step.
#[tokio::test]
async fn a_worker_model_node_enqueued_on_model_is_run_by_the_engine() {
    for tb in support::backends().await {
        let workflow = ir(
            "wf-operator",
            &[
                ("prep", python_fn("prep")),
                ("think", worker_model(&["search"])),
            ],
            json!({}),
        );
        let run = start(tb, workflow.clone()).await;
        let name = run.name;
        hand_enqueue(&run, &workflow, "think", "model", false).await;
        let (registry, calls) = scripted_registry();
        let pool = jamjet_worker::server_pool(run.backend.clone(), registry)
            .build_worker("engine".into(), vec!["model".into()]);
        assert!(
            pool.poll_and_execute().await.unwrap(),
            "{name}: the engine claimed nothing"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
        let done = run.completions("think").await;
        assert_eq!(done.len(), 1, "{name}: {:?}", run.log().await);
        assert_eq!(
            done[0]["output"]["tool_calls"][0]["name"], "search",
            "{name}"
        );
        let body = run.claim_as("router").await;
        assert_eq!(body["claimed"], json!(false), "{name}: {body}");
    }
}

async fn register_status(state: &AppState, ir: Value) -> (u16, Value) {
    send(state, "POST", "/workflows", None, json!({ "ir": ir })).await
}

/// `executor` means something on `model` only, and only `engine` or
/// `worker`; `claim` stays refused on `model` (a worker model step is always
/// claimed by its execution).
#[tokio::test]
async fn executor_on_a_tool_node_and_claim_on_a_model_node_are_refused() {
    let state = support::make_state(Arc::new(InMemoryBackend::new()));
    let mut with_executor = python_fn("prep");
    with_executor["executor"] = json!("worker");
    let mut external = json!({
        "type": "external_fn", "queue": "app-calls", "handler": "h", "output_schema": ""
    });
    external["executor"] = json!("worker");
    let mut claimed_model = worker_model(&[]);
    claimed_model["claim"] = json!("by_execution");
    let mut unknown = engine_model(&[]);
    unknown["executor"] = json!("whoever");
    // An unknown value may be named by the field or by the value itself
    // (serde's wording quotes the value only).
    for (what, kind, needles) in [
        ("executor on python_fn", with_executor, &["executor"][..]),
        ("executor on external_fn", external, &["executor"][..]),
        ("claim on model", claimed_model, &["claim"][..]),
        ("an unknown executor", unknown, &["executor", "whoever"][..]),
    ] {
        let (st, body) = register_status(&state, ir("wf-refused", &[("n", kind)], json!({}))).await;
        assert_eq!(st, 400, "{what}: {body}");
        let text = body.to_string();
        assert!(needles.iter().any(|n| text.contains(n)), "{what}: {body}");
    }
    let mut explicit = engine_model(&[]);
    explicit["executor"] = json!("engine");
    for (what, kind) in [
        ("executor: engine", explicit),
        ("executor: worker", worker_model(&[])),
    ] {
        let wf = format!("wf-{}", what.replace([':', ' '], ""));
        let (st, body) = register_status(&state, ir(&wf, &[("n", kind)], json!({}))).await;
        assert!((200..300).contains(&st), "{what}: {st} {body}");
    }
}

/// `model_worker` is a built-in queue now, so no `external_fn` may name it.
#[tokio::test]
async fn an_external_fn_named_model_worker_is_refused() {
    let state = support::make_state(Arc::new(InMemoryBackend::new()));
    let kind = json!({
        "type": "external_fn", "queue": QUEUE, "handler": "h", "output_schema": ""
    });
    let (st, body) = register_status(&state, ir("wf-squat", &[("n", kind)], json!({}))).await;
    assert_eq!(st, 400, "{body}");
    assert!(body.to_string().contains("built-in queue"), "{body}");
}

// ── The claim ────────────────────────────────────────────────────────────────

/// Only the run's initiator may claim its model step; another principal is
/// told `not_found` and the oldest-first claim gets nothing.
#[tokio::test]
async fn another_token_cannot_claim_a_worker_model_step() {
    let path = std::env::temp_dir().join(format!("jjwm-{}.db", uuid::Uuid::new_v4()));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (bob, _) = sqlite.create_token("bob", "developer").await.unwrap();
    let state = support::make_state(sqlite);
    let (st, body) = send(
        &state,
        "POST",
        "/workflows",
        Some(&alice),
        json!({ "ir": one_step("wf-alice", &[], json!({})) }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    let (st, body) = send(
        &state,
        "POST",
        "/executions",
        Some(&alice),
        json!({ "workflow_id": "wf-alice", "workflow_version": "0.1.0", "input": input() }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    let id = body["execution_id"].as_str().unwrap().to_string();

    let (_, body) = send(
        &state,
        "POST",
        "/work-items/claim",
        Some(&bob),
        claim_request("bob-router", &id),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false, "reason": "not_found" }));
    let (_, body) = send(
        &state,
        "POST",
        "/work-items/claim",
        Some(&bob),
        json!({ "worker_id": "bob-router", "queue_types": [QUEUE] }),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false }));
    let (_, body) = send(
        &state,
        "POST",
        "/work-items/claim",
        Some(&alice),
        claim_request("alice-router", &id),
    )
    .await;
    assert_eq!(body["claimed"], json!(true), "{body}");
    let _ = std::fs::remove_file(path);
}

/// The allow list is checked at claim against the model the request calls
/// (the alias resolved): off the list, the step is `blocked`, the violation
/// recorded, the node failed and the run failed on the next pass.
#[tokio::test]
async fn a_model_off_the_allow_list_is_refused_at_claim() {
    for tb in support::backends().await {
        let run = start(
            tb,
            one_step(
                "wf-off-list",
                &[],
                json!({ "policy": policy(&["anthropic/*"]) }),
            ),
        )
        .await;
        let name = run.name;
        let body = run.claim_as("router").await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "blocked" }),
            "{name}: {:?}",
            run.log().await
        );
        let violations = run.violations("think").await;
        assert!(
            violations.iter().any(|r| r.contains(MODEL)),
            "{name}: {violations:?}"
        );
        assert!(run.failed_for_good("think").await.is_some(), "{name}");
        run.tick().await;
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
    }
}

/// `pay -> think` under `total_tokens: 100`. `pay` reports exactly 100
/// tokens: the run goes on, and the model step is refused at claim with
/// `budget`, its trip recorded, and the run fails on the next pass.
#[tokio::test]
async fn a_spent_run_refuses_the_claim_with_reason_budget() {
    for tb in support::backends().await {
        let run = start(
            tb,
            ir(
                "wf-spent",
                &[("pay", python_fn("pay")), ("think", worker_model(&[]))],
                json!({ "token_budget": { "total_tokens": 100 } }),
            ),
        )
        .await;
        let name = run.name;
        run.http_step(json!({ "input_tokens": 100, "output_tokens": 0 }))
            .await;
        run.tick().await;
        let body = run.claim_as("router").await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "budget" }),
            "{name}: {:?}",
            run.log().await
        );
        assert_eq!(
            run.trips().await,
            vec![json!({
                "type": "token_budget_exceeded", "node_id": "think",
                "kind": "total_tokens", "limit": 100, "current": 100
            })],
            "{name}"
        );
        run.tick().await;
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
    }
}

/// The claim hands the run's worker everything it needs to make the call:
/// the resolved request, the idempotency key the router answers a repeat
/// by, and what the caps leave. `budget_left` sits beside the payload, not
/// in it, so the payload and the digests that bind it are unchanged.
#[tokio::test]
async fn the_claim_carries_the_request_the_idempotency_key_and_budget_left() {
    for tb in support::backends().await {
        let run = start(
            tb,
            ir(
                "wf-request",
                &[
                    ("pay", python_fn("pay")),
                    ("think", worker_model(&["search"])),
                ],
                json!({ "token_budget": { "total_tokens": 2000 }, "cost_budget_usd": 1.0 }),
            ),
        )
        .await;
        let name = run.name;
        run.http_step(json!({ "input_tokens": 800, "output_tokens": 0, "cost_usd": 0.58 }))
            .await;
        run.tick().await;
        let wi = run.step().await;
        assert_eq!(
            wi["payload"],
            json!({
                "workflow_id": "wf-request",
                "workflow_version": "0.1.0",
                "node_id": "think",
                "model": MODEL,
                "temperature": 0.5,
                "max_tokens": 512,
                "system_prompt": "You plan events.",
                "tools": [tool_schema("search")],
                "prompt": "Find a venue in Lisbon",
                "output_schema": "answer",
                "claim": "by_execution"
            }),
            "{name}"
        );
        assert_eq!(
            (
                wi["node_id"].as_str(),
                wi["queue_type"].as_str(),
                wi["attempt"].as_u64()
            ),
            (Some("think"), Some(QUEUE), Some(0)),
            "{name}: {wi}"
        );
        let key = jamjet_state::derive_idempotency_key(run.backend.as_ref(), &run.eid, "think")
            .await
            .unwrap();
        assert_eq!(wi["idempotency_key"], json!(key), "{name}");
        assert_eq!(
            wi["budget_left"],
            json!({ "total_tokens": 1200, "input_tokens": null, "output_tokens": null, "cost_usd": 0.42 }),
            "{name}: {wi}"
        );
    }
}

/// A step reclaimed after its lease ran out offers the same idempotency
/// key, so the caller's router can answer the repeat from what it stored
/// instead of paying for the call again.
#[tokio::test]
async fn a_reclaimed_step_offers_the_same_idempotency_key() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-reclaim", &[], json!({}))).await;
        let name = run.name;
        let first = run.step_as("router-1").await;
        let id = uuid::Uuid::parse_str(first["id"].as_str().unwrap()).unwrap();
        run.tb.expire_lease(id).await;
        let again = run.step_as("router-2").await;
        assert_eq!(again["id"], first["id"], "{name}");
        assert_ne!(again["lease_fence"], first["lease_fence"], "{name}");
        assert!(
            first["idempotency_key"]
                .as_str()
                .is_some_and(|k| !k.is_empty()),
            "{name}: {first}"
        );
        assert_eq!(
            again["idempotency_key"], first["idempotency_key"],
            "{name}: the reclaimed step offered another key"
        );
    }
}

/// A model step the policy gates is held at claim, before the worker gets
/// its request: the request is raised and nothing is handed out. Once it is
/// approved, the step is dispatched again and handed out.
#[tokio::test]
async fn an_approval_gated_model_node_holds_before_the_worker_gets_it() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-gated", &[], json!({}))).await;
        let name = run.name;
        // A tenant catch-all names model calls too.
        set_tenant_policy(
            &run,
            json!({ "blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": [] }),
        )
        .await;
        let body = run.claim_as("router").await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "approval_required" }),
            "{name}: {:?}",
            run.log().await
        );
        let requested = run
            .events()
            .await
            .into_iter()
            .filter(|k| matches!(k, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "think"))
            .count();
        assert_eq!(requested, 1, "{name}: {:?}", run.log().await);

        let (st, body) = run
            .post(
                &format!("/executions/{}/approve", run.id),
                json!({ "decision": "approved", "node_id": "think" }),
            )
            .await;
        assert_eq!(st, 200, "{name}: {body}");
        run.tick().await;
        let wi = run.step().await;
        assert_eq!(wi["node_id"], "think", "{name}");
    }
}

/// A worker model step acting for an agent spends one of the agent's
/// steps, admitted at claim like any other step.
#[tokio::test]
async fn autonomy_limits_count_a_worker_model_step() {
    for tb in support::backends().await {
        let state = support::make_state(tb.dyn_backend());
        let (st, body) = send(
            &state,
            "POST",
            "/agents",
            None,
            json!({
                "id": AGENT, "uri": AGENT, "name": "planner", "description": "plans events",
                "version": "1.0.0",
                "capabilities": { "skills": [], "protocols": [], "tools_provided": [], "tools_consumed": [] },
                "autonomy": "fully_autonomous",
                "constraints": { "max_iterations": 5 },
                "auth": { "type": "none" }
            }),
        )
        .await;
        assert!((200..300).contains(&st), "{st} {body}");
        let run = start_on(
            tb,
            state,
            one_step("wf-agent-step", &[], json!({ "agent_ref": AGENT })),
        )
        .await;
        let name = run.name;
        let wi = run.step().await;
        let admitted: Vec<(Option<uuid::Uuid>, Vec<String>)> = run
            .events()
            .await
            .into_iter()
            .filter_map(|k| match k {
                EventKind::AgentStepAdmitted {
                    node_id,
                    work_item_id,
                    agents,
                    ..
                } if node_id == "think" => Some((work_item_id, agents)),
                _ => None,
            })
            .collect();
        let item = uuid::Uuid::parse_str(wi["id"].as_str().unwrap()).unwrap();
        assert_eq!(
            admitted,
            vec![(Some(item), vec![AGENT.to_string()])],
            "{name}"
        );
    }
}

// ── The completion ───────────────────────────────────────────────────────────

/// The fields of a `NodeCompleted` that depend on the answer alone. The
/// in-process worker adds the engine's own display counters (`__budget`)
/// to a step's state patch after its executor ran; those are left out.
fn answer_fields(completed: &Value) -> Value {
    json!({
        "output": completed["output"],
        "state_patch": jamjet_state::approvals::action_state(&completed["state_patch"]),
        "input_tokens": completed["input_tokens"],
        "output_tokens": completed["output_tokens"],
        "cost_usd": completed["cost_usd"],
        "finish_reason": completed["finish_reason"],
        "gen_ai_model": completed["gen_ai_model"]
    })
}

/// The same answer, from the engine's model adapter in process and from the
/// run's worker over HTTP, is recorded as the same step.
#[tokio::test]
async fn the_same_answer_writes_the_same_event_on_both_executors() {
    let engine_backends = support::backends().await;
    let worker_backends = support::backends().await;
    for (engine_tb, worker_tb) in engine_backends.into_iter().zip(worker_backends) {
        let engine_run = start(
            engine_tb,
            ir(
                "wf-both",
                &[("think", engine_model(&["search"]))],
                json!({}),
            ),
        )
        .await;
        let name = engine_run.name;
        let (registry, calls) = scripted_registry();
        let worker = jamjet_worker::Worker::new(
            "engine".into(),
            engine_run.backend.clone(),
            vec!["model".into()],
        )
        .register_executor(
            "model",
            Arc::new(jamjet_worker::ModelNodeExecutor::new(registry)),
        );
        assert!(worker.poll_and_execute().await.unwrap(), "{name}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{name}");
        let in_process = engine_run.completions("think").await;
        assert_eq!(in_process.len(), 1, "{name}: {:?}", engine_run.log().await);

        let worker_run = start(worker_tb, one_step("wf-both", &["search"], json!({}))).await;
        let wi = worker_run.step().await;
        let (st, body) = worker_run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{name}: {body}");
        let over_http = worker_run.completions("think").await;
        assert_eq!(over_http.len(), 1, "{name}: {:?}", worker_run.log().await);

        assert_eq!(
            answer_fields(&over_http[0]),
            answer_fields(&in_process[0]),
            "{name}"
        );
    }
}

/// The step's output and state are built by the engine from the answer:
/// the state patch has exactly the three keys the in-process executor
/// writes, and they land in the run's state.
#[tokio::test]
async fn the_engine_builds_the_state_patch() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-patch", &["search"], json!({}))).await;
        let name = run.name;
        let wi = run.step().await;
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{name}: {body}");
        assert_eq!(body["completed"], json!(true), "{name}: {body}");
        let done = run.completions("think").await;
        assert_eq!(done.len(), 1, "{name}");
        let calls = json!([ { "id": "c1", "name": "search", "arguments": { "q": "venues" } } ]);
        assert_eq!(
            done[0]["state_patch"],
            json!({
                "last_model_output": "",
                "last_model_finish_reason": "tool_calls",
                "last_model_tool_calls": calls
            }),
            "{name}"
        );
        assert_eq!(
            done[0]["output"],
            json!({
                "content": "", "model": MODEL, "finish_reason": "tool_calls", "tool_calls": calls
            }),
            "{name}"
        );
        assert_eq!(
            (
                done[0]["input_tokens"].clone(),
                done[0]["output_tokens"].clone(),
                done[0]["cost_usd"].clone(),
                done[0]["gen_ai_model"].clone()
            ),
            (json!(812), json!(95), json!(0.0031), json!(MODEL)),
            "{name}"
        );
        let state = run
            .backend
            .get_execution(&run.eid)
            .await
            .unwrap()
            .unwrap()
            .current_state;
        assert_eq!(state["last_model_finish_reason"], "tool_calls", "{name}");
        assert_eq!(state["last_model_tool_calls"], calls, "{name}");
    }
}

/// A worker model step's result is the model's answer, never an output or
/// a state patch of the worker's own: either is a 400 and records nothing.
#[tokio::test]
async fn a_result_with_state_patch_is_400() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-no-patch", &["search"], json!({}))).await;
        let name = run.name;
        let wi = run.step().await;
        let before = run.events().await.len();
        let mut with_patch = result_of(&wi, answer());
        with_patch["state_patch"] = json!({ "approved": true });
        let mut with_output = result_of(&wi, answer());
        with_output["output"] = json!({ "content": "forged" });
        let mut engine_keys = result_of(&wi, answer());
        engine_keys["state_patch"] = json!({ "__budget": { "spent": 0 } });
        let legacy = json!({
            "output": { "content": "forged" }, "state_patch": {},
            "lease_fence": wi["lease_fence"], "idempotency_key": wi["idempotency_key"]
        });
        for (what, body) in [
            ("a state patch", with_patch),
            ("an output", with_output),
            ("engine keys", engine_keys),
            ("an output and a state patch without an answer", legacy),
        ] {
            let (st, answer) = run.complete(&wi, body).await;
            assert_eq!(st, 400, "{name}: {what}: {answer}");
            run.unchanged(&wi, before, what).await;
        }
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{name}: {body}");
    }
}

/// `model_result` belongs to a worker model step only: on any other item it
/// is a 400 and records nothing.
#[tokio::test]
async fn a_model_result_for_another_kind_is_400() {
    for tb in support::backends().await {
        let run = start(tb, ir("wf-py", &[("pay", python_fn("pay"))], json!({}))).await;
        let name = run.name;
        let (_, claimed) = run
            .post(
                "/work-items/claim",
                json!({ "worker_id": "py", "queue_types": ["python_tool"] }),
            )
            .await;
        assert_eq!(claimed["claimed"], json!(true), "{name}: {claimed}");
        let wi = &claimed["work_item"];
        let before = run.events().await.len();
        let mut body = result_of(wi, answer());
        body["output"] = json!({});
        body["state_patch"] = json!({});
        let (st, answer) = run.complete(wi, body).await;
        assert_eq!(st, 400, "{name}: {answer}");
        run.unchanged(wi, before, "a model result on a python_fn item")
            .await;
    }
}

/// An answer that arrives after the step's lease ran out and the step was
/// taken back is a 409 and records nothing. The holder of the new lease
/// completes it.
#[tokio::test]
async fn a_result_after_the_lease_expired_is_409_and_records_nothing() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-late", &["search"], json!({}))).await;
        let name = run.name;
        let first = run.step_as("router-1").await;
        let id = uuid::Uuid::parse_str(first["id"].as_str().unwrap()).unwrap();
        run.tb.expire_lease(id).await;
        let again = run.step_as("router-2").await;
        let (st, body) = run.complete(&first, result_of(&first, answer())).await;
        assert_eq!(st, 409, "{name}: {body}");
        assert!(run.completions("think").await.is_empty(), "{name}");
        let (st, body) = run.complete(&again, result_of(&again, answer())).await;
        assert_eq!(st, 200, "{name}: {body}");
        assert_eq!(run.completions("think").await.len(), 1, "{name}");
    }
}

/// The same answer sent twice (the first reply was lost) is recorded once;
/// the second is a 409.
#[tokio::test]
async fn a_result_sent_twice_is_recorded_once() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-twice", &["search"], json!({}))).await;
        let name = run.name;
        let wi = run.step().await;
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{name}: {body}");
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 409, "{name}: {body}");
        assert_eq!(run.completions("think").await.len(), 1, "{name}");
    }
}

/// A worker model step completes under the lease it was claimed with only:
/// no fence, or a zero one, is a 409 and records nothing.
#[tokio::test]
async fn a_result_without_a_fence_is_409() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-unfenced", &["search"], json!({}))).await;
        let name = run.name;
        let wi = run.step().await;
        let before = run.events().await.len();
        let mut unfenced = result_of(&wi, answer());
        unfenced.as_object_mut().unwrap().remove("lease_fence");
        let mut zero = result_of(&wi, answer());
        zero["lease_fence"] = json!(0);
        for (what, body) in [("no fence", unfenced), ("a zero fence", zero)] {
            let (st, answer) = run.complete(&wi, body).await;
            assert_eq!(st, 409, "{name}: {what}: {answer}");
            run.unchanged(&wi, before, what).await;
        }
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{name}: {body}");
    }
}

/// Another principal cannot complete, fail or heartbeat the run's model
/// step, though it knows the id and the fence: each is a 404, as for an
/// item that does not exist.
#[tokio::test]
async fn another_token_cannot_complete_it() {
    let path = std::env::temp_dir().join(format!("jjwm-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}", path.display());
    let sqlite = Arc::new(SqliteBackend::open(&url).await.unwrap());
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (bob, _) = sqlite.create_token("bob", "developer").await.unwrap();
    let backend: Arc<dyn StateBackend> = sqlite;
    let state = support::make_state(backend.clone());
    let (st, body) = send(
        &state,
        "POST",
        "/workflows",
        Some(&alice),
        json!({ "ir": one_step("wf-alice-done", &["search"], json!({})) }),
    )
    .await;
    assert!((200..300).contains(&st), "{st} {body}");
    let (_, body) = send(
        &state,
        "POST",
        "/executions",
        Some(&alice),
        json!({ "workflow_id": "wf-alice-done", "workflow_version": "0.1.0", "input": input() }),
    )
    .await;
    let id = body["execution_id"].as_str().unwrap().to_string();
    let (_, claimed) = send(
        &state,
        "POST",
        "/work-items/claim",
        Some(&alice),
        claim_request("alice-router", &id),
    )
    .await;
    assert_eq!(claimed["claimed"], json!(true), "{claimed}");
    let wi = claimed["work_item"].clone();
    let item = wi["id"].as_str().unwrap();
    let before = backend.get_events(&parse_id(&id)).await.unwrap().len();

    for (what, path, body) in [
        (
            "complete",
            format!("/work-items/{item}/complete"),
            result_of(&wi, answer()),
        ),
        (
            "fail",
            format!("/work-items/{item}/fail"),
            json!({ "error": "no", "lease_fence": wi["lease_fence"] }),
        ),
        (
            "heartbeat",
            format!("/work-items/{item}/heartbeat"),
            json!({ "worker_id": "alice-router", "lease_fence": wi["lease_fence"] }),
        ),
    ] {
        let (st, answer) = send(&state, "POST", &path, Some(&bob), body).await;
        assert_eq!(st, 404, "{what}: {answer}");
    }
    assert_eq!(
        backend.get_events(&parse_id(&id)).await.unwrap().len(),
        before,
        "another principal's requests recorded something"
    );
    let (st, body) = send(
        &state,
        "POST",
        &format!("/work-items/{item}/complete"),
        Some(&alice),
        result_of(&wi, answer()),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    let _ = std::fs::remove_file(path);
}

/// The model the worker reports using is checked against the allow list,
/// the workflow's and a node's own: off it, the node fails for good with
/// the violation on record, nothing is completed, and the answer is a 409.
#[tokio::test]
async fn a_reported_model_off_the_allow_list_fails_the_node() {
    for (level, workflow) in [
        (
            "workflow",
            one_step(
                "wf-used-off-list",
                &["search"],
                json!({ "policy": policy(&["vertex_ai/*"]) }),
            ),
        ),
        ("node", {
            let mut w = one_step("wf-used-off-node-list", &["search"], json!({}));
            w["nodes"]["think"]["policy"] = policy(&["vertex_ai/*"]);
            w
        }),
    ] {
        for tb in support::backends().await {
            let run = start(tb, workflow.clone()).await;
            let name = run.name;
            let wi = run.step().await;
            let (st, body) = run
                .complete(
                    &wi,
                    result_of(&wi, answer_with("model", json!("openai/gpt-4o"))),
                )
                .await;
            assert_eq!(st, 409, "{name}, {level}: {body}");
            assert!(run.completions("think").await.is_empty(), "{name}, {level}");
            assert!(
                run.failed_for_good("think").await.is_some(),
                "{name}, {level}: {:?}",
                run.log().await
            );
            assert_eq!(
                run.violations("think").await,
                vec!["model used is not allowed: openai/gpt-4o".to_string()],
                "{name}, {level}"
            );
            run.tick().await;
            assert_eq!(
                run.status().await,
                WorkflowStatus::Failed,
                "{name}, {level}"
            );
        }
    }
}

/// The tenant's allow list, a ceiling over the IR, applies to the reported
/// model too.
#[tokio::test]
async fn a_tenant_allow_list_applies_to_the_reported_model() {
    for tb in support::backends().await {
        let run = start(tb, one_step("wf-tenant-list", &["search"], json!({}))).await;
        let name = run.name;
        set_tenant_policy(&run, policy(&["vertex_ai/*"])).await;
        let wi = run.step().await;
        let (st, body) = run
            .complete(
                &wi,
                result_of(&wi, answer_with("model", json!("openai/gpt-4o"))),
            )
            .await;
        assert_eq!(st, 409, "{name}: {body}");
        assert!(run.completions("think").await.is_empty(), "{name}");
        assert!(run.failed_for_good("think").await.is_some(), "{name}");
        assert_eq!(
            run.violations("think").await,
            vec!["model used is not allowed: openai/gpt-4o".to_string()],
            "{name}"
        );
    }
}

/// Claim the step of a fresh one-step run offering `tools`, send each body
/// built from the claimed item, and expect `status` and nothing recorded for
/// each. Then the claimed item and the run, for a follow-up.
async fn refused_each(
    tb: TestBackend,
    workflow_id: &str,
    tools: &[&str],
    status: u16,
    bodies: Vec<(&str, Value)>,
) -> (Run, Value) {
    let run = start(tb, one_step(workflow_id, tools, json!({}))).await;
    let wi = run.step().await;
    let before = run.events().await.len();
    for (what, model_result) in bodies {
        let (st, answer) = run.complete(&wi, result_of(&wi, model_result)).await;
        assert_eq!(st, status, "{}: {what}: {answer}", run.name);
        run.unchanged(&wi, before, what).await;
    }
    (run, wi)
}

fn call(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name, "arguments": {} })
}

/// A call naming a tool the node did not offer is refused, so the engine
/// never dispatches a tool the step was not given.
#[tokio::test]
async fn tool_calls_for_unoffered_tools_are_422() {
    for tb in support::backends().await {
        let (run, wi) = refused_each(
            tb,
            "wf-unoffered",
            &["search"],
            422,
            vec![
                (
                    "a tool the node does not offer",
                    answer_with("tool_calls", json!([call("c1", "delete_records")])),
                ),
                (
                    "one offered call and one not",
                    answer_with(
                        "tool_calls",
                        json!([call("c1", "search"), call("c2", "delete_records")]),
                    ),
                ),
            ],
        )
        .await;
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{}: {body}", run.name);
    }
    for tb in support::backends().await {
        refused_each(
            tb,
            "wf-no-tools",
            &[],
            422,
            vec![("a call on a node with no tools", answer())],
        )
        .await;
    }
}

/// Every call needs an id of its own: the results are matched back by it.
#[tokio::test]
async fn repeated_or_empty_call_ids_are_422() {
    for tb in support::backends().await {
        refused_each(
            tb,
            "wf-call-ids",
            &["search"],
            422,
            vec![
                (
                    "a repeated id",
                    answer_with(
                        "tool_calls",
                        json!([call("c1", "search"), call("c1", "search")]),
                    ),
                ),
                (
                    "an empty id",
                    answer_with("tool_calls", json!([call("", "search")])),
                ),
            ],
        )
        .await;
    }
}

/// `finish_reason` and the calls must agree: `tool_calls` with none, or
/// calls with another reason, is refused.
#[tokio::test]
async fn tool_calls_finish_without_calls_is_422() {
    for tb in support::backends().await {
        refused_each(
            tb,
            "wf-finish",
            &["search"],
            422,
            vec![
                (
                    "tool_calls with no calls",
                    answer_with("tool_calls", json!([])),
                ),
                (
                    "calls that finished with stop",
                    answer_with("finish_reason", json!("stop")),
                ),
            ],
        )
        .await;
    }
}

/// At most 256 calls in one answer, as a dispatch node reads at most 256.
#[tokio::test]
async fn more_than_256_calls_is_422() {
    let calls = |n: usize| -> Value {
        (0..n)
            .map(|i| call(&format!("c{i}"), "search"))
            .collect::<Vec<_>>()
            .into()
    };
    for tb in support::backends().await {
        let (run, wi) = refused_each(
            tb,
            "wf-many-calls",
            &["search"],
            422,
            vec![("257 calls", answer_with("tool_calls", calls(257)))],
        )
        .await;
        let (st, body) = run
            .complete(&wi, result_of(&wi, answer_with("tool_calls", calls(256))))
            .await;
        assert_eq!(st, 200, "{}: 256 calls: {body}", run.name);
    }
}

/// The reported model id is recorded and checked against allow lists, so
/// it must be one: not empty, at most 256 characters, no control or
/// invisible format character.
#[tokio::test]
async fn a_bad_model_id_is_422() {
    for tb in support::backends().await {
        let (run, wi) = refused_each(
            tb,
            "wf-model-id",
            &["search"],
            422,
            vec![
                ("an empty model", answer_with("model", json!(""))),
                (
                    "257 characters",
                    answer_with("model", json!("m".repeat(257))),
                ),
                (
                    "a newline",
                    answer_with("model", json!("vertex_ai/gemini\nopenai/gpt-4o")),
                ),
                (
                    "a right-to-left override",
                    answer_with("model", json!("vertex_ai/gemini\u{202E}")),
                ),
            ],
        )
        .await;
        let (st, body) = run
            .complete(
                &wi,
                result_of(&wi, answer_with("model", json!("m".repeat(256)))),
            )
            .await;
        assert_eq!(st, 200, "{}: 256 characters: {body}", run.name);
    }
}

/// Tokens are what the caps count, so both counts are required.
#[tokio::test]
async fn tokens_missing_is_400() {
    for tb in support::backends().await {
        let (run, wi) = refused_each(
            tb,
            "wf-tokens",
            &["search"],
            400,
            vec![
                ("no input tokens", answer_without(&["input_tokens"])),
                ("no output tokens", answer_without(&["output_tokens"])),
                (
                    "no tokens",
                    answer_without(&["input_tokens", "output_tokens"]),
                ),
            ],
        )
        .await;
        let (st, body) = run.complete(&wi, result_of(&wi, answer())).await;
        assert_eq!(st, 200, "{}: {body}", run.name);
    }
}

/// A cost must be a number from 0 to 10,000, as for any step. Without a
/// dollar cap a bad one is refused and nothing is recorded. Under one, the
/// step may have paid and its spend cannot be counted, so it fails for good.
#[tokio::test]
async fn a_cost_that_cannot_be_counted_is_400() {
    for tb in support::backends().await {
        refused_each(
            tb,
            "wf-bad-cost",
            &["search"],
            400,
            vec![
                ("a negative cost", answer_with("cost_usd", json!(-0.01))),
                (
                    "a cost over the ceiling",
                    answer_with("cost_usd", json!(10_000.01)),
                ),
            ],
        )
        .await;
    }
    for tb in support::backends().await {
        let run = start(
            tb,
            one_step(
                "wf-bad-cost-capped",
                &["search"],
                json!({ "cost_budget_usd": 1.0 }),
            ),
        )
        .await;
        let name = run.name;
        let wi = run.step().await;
        let (st, body) = run
            .complete(&wi, result_of(&wi, answer_with("cost_usd", json!(-1.0))))
            .await;
        assert_eq!(st, 400, "{name}: {body}");
        assert!(run.completions("think").await.is_empty(), "{name}");
        assert!(
            run.failed_for_good("think").await.is_some(),
            "{name}: {:?}",
            run.log().await
        );
    }
}

/// Under a dollar cap, an answer that reports no cost is recorded, and the
/// next pass fails the run: its spend cannot be counted.
#[tokio::test]
async fn cost_missing_under_a_dollar_cap_fails_the_run() {
    for tb in support::backends().await {
        let run = start(
            tb,
            ir(
                "wf-no-cost",
                &[("think", worker_model(&[])), ("after", python_fn("after"))],
                json!({ "cost_budget_usd": 1.0 }),
            ),
        )
        .await;
        let name = run.name;
        let wi = run.step().await;
        let (st, body) = run
            .complete(&wi, result_of(&wi, text_answer(120, 30)))
            .await;
        assert_eq!(st, 200, "{name}: {body}");
        run.tick().await;
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
        assert_eq!(
            run.trips().await,
            vec![json!({
                "type": "cost_budget_exceeded", "node_id": "think",
                "limit_usd": 1.0, "current_usd": 0.0, "reason": "cost_not_reported"
            })],
            "{name}"
        );
        assert!(
            !run.scheduled().await.iter().any(|(n, _)| n == "after"),
            "{name}: {:?}",
            run.log().await
        );
    }
}

/// A tool call the worker's model asks for is dispatched like any other: a
/// blocked one is refused by the dispatch guard and never handed to a
/// worker.
#[tokio::test]
async fn a_tool_call_from_a_worker_model_step_still_passes_the_dispatch_guard() {
    for tb in support::backends().await {
        let mut dispatch = json!({
            "type": "python_fn",
            "module": "jamjet.agents.tool_runtime",
            "function": "dispatch_tool_calls",
            "output_schema": "",
            "agent_tool_dispatch": true
        });
        dispatch["claim"] = json!("by_execution");
        let run = start(
            tb,
            ir(
                "wf-wire",
                &[
                    ("__model_0__", worker_model(&["search", "send_wire"])),
                    ("__tools_0__", dispatch),
                ],
                json!({
                    "policy": { "blocked_tools": ["send_wire"], "require_approval_for": [], "model_allowlist": [] }
                }),
            ),
        )
        .await;
        let name = run.name;
        let wi = run.step().await;
        let wire = answer_with(
            "tool_calls",
            json!([ { "id": "c1", "name": "send_wire", "arguments": { "to": "acct-1", "amount": 100 } } ]),
        );
        let (st, body) = run.complete(&wi, result_of(&wi, wire)).await;
        assert_eq!(st, 200, "{name}: {body}");
        run.tick().await;
        let (_, body) = run
            .post(
                "/work-items/claim",
                json!({ "worker_id": "router", "queue_types": ["python_tool"], "execution_id": run.id }),
            )
            .await;
        assert_eq!(
            body,
            json!({ "claimed": false, "reason": "blocked" }),
            "{name}: {:?}",
            run.log().await
        );
        let violations = run.violations("__tools_0__").await;
        assert!(
            violations.iter().any(|r| r.contains("send_wire")),
            "{name}: {violations:?}"
        );
    }
}

/// `think -> after` under `total_tokens: 100`. The worker reports 150
/// tokens: the step is recorded, and the next pass fails the run before
/// `after` is dispatched.
#[tokio::test]
async fn a_worker_model_step_past_the_cap_fails_the_run_next_pass() {
    for tb in support::backends().await {
        let run = start(
            tb,
            ir(
                "wf-past-cap",
                &[("think", worker_model(&[])), ("after", python_fn("after"))],
                json!({ "token_budget": { "total_tokens": 100 } }),
            ),
        )
        .await;
        let name = run.name;
        let wi = run.step().await;
        let (st, body) = run
            .complete(&wi, result_of(&wi, text_answer(120, 30)))
            .await;
        assert_eq!(st, 200, "{name}: {body}");
        assert_eq!(run.completions("think").await.len(), 1, "{name}");
        run.tick().await;
        assert_eq!(run.status().await, WorkflowStatus::Failed, "{name}");
        assert_eq!(
            run.trips().await,
            vec![json!({
                "type": "token_budget_exceeded", "node_id": "think",
                "kind": "total_tokens", "limit": 100, "current": 150
            })],
            "{name}"
        );
        assert!(
            !run.scheduled().await.iter().any(|(n, _)| n == "after"),
            "{name}: {:?}",
            run.log().await
        );
    }
}
