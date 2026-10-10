//! The MCP bridge (`POST /mcp`) outside dev mode.
//!
//! The bridge used to be merged into the router with no auth layer in every
//! mode, took `tenant_id` from the tool arguments and recorded every approval as
//! `"mcp-client"`. The published image binds 0.0.0.0, so anyone who could reach
//! the port could approve any pending action in any tenant. These tests pin the
//! production router: Bearer token required, tenant taken from the token, and an
//! argument tenant that disagrees with the token refused.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

fn make_state(backend: Arc<InMemoryBackend>) -> AppState {
    let backend_clone = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone() as Arc<dyn StateBackend>,
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| {
            backend_clone.clone() as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    }
}

/// A running execution with one pending approval on node `gate`.
async fn pending_execution(backend: &Arc<InMemoryBackend>) -> ExecutionId {
    let eid = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
    backend
        .append_event(Event::new(
            eid.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "wf".into(),
                workflow_version: "0.1.0".into(),
                initial_input: json!({}),
                // A principal no test token has, so SoD is provable.
                initiated_by: Some("token:the-initiator".into()),
            },
        ))
        .await
        .unwrap();
    backend
        .append_event(Event::new(
            eid.clone(),
            2,
            EventKind::ToolApprovalRequired {
                node_id: "gate".into(),
                tool_name: "payments.transfer".into(),
                approver: "human".into(),
                context: json!({ "node_id": "gate" }),
            },
        ))
        .await
        .unwrap();
    eid
}

fn approve_call(eid: &ExecutionId, extra: Value) -> Value {
    let mut args = json!({
        "execution_id": eid.to_string(),
        "decision": "approved",
        "node_id": "gate",
    });
    if let (Some(a), Some(e)) = (args.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            a.insert(k.clone(), v.clone());
        }
    }
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "jamjet_approve", "arguments": args },
    })
}

async fn post_mcp(app: axum::Router, token: Option<&str>, body: Value) -> (StatusCode, Value) {
    let mut req = Request::post("/mcp").header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// `result.isError` of a JSON-RPC `tools/call` response.
fn tool_failed(resp: &Value) -> bool {
    resp["result"]["isError"].as_bool().unwrap_or(true)
}

async fn approvals(backend: &Arc<InMemoryBackend>, eid: &ExecutionId) -> Vec<(String, String)> {
    backend
        .get_events(eid)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::ApprovalReceived {
                node_id, user_id, ..
            } => Some((node_id, user_id)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn unauthenticated_bridge_approve_is_401_and_appends_nothing() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (status, _) = post_mcp(app, None, approve_call(&eid, json!({}))).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(approvals(&backend, &eid).await.is_empty());
}

#[tokio::test]
async fn a_forged_token_is_401_and_appends_nothing() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (status, _) = post_mcp(
        app,
        Some("jj_not_a_real_token"),
        approve_call(&eid, json!({})),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(approvals(&backend, &eid).await.is_empty());
}

#[tokio::test]
async fn unauthenticated_tool_listing_is_401_too() {
    // Not just the approve tool: the whole bridge sits behind auth, reads included.
    let backend = Arc::new(InMemoryBackend::new());
    let app = build_router_with_opts(make_state(backend), false);

    let (status, _) = post_mcp(
        app,
        None,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_argument_tenant_that_disagrees_with_the_token_is_refused() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let (token, _) = backend.create_token("alice", "operator").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (status, resp) = post_mcp(
        app,
        Some(&token),
        approve_call(&eid, json!({ "tenant_id": "someone-else" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "JSON-RPC carries the tool error");
    assert!(tool_failed(&resp), "mismatched tenant must fail: {resp}");
    assert!(approvals(&backend, &eid).await.is_empty());
}

#[tokio::test]
async fn an_authenticated_approve_is_accepted_and_names_the_token() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let (token, _) = backend.create_token("alice", "operator").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    // The token's own tenant, spelled out, is not a mismatch.
    let (status, resp) = post_mcp(
        app,
        Some(&token),
        approve_call(&eid, json!({ "tenant_id": "default" })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(!tool_failed(&resp), "approve should succeed: {resp}");
    let recorded = approvals(&backend, &eid).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].1, "alice",
        "the approver is the token, not 'mcp-client'"
    );
}

#[tokio::test]
async fn a_viewer_token_cannot_approve_through_the_bridge() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let (token, _) = backend.create_token("vic", "viewer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (_, resp) = post_mcp(app, Some(&token), approve_call(&eid, json!({}))).await;

    assert!(tool_failed(&resp), "viewer must not approve: {resp}");
    assert!(approvals(&backend, &eid).await.is_empty());
}

#[tokio::test]
async fn a_viewer_token_cannot_start_a_workflow_through_the_bridge() {
    let backend = Arc::new(InMemoryBackend::new());
    let (token, _) = backend.create_token("vic", "viewer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (_, resp) = post_mcp(
        app,
        Some(&token),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_run_workflow",
                        "arguments": { "workflow_id": "wf", "input": {} } },
        }),
    )
    .await;

    assert!(tool_failed(&resp), "viewer must not start runs: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("role"),
        "refused for its role, not for a missing workflow: {text}"
    );
}

#[tokio::test]
async fn dev_mode_bridge_still_works_without_a_token() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let app = build_router_with_opts(make_state(backend.clone()), true);

    let (status, resp) = post_mcp(app, None, approve_call(&eid, json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        !tool_failed(&resp),
        "dev mode approve should succeed: {resp}"
    );
    assert_eq!(approvals(&backend, &eid).await.len(), 1);
}

#[tokio::test]
async fn the_bridge_fails_closed_if_mounted_without_the_auth_layer() {
    // Defence in depth: a production bridge with no token in the request
    // extensions refuses, rather than falling back to anonymous dev behaviour.
    let backend = Arc::new(InMemoryBackend::new());
    let eid = pending_execution(&backend).await;
    let bare = jamjet_api::mcp_bridge::build_mcp_bridge(make_state(backend.clone()), false);

    let (status, resp) = post_mcp(bare, None, approve_call(&eid, json!({}))).await;

    assert_eq!(status, StatusCode::OK);
    assert!(tool_failed(&resp), "no token must mean no approval: {resp}");
    assert!(approvals(&backend, &eid).await.is_empty());
}
