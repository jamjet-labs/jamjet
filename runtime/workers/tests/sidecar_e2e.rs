//! End-to-end test: the durable Model node routes through the sidecar seam.
//!
//! Proves that when `ModelRegistry` is configured with a `SidecarModelAdapter`,
//! `ModelNodeExecutor` routes calls through it and surfaces the sidecar's token
//! metering in `ExecutionResult`. This is a node-level test (not registry-level)
//! — the full `execute()` path runs, identical to the durable worker call path.
//!
//! The tests after it run whole steps through a worker and the scheduler: the
//! sidecar's cost reaching the step's `NodeCompleted` and a run's dollar cap,
//! and a cost the engine cannot count failing the step.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_models::{ModelRegistry, SidecarModelAdapter};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, WorkItem, WorkflowDefinition};
use jamjet_state::{EventKind, InMemoryBackend, DEFAULT_TENANT};
use jamjet_worker::{ModelNodeExecutor, NodeExecutor, Worker};
use serde_json::{json, Value};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal work item that drives a Model node with a plain text prompt.
fn make_model_item() -> WorkItem {
    WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: ExecutionId::new(),
        node_id: "model-1".into(),
        queue_type: "model".into(),
        payload: serde_json::json!({
            "model": "anthropic/claude-sonnet-4-6",
            "prompt": "say hi",
        }),
        attempt: 1,
        max_attempts: 3,
        created_at: chrono::Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    }
}

/// The durable Model node routes through the sidecar and carries its metering.
///
/// Wires `SidecarModelAdapter` as the registry default, drives
/// `ModelNodeExecutor::execute()`, and asserts that token counts and content
/// from the canned sidecar response appear unchanged in the `ExecutionResult`.
#[tokio::test]
async fn model_node_routes_through_sidecar_seam() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{
                "message": {"content": "hi from seam", "role": "assistant"},
                "input_tokens": 11,
                "output_tokens": 7,
                "cost_usd": 0.002,
                "model": "anthropic/claude-sonnet-4-6",
                "finish_reason": "stop"
            }"#,
        ))
        .mount(&server)
        .await;

    // Wire the registry so every call goes to the sidecar.
    let registry = Arc::new(
        ModelRegistry::new()
            .register(Arc::new(SidecarModelAdapter::new(server.uri())))
            .with_default("sidecar"),
    );

    let executor = ModelNodeExecutor::new(registry);
    let item = make_model_item();

    let result = executor
        .execute(&item)
        .await
        .expect("ModelNodeExecutor must succeed against the mock sidecar");

    // Token metering from the sidecar must flow through unmodified.
    assert_eq!(
        result.input_tokens,
        Some(11),
        "input_tokens must match the sidecar response"
    );
    assert_eq!(
        result.output_tokens,
        Some(7),
        "output_tokens must match the sidecar response"
    );
    assert_eq!(
        result.finish_reason.as_deref(),
        Some("stop"),
        "finish_reason must propagate"
    );

    // Content surfaces in the ExecutionResult output object.
    assert_eq!(
        result.output["content"].as_str(),
        Some("hi from seam"),
        "content must be the sidecar's response verbatim"
    );

    // Telemetry fields must be populated.
    assert_eq!(
        result.gen_ai_model.as_deref(),
        Some("anthropic/claude-sonnet-4-6"),
        "model name must propagate to telemetry"
    );
    // Regression guard for the prefix-tolerant gen_ai_system fix:
    // "anthropic/claude-sonnet-4-6" must classify as "anthropic", not "unknown".
    assert_eq!(
        result.gen_ai_system,
        Some("anthropic".to_string()),
        "gen_ai_system must classify provider-prefixed model names correctly"
    );
}

// ── The dollar cap, in process ───────────────────────────────────────────────

/// The sidecar's answer to every call: a few tokens and `cost_usd: 0.6`.
const PRICED_ANSWER: &str = r#"{
    "message": {"content": "ok", "role": "assistant"},
    "input_tokens": 3,
    "output_tokens": 2,
    "cost_usd": 0.6,
    "model": "anthropic/claude-sonnet-4-6",
    "finish_reason": "stop"
}"#;

/// `m1 -> m2 -> m3 -> end`, three model steps, under `cost_budget_usd: 1.0`.
fn priced_ir() -> Value {
    steps_ir(
        "wf-priced",
        &["m1", "m2", "m3"],
        json!({ "cost_budget_usd": 1.0 }),
    )
}

/// `ids[0] -> ids[1] -> ... -> end`, every node a model step, with the keys
/// of `extra` set on the workflow.
fn steps_ir(workflow_id: &str, ids: &[&str], extra: Value) -> Value {
    let step = |id: &str| {
        json!({
            "id": id,
            "kind": {
                "type": "model",
                "model_ref": "anthropic/claude-sonnet-4-6",
                "prompt_ref": "say hi",
                "output_schema": ""
            },
            "retry_policy": null,
            "node_timeout_secs": null,
            "description": null,
            "labels": {}
        })
    };
    let nodes: serde_json::Map<String, Value> =
        ids.iter().map(|id| (id.to_string(), step(id))).collect();
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
    for (key, value) in extra.as_object().unwrap() {
        ir[key] = value.clone();
    }
    ir
}

/// Store `ir` and start one run of it as `POST /executions` does: the run,
/// its first two events and the start node's item, in one write.
async fn start_run(backend: &dyn StateBackend, ir: Value) -> ExecutionId {
    let now = chrono::Utc::now();
    let workflow_id = ir["workflow_id"].as_str().unwrap().to_string();
    let version = ir["version"].as_str().unwrap().to_string();
    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: workflow_id.clone(),
            version: version.clone(),
            ir: ir.clone(),
            created_at: now,
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .unwrap();
    let parsed: jamjet_ir::WorkflowIr = serde_json::from_value(ir).unwrap();
    let start = parsed.start_node.clone();
    let start_def = &parsed.nodes[&start];
    let queue = start_def.kind.queue_type().as_str().to_string();
    let execution_id = ExecutionId::new();
    backend
        .start_execution_atomic(
            WorkflowExecution {
                execution_id: execution_id.clone(),
                workflow_id: workflow_id.clone(),
                workflow_version: version.clone(),
                status: WorkflowStatus::Running,
                initial_input: json!({}),
                current_state: json!({}),
                started_at: now,
                updated_at: now,
                completed_at: None,
                session_type: None,
                parent_execution_id: None,
                segment_number: 0,
            },
            EventKind::WorkflowStarted {
                workflow_id: workflow_id.clone(),
                workflow_version: version.clone(),
                initial_input: json!({}),
                initiated_by: None,
                run_key: None,
            },
            EventKind::NodeScheduled {
                node_id: start.clone(),
                queue_type: queue.clone(),
            },
            WorkItem {
                id: uuid::Uuid::new_v4(),
                execution_id: execution_id.clone(),
                node_id: start.clone(),
                queue_type: queue,
                payload: jamjet_scheduler::build_work_item_payload(
                    &parsed,
                    &start,
                    start_def,
                    &serde_json::Map::new(),
                    &workflow_id,
                    &version,
                ),
                attempt: 0,
                max_attempts: jamjet_scheduler::max_attempts_for(start_def.retry_policy.as_deref()),
                created_at: now,
                lease_expires_at: None,
                worker_id: None,
                lease_fence: 0,
                tenant_id: DEFAULT_TENANT.into(),
            },
        )
        .await
        .unwrap();
    execution_id
}

/// Three model steps run in process through the sidecar, which prices each
/// call at $0.60, under a $1.00 cap. The second call takes the run to $1.20,
/// past the cap: the run must fail there and the third step never run.
///
/// The adapter drops the sidecar's `cost_usd` and the worker records no
/// cost, so the cap never trips and the run completes after three calls.
#[tokio::test]
async fn the_in_process_dollar_cap_trips_on_sidecar_cost() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_string(PRICED_ANSWER))
        .mount(&server)
        .await;
    let registry = Arc::new(
        ModelRegistry::new()
            .register(Arc::new(SidecarModelAdapter::new(server.uri())))
            .with_default("sidecar"),
    );

    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let eid = start_run(backend.as_ref(), priced_ir()).await;
    let worker = Worker::new("w".into(), backend.clone(), vec!["model".into()])
        .register_executor("model", Arc::new(ModelNodeExecutor::new(registry)));
    let scheduler = Scheduler::new(backend.clone());
    for _ in 0..12 {
        scheduler.tick().await.expect("scheduler tick");
        let run = backend.get_execution(&eid).await.unwrap().unwrap();
        if run.status.is_terminal() {
            break;
        }
        let _ = worker.poll_and_execute().await;
    }

    let events: Vec<EventKind> = backend
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    let log: Vec<String> = events
        .iter()
        .map(|k| {
            let v = serde_json::to_value(k).unwrap();
            let kind = v["type"].as_str().unwrap_or("?").to_string();
            match v["node_id"].as_str() {
                Some(node) => format!("{kind}({node})"),
                None => kind,
            }
        })
        .collect();
    let trips: Vec<Value> = events
        .iter()
        .filter(|k| matches!(k, EventKind::CostBudgetExceeded { .. }))
        .map(|k| serde_json::to_value(k).unwrap())
        .collect();
    assert_eq!(
        trips,
        vec![json!({
            "type": "cost_budget_exceeded",
            "node_id": "m2",
            "limit_usd": 1.0,
            "current_usd": 1.2
        })],
        "the dollar cap must trip on the sidecar's cost; the log was {log:?}"
    );
    let costs: Vec<(String, Option<f64>)> = events
        .iter()
        .filter_map(|k| match k {
            EventKind::NodeCompleted {
                node_id, cost_usd, ..
            } => Some((node_id.clone(), *cost_usd)),
            _ => None,
        })
        .collect();
    assert_eq!(
        costs,
        vec![("m1".to_string(), Some(0.6)), ("m2".to_string(), Some(0.6))],
        "each step records the sidecar's cost"
    );
    let run = backend.get_execution(&eid).await.unwrap().unwrap();
    assert_eq!(run.status, WorkflowStatus::Failed, "{log:?}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        2,
        "no model call after the cap is passed"
    );
}

// ── The sidecar's cost, in process ───────────────────────────────────────────

/// A sidecar answer with a few tokens and `cost` spliced in after them: a
/// JSON fragment such as `, "cost_usd": 0.0123`, or nothing.
fn answer_with(cost: &str) -> String {
    format!(
        r#"{{"message": {{"content": "ok", "role": "assistant"}},
            "input_tokens": 3, "output_tokens": 2{cost},
            "model": "anthropic/claude-sonnet-4-6", "finish_reason": "stop"}}"#
    )
}

/// One run of `m1 -> end`, no cap, in process through a sidecar that gives
/// `answer` to every call. Returns the run's log and its status. The step
/// calls the sidecar once, whatever it answers.
async fn one_step(answer: String) -> (Vec<EventKind>, WorkflowStatus) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/complete"))
        .respond_with(ResponseTemplate::new(200).set_body_string(answer))
        .mount(&server)
        .await;
    let registry = Arc::new(
        ModelRegistry::new()
            .register(Arc::new(SidecarModelAdapter::new(server.uri())))
            .with_default("sidecar"),
    );
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let eid = start_run(backend.as_ref(), steps_ir("wf-cost", &["m1"], json!({}))).await;
    let worker = Worker::new("w".into(), backend.clone(), vec!["model".into()])
        .register_executor("model", Arc::new(ModelNodeExecutor::new(registry)));
    let scheduler = Scheduler::new(backend.clone());
    for _ in 0..6 {
        scheduler.tick().await.expect("scheduler tick");
        if backend
            .get_execution(&eid)
            .await
            .unwrap()
            .unwrap()
            .status
            .is_terminal()
        {
            break;
        }
        let _ = worker.poll_and_execute().await;
    }
    let events: Vec<EventKind> = backend
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    let status = backend.get_execution(&eid).await.unwrap().unwrap().status;
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the step calls the sidecar once"
    );
    (events, status)
}

/// Each `NodeCompleted` in `events`, as its node and the event's JSON.
fn completions(events: &[EventKind]) -> Vec<(String, Value)> {
    events
        .iter()
        .filter_map(|k| match k {
            EventKind::NodeCompleted { node_id, .. } => {
                Some((node_id.clone(), serde_json::to_value(k).unwrap()))
            }
            _ => None,
        })
        .collect()
}

/// Each `NodeFailed` in `events`: node, error, retryable.
fn failures(events: &[EventKind]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|k| match k {
            EventKind::NodeFailed {
                node_id,
                error,
                retryable,
                ..
            } => Some((node_id.clone(), error.clone(), *retryable)),
            _ => None,
        })
        .collect()
}

/// The sidecar's `cost_usd` is recorded on the step's `NodeCompleted`, where
/// a run's dollar cap counts it. It used to be dropped by the adapter.
#[tokio::test]
async fn the_sidecar_cost_reaches_node_completed() {
    let (events, status) = one_step(answer_with(r#", "cost_usd": 0.0123"#)).await;
    let done = completions(&events);
    assert_eq!(done.len(), 1, "{events:?}");
    assert_eq!(done[0].0, "m1");
    assert_eq!(done[0].1["cost_usd"], json!(0.0123), "{}", done[0].1);
    assert!(
        matches!(
            events.iter().find(|k| matches!(k, EventKind::NodeCompleted { .. })),
            Some(EventKind::NodeCompleted { cost_usd: Some(c), .. }) if *c == 0.0123
        ),
        "{events:?}"
    );
    assert_eq!(status, WorkflowStatus::Completed);
}

/// A cost the engine cannot count is not dropped and not read as no cost:
/// a negative one, or one sent as a string, fails the step for good, and
/// the run with it.
#[tokio::test]
async fn a_sidecar_cost_that_is_negative_or_a_string_fails_the_node() {
    for (cost, why) in [
        (r#", "cost_usd": -0.5"#, "-0.5 is outside 0 to 10000"),
        (r#", "cost_usd": "0.01""#, "a string, not a number"),
    ] {
        let (events, status) = one_step(answer_with(cost)).await;
        assert_eq!(
            failures(&events),
            vec![(
                "m1".to_string(),
                format!("Model call failed: serialization error: sidecar cost_usd invalid: {why}"),
                false
            )],
            "{cost}: {events:?}"
        );
        assert!(completions(&events).is_empty(), "{cost}: {events:?}");
        assert_eq!(status, WorkflowStatus::Failed, "{cost}");
    }
}

/// A sidecar that sends no cost, or a null one, records no cost: `None` on
/// the event, not a zero, so a dollar cap reads the step as unpriced.
#[tokio::test]
async fn a_sidecar_without_cost_records_none() {
    for cost in ["", r#", "cost_usd": null"#] {
        let (events, status) = one_step(answer_with(cost)).await;
        let recorded: Vec<(String, Option<f64>)> = events
            .iter()
            .filter_map(|k| match k {
                EventKind::NodeCompleted {
                    node_id, cost_usd, ..
                } => Some((node_id.clone(), *cost_usd)),
                _ => None,
            })
            .collect();
        assert_eq!(recorded, vec![("m1".to_string(), None)], "{cost:?}");
        let done = completions(&events);
        assert_eq!(done[0].1.get("cost_usd"), None, "{cost:?}: {}", done[0].1);
        assert_eq!(status, WorkflowStatus::Completed, "{cost:?}");
    }
}

/// A cost in another currency is not a cost the engine can count in USD:
/// the step fails for good.
#[tokio::test]
async fn a_sidecar_cost_in_eur_fails_the_node() {
    let (events, status) =
        one_step(answer_with(r#", "cost_usd": 0.01, "cost_currency": "EUR""#)).await;
    assert_eq!(
        failures(&events),
        vec![(
            "m1".to_string(),
            "Model call failed: serialization error: sidecar cost_usd invalid: \
             cost_currency must be \"USD\""
                .to_string(),
            false
        )],
        "{events:?}"
    );
    assert!(completions(&events).is_empty(), "{events:?}");
    assert_eq!(status, WorkflowStatus::Failed);
}

/// The sidecar takes a cost up to the same per-call ceiling the usage fold
/// counts, so a cost the adapter lets through is never one the fold reads as
/// unknown.
#[test]
fn the_sidecar_ceiling_is_the_fold_ceiling() {
    assert_eq!(
        jamjet_models::adapter::MAX_REPORTED_COST_USD,
        jamjet_state::usage::MAX_REPORTED_COST_USD
    );
    assert_eq!(
        jamjet_state::usage::cost_to_nanos(jamjet_models::adapter::MAX_REPORTED_COST_USD),
        Some(10_000_000_000_000)
    );
}
