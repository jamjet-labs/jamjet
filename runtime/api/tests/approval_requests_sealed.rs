//! Approval requests are sealed into the audit chain (gap 5 of the 2026-07-14
//! approval gaps).
//!
//! Only the decision used to be sealed: the `ToolApprovalRequired` a worker or
//! the claim route raised went to the event log alone, so the record of what
//! was asked (its action digest, its deadline) could be rewritten without the
//! chain noticing. Every request, expiry and escalation is now sealed into
//! the same signed, hash-chained log as the decisions, whichever component
//! appended it: the HTTP claim route, the in-process worker, the scheduler's
//! deadline sweep.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{
    verify_chain, ActorType, AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery,
    AuditSigner, ChainError,
};
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// Keeps every appended entry in chain order.
#[derive(Default)]
struct CapturingAuditBackend {
    entries: tokio::sync::Mutex<Vec<AuditLogEntry>>,
}

#[async_trait]
impl AuditBackend for CapturingAuditBackend {
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
    audit: Arc<CapturingAuditBackend>,
    enricher: Arc<AuditEnricher>,
    signer: AuditSigner,
    app: axum::Router,
    alice: String,
    bob: String,
}

async fn env() -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    let signer = AuditSigner::new(b"sealed-requests-key".to_vec());
    let audit = Arc::new(CapturingAuditBackend::default());
    let audit_dyn: Arc<dyn AuditBackend> = audit.clone();
    let enricher = Arc::new(AuditEnricher::with_signer(
        audit_dyn.clone(),
        signer.clone(),
    ));
    let b = backend.clone();
    let state = AppState {
        backend: backend.clone() as Arc<dyn StateBackend>,
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| {
            b.clone() as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit: audit_dyn,
        enricher: enricher.clone(),
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    Env {
        backend,
        audit,
        enricher,
        signer,
        app,
        alice,
        bob,
    }
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
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

fn ir() -> Value {
    json!({
        "workflow_id": "wf-sealed", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pay",
        "nodes": { "pay": {
            "id": "pay",
            "kind": { "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {},
            "policy": { "require_approval_for": ["*"], "approval_timeout_secs": 600, "escalate_after_secs": 60 }
        } },
        "edges": [ { "from": "pay", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

async fn start(env: &Env) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir() }),
    )
    .await;
    assert!(st.is_success(), "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-sealed", "workflow_version": "0.1.0", "input": { "amount": 5 } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

/// The request as the event log has it: (sequence, context).
async fn logged_request(env: &Env, eid: &ExecutionId) -> (i64, Value) {
    env.backend
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => Some((e.sequence, context.clone())),
            _ => None,
        })
        .expect("a request in the log")
}

fn of_type<'a>(entries: &'a [AuditLogEntry], t: &str) -> Vec<&'a AuditLogEntry> {
    entries.iter().filter(|e| e.event_type == t).collect()
}

/// The sealed request carries the digest and deadline the log records, the
/// whole chain (request, then decision) verifies, and editing the request's
/// sealed record breaks verification at that entry.
async fn assert_request_sealed(env: &Env, eid: &ExecutionId) {
    let (seq, context) = logged_request(env, eid).await;
    let entries = env.audit.entries.lock().await.clone();
    let requests = of_type(&entries, "tool_approval_required");
    assert_eq!(requests.len(), 1, "one sealed request: {entries:#?}");
    let sealed = requests[0];
    assert_eq!(sealed.execution_id, eid.to_string());
    assert_eq!(
        sealed.sequence, seq,
        "the sealed entry carries the logged sequence"
    );
    assert_eq!(
        sealed.raw_event["context"]["action_digest"],
        context["action_digest"]
    );
    assert_eq!(
        sealed.raw_event["context"]["expires_at"],
        context["expires_at"]
    );
    assert!(
        sealed.entry_hash.is_some() && sealed.signature.is_some(),
        "sealed and signed"
    );
    verify_chain(&entries, &env.signer).expect("the chain with the request verifies");

    // Tamper with the sealed request: point it at another action.
    let index = entries
        .iter()
        .position(|e| e.event_type == "tool_approval_required")
        .unwrap();
    let mut tampered = entries.clone();
    tampered[index].raw_event["context"]["action_digest"] = json!("0".repeat(64));
    assert_eq!(
        verify_chain(&tampered, &env.signer),
        Err(ChainError::HashMismatch { index })
    );
}

#[tokio::test]
async fn a_request_raised_on_the_claim_route_is_sealed() {
    let env = env().await;
    let eid = start(&env).await;
    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!((st, claimed), (StatusCode::OK, json!({ "claimed": false })));
    // The decision joins the same chain after the request.
    let (_, context) = logged_request(&env, &eid).await;
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/executions/exec_{}/approve", eid.0.simple()),
        &env.bob,
        json!({ "decision": "approved", "node_id": "pay", "action_digest": context["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let entries = env.audit.entries.lock().await.clone();
    let types: Vec<&str> = entries.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, ["tool_approval_required", "approval_received"]);
    assert_request_sealed(&env, &eid).await;
}

#[tokio::test]
async fn a_request_raised_by_the_in_process_worker_is_sealed() {
    let env = env().await;
    let eid = start(&env).await;
    let worker = jamjet_worker::Worker::new(
        "w".into(),
        env.backend.clone() as Arc<dyn StateBackend>,
        vec!["python_tool".into()],
    )
    .with_sealer(env.enricher.clone());
    assert!(
        worker.poll_and_execute().await.unwrap(),
        "the start item is claimed"
    );
    assert_request_sealed(&env, &eid).await;
}

#[tokio::test]
async fn expiries_and_escalations_are_sealed() {
    let env = env().await;
    let eid = start(&env).await;
    let (st, _) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let scheduler = Scheduler::new(env.backend.clone() as Arc<dyn StateBackend>)
        .with_sealer(env.enricher.clone());
    let now = Utc::now();
    scheduler
        .sweep_approval_deadlines(&eid, now + Duration::seconds(120))
        .await
        .unwrap();
    scheduler
        .sweep_approval_deadlines(&eid, now + Duration::seconds(3600))
        .await
        .unwrap();

    let entries = env.audit.entries.lock().await.clone();
    let types: Vec<&str> = entries.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "tool_approval_required",
            "approval_escalated",
            "approval_expired"
        ],
        "{entries:#?}"
    );
    let expired = of_type(&entries, "approval_expired")[0];
    assert_eq!(expired.actor_id, "system:deadline");
    assert_eq!(expired.actor_type, ActorType::System);
    let (seq, _) = logged_request(&env, &eid).await;
    assert_eq!(expired.raw_event["request_sequence"], json!(seq));
    verify_chain(&entries, &env.signer).expect("the chain verifies");

    let mut tampered = entries.clone();
    tampered[2].raw_event["request_sequence"] = json!(seq + 1000);
    assert_eq!(
        verify_chain(&tampered, &env.signer),
        Err(ChainError::HashMismatch { index: 2 })
    );
}

/// An audit log that refuses every write.
struct FailingAuditBackend;

#[async_trait]
impl AuditBackend for FailingAuditBackend {
    async fn append(&self, _entry: AuditLogEntry) -> Result<(), AuditError> {
        Err(AuditError::Database("disk full".into()))
    }
    async fn query(&self, _q: &AuditQuery) -> Result<Vec<AuditLogEntry>, AuditError> {
        Ok(vec![])
    }
    async fn count(&self, _q: &AuditQuery) -> Result<u64, AuditError> {
        Ok(0)
    }
}

/// Review M-7: a seal that fails leaves no audit entry, and `verify_chain`
/// cannot see an entry that was never written. The gap is recorded in the
/// run's own event log (`audit_seal_failed`, naming the unsealed event) and
/// counted on the enricher, so it can be found.
#[tokio::test]
async fn a_failed_seal_is_recorded_as_a_gap() {
    let backend = Arc::new(InMemoryBackend::new());
    let audit: Arc<dyn AuditBackend> = Arc::new(FailingAuditBackend);
    let enricher = Arc::new(AuditEnricher::with_signer(
        audit.clone(),
        AuditSigner::new(b"k".to_vec()),
    ));
    let b = backend.clone();
    let state = AppState {
        backend: backend.clone() as Arc<dyn StateBackend>,
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| {
            b.clone() as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher: enricher.clone(),
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (st, body) = call(&app, "POST", "/workflows", &alice, json!({ "ir": ir() })).await;
    assert!(st.is_success(), "{body}");
    let (st, body) = call(
        &app,
        "POST",
        "/executions",
        &alice,
        json!({ "workflow_id": "wf-sealed", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap();
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
    let (st, _) = call(
        &app,
        "POST",
        "/work-items/claim",
        &alice,
        json!({ "worker_id": "w", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let events = backend.get_events(&eid).await.unwrap();
    let request = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
        .expect("the request is recorded even though its seal failed");
    let gap = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::AuditSealFailed {
                event_id,
                sequence,
                event_type,
                error,
            } => Some((*event_id, *sequence, event_type.clone(), error.clone())),
            _ => None,
        })
        .expect("the failed seal is recorded in the run's log");
    assert_eq!(gap.0, request.id);
    assert_eq!(gap.1, request.sequence);
    assert_eq!(gap.2, "tool_approval_required");
    assert!(gap.3.contains("disk full"), "{}", gap.3);
    assert_eq!(enricher.seal_failures(), 1);
}
