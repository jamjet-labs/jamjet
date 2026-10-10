//! An approval is bound to the exact action that was reviewed.
//!
//! The hold records `action_digest` in the request context. A decision may
//! echo it back (stale view -> 409), and the recorded `ApprovalReceived`
//! carries `approved_digest` (what the worker will accept at resume) and
//! `request_sequence` (which request it answers). An approver's `state_patch`
//! is only accepted against the state that was reviewed.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::approvals::{action_basis, derive_action_digest};
use jamjet_state::backend::StateBackend;
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
    app: axum::Router,
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
        app: build_router_with_opts(state, true),
    }
}

/// The definition hash the simulated hold binds (the worker computes it from
/// the node's IR; these route tests only need it to be consistent).
const DEF: &str = "test-definition";

async fn append(backend: &Arc<InMemoryBackend>, eid: &ExecutionId, kind: EventKind) -> i64 {
    let seq = backend.latest_sequence(eid).await.unwrap() + 1;
    backend
        .append_event(Event::new(eid.clone(), seq, kind))
        .await
        .unwrap();
    seq
}

/// A run with state `{"amount": 10}` and node `gate` held the way the worker
/// holds it: the request context carries the digest of the action now.
/// Returns (execution, requested digest, request sequence).
async fn held(backend: &Arc<InMemoryBackend>, with_digest: bool) -> (ExecutionId, String, i64) {
    let eid = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf".into(),
            workflow_version: "0.1.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({ "amount": 10 }),
            current_state: json!({ "amount": 10 }),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
    let digest = derive_action_digest(backend.as_ref(), &eid, "gate", DEF)
        .await
        .unwrap();
    let mut context = json!({ "node_id": "gate" });
    if with_digest {
        context["action_digest"] = json!(digest);
        context["action_definition"] = json!(DEF);
    }
    let seq = append(
        backend,
        &eid,
        EventKind::ToolApprovalRequired {
            node_id: "gate".into(),
            tool_name: "payments.transfer".into(),
            approver: "human".into(),
            context,
        },
    )
    .await;
    (eid, digest, seq)
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
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

async fn decisions(backend: &Arc<InMemoryBackend>, eid: &ExecutionId) -> Vec<Value> {
    backend
        .get_events(eid)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .map(|e| serde_json::to_value(&e.kind).unwrap())
        .collect()
}

fn sibling_commits(patch: Value) -> EventKind {
    EventKind::NodeCompleted {
        node_id: "sibling".into(),
        output: json!({}),
        state_patch: patch,
        duration_ms: 0,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
        idempotency_key: None,
    }
}

#[tokio::test]
async fn an_approval_records_the_requested_digest_and_request_sequence() {
    let env = env();
    let (eid, digest, req_seq) = held(&env.backend, true).await;

    let (status, body) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let d = decisions(&env.backend, &eid).await;
    assert_eq!(d[0]["approved_digest"], digest);
    assert_eq!(d[0]["request_sequence"], req_seq);
}

#[tokio::test]
async fn an_echoed_matching_digest_is_accepted() {
    let env = env();
    let (eid, digest, _) = held(&env.backend, true).await;

    let (status, _) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved", "action_digest": digest }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        decisions(&env.backend, &eid).await[0]["approved_digest"],
        digest
    );
}

#[tokio::test]
async fn a_stale_echoed_digest_is_409_and_appends_nothing() {
    let env = env();
    let (eid, _, _) = held(&env.backend, true).await;

    let (status, _) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved", "action_digest": "0".repeat(64) }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(decisions(&env.backend, &eid).await.is_empty());
}

#[tokio::test]
async fn an_echoed_digest_against_a_digestless_request_is_409() {
    let env = env();
    let (eid, digest, _) = held(&env.backend, false).await;

    let (status, _) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved", "action_digest": digest }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(decisions(&env.backend, &eid).await.is_empty());
}

#[tokio::test]
async fn a_digestless_request_records_no_approved_digest() {
    // Agent tool dispatch requests bind with calls_hash instead; their
    // approvals must not invent a digest.
    let env = env();
    let (eid, _, _) = held(&env.backend, false).await;

    let (status, _) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(decisions(&env.backend, &eid).await[0]
        .get("approved_digest")
        .is_none());
}

#[tokio::test]
async fn a_patch_against_a_moved_state_is_409_and_appends_nothing() {
    let env = env();
    let (eid, _, _) = held(&env.backend, true).await;
    append(
        &env.backend,
        &eid,
        sibling_commits(json!({ "amount": 1_000_000 })),
    )
    .await;

    let (status, body) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved", "state_patch": { "memo": "ok" } }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(decisions(&env.backend, &eid).await.is_empty());
}

#[tokio::test]
async fn a_patched_approval_binds_the_patched_state_and_the_worker_will_match_it() {
    let env = env();
    let (eid, requested, _) = held(&env.backend, true).await;
    let expected = action_basis(env.backend.as_ref(), &eid, "gate", DEF)
        .await
        .unwrap()
        .digest_with_patch("gate", &json!({ "amount": 5 }));

    let (status, body) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved", "state_patch": { "amount": 5 } }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let d = decisions(&env.backend, &eid).await;
    assert_eq!(d[0]["approved_digest"], expected);
    assert_ne!(expected, requested, "the patch changes what is approved");
    // TIMING: the approval's state_patch is applied by the materializer fold as
    // soon as the event is in the log, so the digest the worker recomputes at
    // resume is the patched one. Otherwise a patched approval would re-request
    // forever.
    assert_eq!(
        derive_action_digest(env.backend.as_ref(), &eid, "gate", DEF)
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn a_rejection_records_its_request_but_no_digest() {
    let env = env();
    let (eid, _, req_seq) = held(&env.backend, true).await;

    let (status, _) = post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "rejected" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let d = decisions(&env.backend, &eid).await;
    assert!(d[0].get("approved_digest").is_none());
    assert_eq!(d[0]["request_sequence"], req_seq);
}

#[tokio::test]
async fn the_pending_listing_exposes_the_action_digest() {
    let env = env();
    let (eid, digest, _) = held(&env.backend, true).await;

    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(format!("/executions/{eid}/approvals"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(body["pending"][0]["context"]["action_digest"], digest);
}

#[tokio::test]
async fn the_sealed_audit_entry_carries_the_binding() {
    let env = env();
    let (eid, digest, req_seq) = held(&env.backend, true).await;

    post(
        &env.app,
        &format!("/executions/{eid}/approve"),
        json!({ "decision": "approved" }),
    )
    .await;

    let entries = env.audit.entries.lock().await;
    let entry = entries
        .iter()
        .find(|e| e.event_type == "approval_received")
        .expect("sealed approval");
    assert_eq!(entry.raw_event["approved_digest"], digest);
    assert_eq!(entry.raw_event["request_sequence"], req_seq);
    assert!(
        entry.entry_hash.is_some(),
        "the binding is inside the sealed entry"
    );
}

#[tokio::test]
async fn a_stale_digest_through_the_mcp_bridge_is_refused() {
    let env = env();
    let (eid, _, _) = held(&env.backend, true).await;

    let (_, resp) = post(
        &env.app,
        "/mcp",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_approve", "arguments": {
                "execution_id": eid.to_string(), "decision": "approved",
                "action_digest": "0".repeat(64)
            }},
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true, "{resp}");
    assert!(decisions(&env.backend, &eid).await.is_empty());
}
