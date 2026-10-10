//! A code-backed node can be approval-gated and blocked.
//!
//! The reviewer's demonstration (review I3): a `python_fn` node with node
//! policy `require_approval_for: ["*"]` was handed out on `python_tool` with no
//! approval, because function kinds had no tool name for a pattern to match.
//! A developer could call any gated tool's code through a python_fn node.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

fn ir(workflow_id: &str, kind: Value, policy: Value) -> Value {
    json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "payments.transfer",
        "nodes": {
            "payments.transfer": {
                "id": "payments.transfer",
                "kind": kind,
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": policy
            }
        },
        "edges": [ { "from": "payments.transfer", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

fn python_fn() -> Value {
    json!({ "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": "" })
}

fn java_fn() -> Value {
    json!({ "type": "java_fn", "class_name": "com.acme.Payments", "method": "transfer", "output_schema": "" })
}

fn external_fn() -> Value {
    json!({ "type": "external_fn", "queue": "pas-control", "handler": "transfer", "output_schema": "" })
}

fn policy(approve: &[&str], block: &[&str]) -> Value {
    json!({ "blocked_tools": block, "require_approval_for": approve, "model_allowlist": [] })
}

async fn send(app: &axum::Router, uri: &str, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
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

/// Register and start one run, then claim from every queue a function node
/// could be on. Returns the claim answer and the run's events.
async fn start_and_claim(kind: Value, policy: Value) -> (Value, Vec<jamjet_state::Event>) {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (st, body) = send(
        &app,
        "/workflows",
        &alice,
        json!({ "ir": ir("wf-fn", kind, policy) }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = send(
        &app,
        "/executions",
        &alice,
        json!({ "workflow_id": "wf-fn", "workflow_version": "0.1.0", "input": { "amount": 1_000_000 } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let eid = body["execution_id"].as_str().unwrap();
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(eid.strip_prefix("exec_").unwrap_or(eid)).unwrap(),
    );
    let (st, claimed) = send(
        &app,
        "/work-items/claim",
        &alice,
        json!({ "worker_id": "w", "queue_types": ["python_tool", "java_tool", "pas-control", "general"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{claimed}");
    (claimed, backend.get_events(&eid).await.unwrap())
}

fn requests(events: &[jamjet_state::Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { tool_name, .. } => Some(tool_name.clone()),
            _ => None,
        })
        .collect()
}

fn violations(events: &[jamjet_state::Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::PolicyViolation { .. }))
        .count()
}

/// The reviewer's demonstration, for each code-backed kind.
#[tokio::test]
async fn a_star_policy_holds_every_function_kind() {
    for (kind, handler) in [
        (python_fn(), "myapp.payments:transfer"),
        (java_fn(), "com.acme.Payments/transfer"),
        (external_fn(), "pas-control/transfer"),
    ] {
        let (claimed, events) = start_and_claim(kind.clone(), policy(&["*"], &[])).await;
        assert_eq!(
            claimed,
            json!({ "claimed": false }),
            "{kind} was handed out"
        );
        assert_eq!(requests(&events), vec![handler.to_string()], "{kind}");
    }
}

#[tokio::test]
async fn a_function_node_is_gated_by_its_handler_or_its_node_id() {
    for pattern in ["myapp.payments:*", "payments.*"] {
        let (claimed, events) = start_and_claim(python_fn(), policy(&[pattern], &[])).await;
        assert_eq!(claimed, json!({ "claimed": false }), "{pattern}");
        assert_eq!(requests(&events).len(), 1, "{pattern}");
    }
}

#[tokio::test]
async fn a_function_node_can_be_blocked() {
    let (claimed, events) = start_and_claim(external_fn(), policy(&[], &["pas-control/*"])).await;
    assert_eq!(claimed, json!({ "claimed": false }));
    assert_eq!(violations(&events), 1);
}

/// Control: a policy that names something else does not touch the node.
#[tokio::test]
async fn an_unrelated_policy_leaves_a_function_node_alone() {
    let (claimed, _) = start_and_claim(python_fn(), policy(&["search.*"], &["shell.*"])).await;
    assert_eq!(claimed["claimed"], true, "{claimed}");
    assert_eq!(
        claimed["work_item"]["payload"]["input"]["amount"],
        1_000_000
    );
}
