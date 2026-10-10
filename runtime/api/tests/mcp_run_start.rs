//! The MCP bridge starts a run exactly as `POST /executions` does.
//!
//! `jamjet_run_workflow` had its own copy of the start path: the start item
//! went on queue `general` with a bare `{workflow_id, workflow_version}`
//! payload, whatever the start node's kind, and nothing validated the IR's
//! start node. The REST path builds the payload with the scheduler's builder
//! and routes it by node kind. A python_fn start node started over MCP never
//! reached a python worker, and since an approval binds the dispatched
//! payload, the two paths could not even agree on what a reviewer approves.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

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

fn parse_eid(s: &str) -> ExecutionId {
    ExecutionId(uuid::Uuid::parse_str(s.strip_prefix("exec_").unwrap_or(s)).unwrap())
}

async fn mcp_run(app: &axum::Router, token: &str, workflow_id: &str) -> Value {
    let (st, resp) = send(
        app,
        "/mcp",
        token,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_run_workflow", "arguments": {
                "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": { "amount": 10 },
            } },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    resp
}

#[tokio::test]
async fn an_mcp_run_enqueues_the_same_start_item_as_rest() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, false);
    let (alice, alice_token) = backend.create_token("alice", "developer").await.unwrap();
    let (st, body) = send(
        &app,
        "/workflows",
        &alice,
        json!({ "ir": support::python_fn_start_ir("wf-mcp") }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");

    let resp = mcp_run(&app, &alice, "wf-mcp").await;
    assert_ne!(resp["result"]["isError"], true, "{resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let mcp_eid = parse_eid(
        serde_json::from_str::<Value>(text).unwrap()["execution_id"]
            .as_str()
            .unwrap(),
    );
    let (st, body) = send(
        &app,
        "/executions",
        &alice,
        json!({ "workflow_id": "wf-mcp", "workflow_version": "0.1.0", "input": { "amount": 10 } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let rest_eid = parse_eid(body["execution_id"].as_str().unwrap());

    let mut items = Vec::new();
    while let Some(item) = backend
        .claim_work_item_with_lease("probe", &["python_tool", "general"], None)
        .await
        .unwrap()
    {
        items.push(item);
    }
    let of = |eid: &ExecutionId| {
        items
            .iter()
            .find(|i| &i.execution_id == eid)
            .unwrap_or_else(|| panic!("no start item for {eid}: {items:?}"))
            .clone()
    };
    let (mcp_item, rest_item) = (of(&mcp_eid), of(&rest_eid));
    assert_eq!(mcp_item.queue_type, "python_tool", "{mcp_item:?}");
    assert_eq!(mcp_item.queue_type, rest_item.queue_type);
    assert_eq!(mcp_item.payload, rest_item.payload);
    assert_eq!(mcp_item.max_attempts, rest_item.max_attempts);

    let events = backend.get_events(&mcp_eid).await.unwrap();
    assert_eq!(
        jamjet_state::approvals::run_initiator(&events),
        Some(jamjet_api::auth::principal_of(&alice_token))
    );
    assert!(events.iter().any(|e| matches!(
        &e.kind,
        EventKind::NodeScheduled { queue_type, .. } if queue_type == "python_tool"
    )));
}

/// The REST path refuses an IR whose start node does not exist; so does MCP.
#[tokio::test]
async fn an_mcp_run_of_an_unknown_workflow_is_an_error() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let resp = mcp_run(&app, &alice, "nope").await;
    assert_eq!(resp["result"]["isError"], true, "{resp}");
}
