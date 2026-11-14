//! A node nothing can run is refused, never reported as done.
//!
//! Until 2026-11 a worker that claimed a node with no executor completed it
//! with an empty output ("No executor; using stub"), and the server
//! registered executors for three kinds. A `tool` node "succeeded" without
//! running and the run reached `WorkflowCompleted`. Now registration refuses
//! such a workflow (400, naming each node and kind), and a worker that claims
//! one anyway (a version stored before this release) fails it.

use jamjet_scheduler::Scheduler;
use jamjet_state::backend::{StateBackend, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend, DEFAULT_TENANT};
use serde_json::{json, Value};
use std::sync::Arc;

mod support;

fn node(id: &str, kind: Value) -> Value {
    json!({
        "id": id, "kind": kind,
        "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
    })
}

/// A workflow of `nodes` run in the given order, starting at the first.
fn ir(workflow_id: &str, nodes: &[(&str, Value)]) -> Value {
    let mut map = serde_json::Map::new();
    let mut edges = Vec::new();
    for (i, (id, kind)) in nodes.iter().enumerate() {
        map.insert(id.to_string(), node(id, kind.clone()));
        let to = nodes.get(i + 1).map(|(n, _)| *n).unwrap_or("end");
        edges.push(json!({ "from": id, "to": to, "condition": null }));
    }
    json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": nodes[0].0,
        "nodes": map, "edges": edges,
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

fn tool(tool_ref: &str) -> Value {
    json!({ "type": "tool", "tool_ref": tool_ref, "input_mapping": {}, "output_schema": "" })
}

#[tokio::test]
async fn registration_refuses_a_node_kind_nothing_runs() {
    let state = support::make_state(Arc::new(InMemoryBackend::new()));
    let bad = ir(
        "wf-unrunnable",
        &[
            ("search", tool("tools.web_search")),
            (
                "notes",
                json!({ "type": "memory_retrieval", "connector_ref": "kb", "query_expr": "state.q", "output_schema": "" }),
            ),
            ("fanout", json!({ "type": "parallel", "branches": [] })),
        ],
    );
    let (st, body) = support::post(&state, "/workflows", json!({ "ir": bad })).await;
    assert_eq!(st.as_u16(), 400, "{body}");
    let msg = body.to_string();
    for named in [
        "'search' (tool)",
        "'notes' (memory_retrieval)",
        "'fanout' (parallel)",
    ] {
        assert!(msg.contains(named), "names {named}: {msg}");
    }
}

#[tokio::test]
async fn kinds_the_engine_or_an_external_worker_runs_still_register() {
    let state = support::make_state(Arc::new(InMemoryBackend::new()));
    let ok = ir(
        "wf-runnable",
        &[
            (
                "py",
                json!({ "type": "python_fn", "module": "m", "function": "f", "output_schema": "" }),
            ),
            (
                "java",
                json!({ "type": "java_fn", "class_name": "C", "method": "m", "output_schema": "" }),
            ),
            (
                "ext",
                json!({ "type": "external_fn", "queue": "pas-control", "handler": "h", "output_schema": "" }),
            ),
            (
                "think",
                json!({ "type": "model", "model_ref": "m", "prompt_ref": "", "output_schema": "" }),
            ),
            (
                "review",
                json!({ "type": "human_approval", "description": "ok?", "timeout_secs": null, "fallback_node": null }),
            ),
            ("route", json!({ "type": "condition", "branches": [] })),
            ("stop", json!({ "type": "limit_exceeded" })),
        ],
    );
    let (st, body) = support::post(&state, "/workflows", json!({ "ir": ok })).await;
    assert!(st.is_success(), "{st} {body}");
}

/// Store `ir` the way a version registered before this release was stored
/// (no registration check), start a run and drive the server's in-process
/// worker and the scheduler. Returns the run's events.
async fn run_stored(ir: Value, queues: &[&str]) -> Vec<Event> {
    let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
    let workflow_id = ir["workflow_id"].as_str().unwrap().to_string();
    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: workflow_id.clone(),
            version: "0.1.0".into(),
            ir,
            created_at: chrono::Utc::now(),
            tenant_id: DEFAULT_TENANT.into(),
        })
        .await
        .unwrap();
    let state = support::make_state(backend.clone());
    let (st, body) = support::post(
        &state,
        "/executions",
        json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let id = body["execution_id"].as_str().unwrap();
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap(),
    );
    let worker = engine_worker(backend.clone(), queues);
    let scheduler = Scheduler::new(backend.clone());
    for _ in 0..6 {
        let _ = worker.poll_and_execute().await;
        scheduler.tick().await.unwrap();
    }
    backend.get_events(&eid).await.unwrap()
}

/// A worker with exactly the server's in-process executors.
fn engine_worker(backend: Arc<dyn StateBackend>, queues: &[&str]) -> jamjet_worker::Worker {
    jamjet_worker::server_pool(backend, Arc::new(jamjet_models::ModelRegistry::new())).build_worker(
        "engine".into(),
        queues.iter().map(|q| q.to_string()).collect(),
    )
}

fn failed_with<'a>(events: &'a [Event], node: &str) -> Option<&'a str> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::NodeFailed {
            node_id,
            error,
            retryable: false,
            ..
        } if node_id == node => Some(error.as_str()),
        _ => None,
    })
}

#[tokio::test]
async fn a_claimed_node_with_no_executor_fails_instead_of_completing() {
    let events = run_stored(
        ir(
            "wf-stored-tool",
            &[
                ("search", tool("tools.web_search")),
                ("after", tool("tools.after")),
            ],
        ),
        &["general", "tool"],
    )
    .await;
    assert_eq!(
        failed_with(&events, "search"),
        Some("no executor for node kind tool"),
        "{events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. })),
        "nothing completes: {events:#?}"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e.kind, EventKind::WorkflowCompleted { .. })));
}

/// `memory_retrieval` goes to the `retrieval` queue, which nothing polled,
/// so the run waited forever. The embedded pool now claims it and fails it.
#[tokio::test]
async fn a_memory_retrieval_node_fails_instead_of_waiting_forever() {
    let events = run_stored(
        ir(
            "wf-stored-retrieval",
            &[(
                "notes",
                json!({ "type": "memory_retrieval", "connector_ref": "kb", "query_expr": "state.q", "output_schema": "" }),
            )],
        ),
        &["general", "tool", "retrieval"],
    )
    .await;
    assert_eq!(
        failed_with(&events, "notes"),
        Some("no executor for node kind memory_retrieval"),
        "{events:#?}"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })));
}

/// Reaching a strategy's `limit_exceeded` node stops the run as a failure,
/// never as a success.
#[tokio::test]
async fn a_limit_exceeded_node_fails_the_run() {
    let events = run_stored(
        ir(
            "wf-limit",
            &[("__limit_exceeded__", json!({ "type": "limit_exceeded" }))],
        ),
        &["general"],
    )
    .await;
    let error = failed_with(&events, "__limit_exceeded__").unwrap_or_else(|| panic!("{events:#?}"));
    assert!(error.contains("strategy limit reached"), "{error}");
    assert!(events
        .iter()
        .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })));
}
