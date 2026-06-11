//! Validated approval API tests.
//!
//! Tests for POST /executions/:id/approve (validation against pending state)
//! and GET /executions/:id/approvals (listing pending + decided approvals).
//! Uses the in-memory backend + dev_mode router (no auth required).

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
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn make_state() -> AppState {
    let backend = Arc::new(InMemoryBackend::new());
    let backend_clone = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone() as Arc<dyn jamjet_state::StateBackend>,
        backend_for_fn: Arc::new(move |_tenant_id: &jamjet_state::TenantId| {
            backend_clone.clone() as Arc<dyn jamjet_state::StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
    }
}

/// Create a fresh execution in the backend and return its ID string.
async fn create_execution(backend: &Arc<dyn StateBackend>) -> ExecutionId {
    let execution_id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: execution_id.clone(),
            workflow_id: "test-wf".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
        })
        .await
        .expect("create_execution");

    backend
        .append_event(Event::new(
            execution_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "test-wf".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({}),
            },
        ))
        .await
        .expect("append WorkflowStarted");

    execution_id
}

/// Append a `ToolApprovalRequired` for the given node.
async fn seed_approval_required(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
    node_id: &str,
) {
    let seq = backend
        .latest_sequence(execution_id)
        .await
        .expect("latest_sequence")
        + 1;
    backend
        .append_event(Event::new(
            execution_id.clone(),
            seq,
            EventKind::ToolApprovalRequired {
                node_id: node_id.into(),
                tool_name: format!("tool_{node_id}"),
                approver: "human".into(),
                context: serde_json::json!({"action": node_id}),
            },
        ))
        .await
        .expect("append ToolApprovalRequired");
}

async fn body_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn approve_body(decision: &str, node_id: Option<&str>) -> Body {
    let mut map = serde_json::json!({ "decision": decision });
    if let Some(n) = node_id {
        map["node_id"] = serde_json::json!(n);
    }
    Body::from(serde_json::to_vec(&map).unwrap())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn approve_with_no_pending_returns_409() {
    let state = make_state();
    let backend = state.backend.clone();
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    let id_str = execution_id.to_string();

    let resp = app
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", None))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json = body_json(resp.into_body()).await;
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("no pending approval"),
        "expected 'no pending approval' in error, got: {error}"
    );
}

#[tokio::test]
async fn approve_unknown_node_returns_409() {
    let state = make_state();
    let backend = state.backend.clone();
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    let id_str = execution_id.to_string();

    let resp = app
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", Some("zzz")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json = body_json(resp.into_body()).await;
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("no pending approval"),
        "expected 'no pending approval' in error for unknown node, got: {error}"
    );
}

#[tokio::test]
async fn approve_single_pending_infers_node() {
    let state = make_state();
    let backend = state.backend.clone();
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    let id_str = execution_id.to_string();

    let resp = app
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", None))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    assert_eq!(json["node_id"], "a", "inferred node_id must be 'a'");
    assert_eq!(json["accepted"], true);

    // Backend must have an ApprovalReceived for node "a".
    let events = backend.get_events(&execution_id).await.unwrap();
    let has_received = events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::ApprovalReceived { node_id, .. } if node_id == "a"));
    assert!(
        has_received,
        "ApprovalReceived for node 'a' must be in event log"
    );
}

#[tokio::test]
async fn approve_multiple_pending_requires_node_id() {
    let state = make_state();
    let backend = state.backend.clone();
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    seed_approval_required(&backend, &execution_id, "b").await;
    let id_str = execution_id.to_string();

    let resp = app
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", None))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp.into_body()).await;
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("specify node_id"),
        "expected 'specify node_id' in error, got: {error}"
    );
}

#[tokio::test]
async fn double_approve_returns_409() {
    let state = make_state();
    let backend = state.backend.clone();
    let router = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    let id_str = execution_id.to_string();

    // First approval — must succeed.
    let resp1 = router
        .clone()
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", Some("a")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second identical approval — must 409.
    let resp2 = router
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", Some("a")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::CONFLICT);
    let json = body_json(resp2.into_body()).await;
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("no pending approval"),
        "double-approve error must mention 'no pending approval', got: {error}"
    );
}

#[tokio::test]
async fn invalid_decision_returns_400() {
    let state = make_state();
    let backend = state.backend.clone();
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    let id_str = execution_id.to_string();

    let resp = app
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({"decision": "maybe"})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp.into_body()).await;
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("unknown decision") || error.contains("maybe"),
        "expected decision error, got: {error}"
    );
}

#[tokio::test]
async fn list_approvals_shows_pending_then_decided() {
    let state = make_state();
    let backend = state.backend.clone();
    let router = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    seed_approval_required(&backend, &execution_id, "b").await;
    let id_str = execution_id.to_string();

    // GET /executions/:id/approvals — both pending.
    let resp = router
        .clone()
        .oneshot(
            Request::get(format!("/executions/{id_str}/approvals"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp.into_body()).await;
    let pending = json["pending"].as_array().expect("pending must be array");
    assert_eq!(pending.len(), 2, "both nodes must be pending");
    let pending_ids: Vec<&str> = pending
        .iter()
        .map(|p| p["node_id"].as_str().unwrap())
        .collect();
    assert!(pending_ids.contains(&"a"), "node 'a' must be pending");
    assert!(pending_ids.contains(&"b"), "node 'b' must be pending");

    // Each pending entry must have node_id, tool_name, approver.
    for p in pending {
        assert!(p["node_id"].is_string(), "pending entry missing node_id");
        assert!(
            p["tool_name"].is_string(),
            "pending entry missing tool_name"
        );
        assert!(p["approver"].is_string(), "pending entry missing approver");
    }

    // Approve node "a".
    let approve_resp = router
        .clone()
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", Some("a")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(approve_resp.status(), StatusCode::OK);

    // GET again — "a" decided, "b" still pending.
    let resp2 = router
        .oneshot(
            Request::get(format!("/executions/{id_str}/approvals"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp2.status(), StatusCode::OK);
    let json2 = body_json(resp2.into_body()).await;

    let pending2 = json2["pending"].as_array().expect("pending must be array");
    assert_eq!(pending2.len(), 1, "only node 'b' remains pending");
    assert_eq!(pending2[0]["node_id"], "b");

    let decided = json2["decided"].as_array().expect("decided must be array");
    assert_eq!(decided.len(), 1, "node 'a' must be in decided");
    assert_eq!(decided[0]["node_id"], "a");
    assert_eq!(decided[0]["status"], "approved");
}

#[tokio::test]
async fn approve_on_terminal_execution_returns_409() {
    let state = make_state();
    let backend = state.backend.clone();
    let app = build_router_with_opts(state, true);

    let execution_id = create_execution(&backend).await;
    seed_approval_required(&backend, &execution_id, "a").await;
    backend
        .update_execution_status(&execution_id, WorkflowStatus::Cancelled)
        .await
        .expect("cancel execution");
    let id_str = execution_id.to_string();

    let resp = app
        .oneshot(
            Request::post(format!("/executions/{id_str}/approve"))
                .header("content-type", "application/json")
                .body(approve_body("approved", Some("a")))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let json = body_json(resp.into_body()).await;
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("Cancelled"),
        "expected terminal status in error, got: {error}"
    );
}
