//! Who approved, and who is allowed to.
//!
//! `POST /executions/:id/approve` used to record `body.user_id` (default
//! "anonymous") as the approver, never bound to the authenticated token, so any
//! caller could sign a decision with any name. And `reviewer`, the role whose
//! whole purpose is reviewing, could not approve at all. These tests drive the
//! production (auth) router and read the recorded event as JSON, so they pin the
//! wire shape an auditor sees.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

#[derive(Default)]
struct CapturingAudit {
    entries: tokio::sync::Mutex<Vec<AuditLogEntry>>,
}

#[async_trait]
impl AuditBackend for CapturingAudit {
    async fn append(&self, entry: AuditLogEntry) -> Result<(), AuditError> {
        self.entries.lock().await.push(entry);
        Ok(())
    }
    async fn query(&self, _q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(self.entries.lock().await.iter().rev().cloned().collect())
    }
    async fn count(&self, _q: &AuditQuery) -> Result<u64, AuditError> {
        Ok(self.entries.lock().await.len() as u64)
    }
}

struct Env {
    backend: Arc<InMemoryBackend>,
    audit: Arc<CapturingAudit>,
    state: AppState,
}

fn env() -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    let audit = Arc::new(CapturingAudit::default());
    let audit_dyn: Arc<dyn AuditBackend> = audit.clone();
    let backend_clone = backend.clone();
    let state = AppState {
        backend: backend.clone() as Arc<dyn StateBackend>,
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| {
            backend_clone.clone() as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit: audit_dyn.clone(),
        enricher: Arc::new(AuditEnricher::new(audit_dyn)),
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    Env {
        backend,
        audit,
        state,
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
    for (seq, kind) in [
        EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "0.1.0".into(),
            initial_input: json!({}),
            // Started by someone other than any approver in these tests, so
            // separation of duties holds and is provable.
            initiated_by: Some("token:the-initiator".into()),
        },
        EventKind::ToolApprovalRequired {
            node_id: "gate".into(),
            tool_name: "payments.transfer".into(),
            approver: "human".into(),
            context: json!({ "node_id": "gate" }),
        },
    ]
    .into_iter()
    .enumerate()
    {
        backend
            .append_event(Event::new(eid.clone(), seq as i64 + 1, kind))
            .await
            .unwrap();
    }
    eid
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let resp = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Every `ApprovalReceived` for the execution, as its serialized JSON.
async fn approvals(backend: &Arc<InMemoryBackend>, eid: &ExecutionId) -> Vec<Value> {
    backend
        .get_events(eid)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .map(|e| serde_json::to_value(&e.kind).unwrap())
        .collect()
}

#[tokio::test]
async fn the_recorded_approver_is_the_token_not_the_body() {
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, info) = env
        .backend
        .create_token("alice", "developer")
        .await
        .unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, _) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        Some(&token),
        json!({ "decision": "approved", "user_id": "the-ceo" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let recorded = approvals(&env.backend, &eid).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["user_id"], "alice", "body user_id is ignored");
    assert_eq!(recorded[0]["principal"], format!("token:{}", info.id));
}

#[tokio::test]
async fn the_audit_entry_names_the_principal_as_actor() {
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, info) = env
        .backend
        .create_token("alice", "developer")
        .await
        .unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, _) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        Some(&token),
        json!({ "decision": "approved", "user_id": "the-ceo" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let entries = env.audit.entries.lock().await;
    let entry = entries
        .iter()
        .find(|e| e.event_type == "approval_received")
        .expect("approval sealed into the audit log");
    assert_eq!(entry.actor_id, format!("token:{}", info.id));
    assert_eq!(entry.raw_event["principal"], format!("token:{}", info.id));
}

#[tokio::test]
async fn a_reviewer_can_approve() {
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, _) = env.backend.create_token("rita", "reviewer").await.unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, body) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        Some(&token),
        json!({ "decision": "approved" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let recorded = approvals(&env.backend, &eid).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["user_id"], "rita");
}

#[tokio::test]
async fn a_reviewer_still_cannot_start_an_execution() {
    let env = env();
    env.backend
        .store_workflow(WorkflowDefinition {
            workflow_id: "wf".into(),
            version: "0.1.0".into(),
            ir: json!({ "start_node": "a", "nodes": {} }),
            created_at: chrono::Utc::now(),
            tenant_id: "default".into(),
        })
        .await
        .unwrap();
    let (token, _) = env.backend.create_token("rita", "reviewer").await.unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, _) = send(
        app,
        "POST",
        "/executions",
        Some(&token),
        json!({ "workflow_id": "wf", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_reviewer_still_cannot_cancel_an_execution() {
    // The exemption is the approve route only, not "any POST under an execution".
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, _) = env.backend.create_token("rita", "reviewer").await.unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, _) = send(
        app,
        "POST",
        &format!("/executions/{eid}/cancel"),
        Some(&token),
        json!({}),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_viewer_cannot_approve() {
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, _) = env.backend.create_token("vic", "viewer").await.unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, _) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        Some(&token),
        json!({ "decision": "approved" }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(approvals(&env.backend, &eid).await.is_empty());
}

#[tokio::test]
async fn an_unknown_role_cannot_approve() {
    // Unknown roles parse as viewer: fail closed.
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, _) = env.backend.create_token("x", "superuser").await.unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, _) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        Some(&token),
        json!({ "decision": "approved" }),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(approvals(&env.backend, &eid).await.is_empty());
}

#[tokio::test]
async fn a_reviewer_can_approve_through_the_mcp_bridge() {
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let (token, info) = env.backend.create_token("rita", "reviewer").await.unwrap();
    let app = build_router_with_opts(env.state.clone(), false);

    let (status, resp) = send(
        app,
        "POST",
        "/mcp",
        Some(&token),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_approve", "arguments": {
                "execution_id": eid.to_string(), "decision": "approved", "node_id": "gate"
            }},
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let recorded = approvals(&env.backend, &eid).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["user_id"], "rita");
    assert_eq!(recorded[0]["principal"], format!("token:{}", info.id));
}

#[tokio::test]
async fn dev_mode_records_the_body_user_and_no_principal() {
    let env = env();
    let eid = pending_execution(&env.backend).await;
    let app = build_router_with_opts(env.state.clone(), true);

    let (status, _) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        None,
        json!({ "decision": "approved", "user_id": "dev-sam" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let recorded = approvals(&env.backend, &eid).await;
    assert_eq!(recorded[0]["user_id"], "dev-sam");
    assert!(
        recorded[0].get("principal").is_none(),
        "no principal key on the wire in dev mode: {}",
        recorded[0]
    );
}
