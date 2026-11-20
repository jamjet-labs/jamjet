//! A decision sent twice is answered with the first one.
//!
//! A reviewer's client that loses the answer to `POST /executions/:id/approve`
//! sends the decision again. That used to answer 409, so the client could not
//! tell "my decision is on record" from "someone else decided". The same
//! decision by the same principal, for the same request, now answers 200 with
//! the first decision's sequence and `repeat: true`, and records nothing.
//! Anything else on a decided request is still refused.
//!
//! The tests marked GUARD passed before the change. They pin what a repeat
//! must never become: a different decision, another principal, another note,
//! a newer request and an expiry all stay refused.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditBackend, AuditEnricher, AuditError, AuditLogEntry, AuditQuery};
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::Event;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

mod support;

use support::hooked::{AfterWrite, HookedBackend, Write};

/// Keeps every audit entry, so a test can count them.
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

/// A one-node workflow whose `python_fn` node `pay` waits for approval.
fn ir(workflow_id: &str, node_policy: Value) -> Value {
    json!({
        "workflow_id": workflow_id, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pay",
        "nodes": { "pay": {
            "id": "pay",
            "kind": { "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {},
            "policy": node_policy
        } },
        "edges": [ { "from": "pay", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

/// A node policy that gates `pay`, with the given deadline settings merged in.
fn gate(extra: Value) -> Value {
    let mut p = json!({ "require_approval_for": ["*"] });
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    p
}

struct Env {
    name: &'static str,
    backend: Arc<dyn StateBackend>,
    audit: Arc<CapturingAudit>,
    app: axum::Router,
    /// Starts the runs and plays their worker.
    alice: String,
    /// Reviewers.
    bob: String,
    carol: String,
}

/// The production (auth) router over `backend`, with an audit log that
/// keeps its entries and every workflow of this file registered.
async fn env_on(name: &'static str, backend: Arc<dyn StateBackend>) -> Env {
    let audit = Arc::new(CapturingAudit::default());
    let audit_dyn: Arc<dyn AuditBackend> = audit.clone();
    let for_tenant = backend.clone();
    let state = AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| for_tenant.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit: audit_dyn.clone(),
        enricher: Arc::new(AuditEnricher::new(audit_dyn)),
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    };
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let (carol, _) = backend.create_token("carol", "reviewer").await.unwrap();
    let env = Env {
        name,
        backend,
        audit,
        app,
        alice,
        bob,
        carol,
    };
    for ir in [
        ir("wf-pay", gate(json!({}))),
        ir("wf-deadline", gate(json!({ "approval_timeout_secs": 600 }))),
        ir("wf-soon", gate(json!({ "approval_timeout_secs": 2 }))),
    ] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/workflows",
            &env.alice,
            Some(json!({ "ir": ir })),
        )
        .await;
        assert!(st.is_success(), "{}: {st} {body}", env.name);
    }
    env
}

/// One environment per backend: in-memory and SQLite.
async fn envs() -> Vec<Env> {
    let mut out = Vec::new();
    for tb in support::backends().await {
        out.push(env_on(tb.name(), tb.dyn_backend()).await);
    }
    out
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"));
    let req = match body {
        Some(b) => req.body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn exec_path(eid: &ExecutionId) -> String {
    format!("exec_{}", eid.0.simple())
}

/// One claim on the `python_tool` queue, as alice.
async fn claim(env: &Env) -> Value {
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        Some(json!({ "worker_id": "w", "queue_types": ["python_tool"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
    body
}

/// Start a run of `workflow_id` as alice and claim its gated step, which
/// raises the approval request. Returns the run and the request's digest.
async fn start_and_request(env: &Env, workflow_id: &str) -> (ExecutionId, String) {
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        Some(json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": { "amount": 5 } })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
    let id = body["execution_id"].as_str().unwrap();
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
    assert_eq!(
        claim(env).await,
        json!({ "claimed": false }),
        "{}: a gated step is withheld until approved",
        env.name
    );
    let digest = request_context(&events(env, &eid).await)["action_digest"]
        .as_str()
        .expect("the request binds a digest")
        .to_string();
    (eid, digest)
}

async fn events(env: &Env, eid: &ExecutionId) -> Vec<Event> {
    env.backend.get_events(eid).await.unwrap()
}

/// The latest request's context.
fn request_context(events: &[Event]) -> Value {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => Some(context.clone()),
            _ => None,
        })
        .expect("an approval request")
}

/// The sequences of the recorded decisions.
fn decisions(events: &[Event]) -> Vec<i64> {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .map(|e| e.sequence)
        .collect()
}

fn approve_body(digest: &str) -> Value {
    json!({ "decision": "approved", "node_id": "pay", "action_digest": digest })
}

fn reject_body() -> Value {
    json!({ "decision": "rejected", "node_id": "pay" })
}

async fn decide(env: &Env, token: &str, eid: &ExecutionId, body: Value) -> (StatusCode, Value) {
    call(
        &env.app,
        "POST",
        &format!("/executions/{}/approve", exec_path(eid)),
        token,
        Some(body),
    )
    .await
}

/// The one decision recorded so far: its sequence.
async fn the_decision(env: &Env, eid: &ExecutionId) -> i64 {
    let recorded = decisions(&events(env, eid).await);
    assert_eq!(recorded.len(), 1, "{}: {recorded:?}", env.name);
    recorded[0]
}

async fn audit_entries(env: &Env) -> usize {
    env.audit.entries.lock().await.len()
}

/// A repeat answers like the first decision, with `repeat: true`.
fn assert_repeat_of(env: &Env, first_sequence: i64, again: &Value) {
    assert_eq!(again["repeat"], true, "{}: {again}", env.name);
    assert_eq!(again["accepted"], true, "{}: {again}", env.name);
    assert_eq!(again["node_id"], "pay", "{}: {again}", env.name);
    assert_eq!(again["sequence"], first_sequence, "{}: {again}", env.name);
}

#[tokio::test]
async fn a_repeat_by_the_same_principal_is_200_with_the_first_sequence() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let audit_before = audit_entries(&env).await;

        let (st, first) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {first}", env.name);
        let sequence = the_decision(&env, &eid).await;
        assert_eq!(first["repeat"], false, "{}: {first}", env.name);
        assert_eq!(first["sequence"], sequence, "{}: {first}", env.name);
        assert_eq!(first["accepted"], true, "{}: {first}", env.name);
        assert_eq!(first["node_id"], "pay", "{}: {first}", env.name);
        // Nothing is queued until the scheduler's next pass.
        assert_eq!(
            first.get("waiting_for"),
            Some(&json!("engine")),
            "{}: {first}",
            env.name
        );
        let audit_after_first = audit_entries(&env).await;
        assert!(
            audit_after_first > audit_before,
            "{}: the decision is sealed",
            env.name
        );
        let events_after_first = events(&env, &eid).await.len();

        let (st, again) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {again}", env.name);
        assert_repeat_of(&env, sequence, &again);
        assert_eq!(again["execution_id"], first["execution_id"], "{}", env.name);
        assert_eq!(
            again.get("waiting_for"),
            Some(&json!("engine")),
            "{}: {again}",
            env.name
        );
        assert_eq!(
            events(&env, &eid).await.len(),
            events_after_first,
            "{}: a repeat records nothing",
            env.name
        );
        assert_eq!(
            audit_entries(&env).await,
            audit_after_first,
            "{}: a repeat seals nothing",
            env.name
        );
    }
}

#[tokio::test]
async fn a_repeat_after_the_run_completed_is_200() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let sequence = the_decision(&env, &eid).await;

        // The approved step runs and the run completes.
        let scheduler = Scheduler::new(env.backend.clone());
        scheduler.tick().await.unwrap();
        let claimed = claim(&env).await;
        assert_eq!(claimed["claimed"], true, "{}: {claimed}", env.name);
        let item = &claimed["work_item"];
        let (st, body) = call(
            &env.app,
            "POST",
            &format!("/work-items/{}/complete", item["id"].as_str().unwrap()),
            &env.alice,
            Some(json!({ "output": {}, "state_patch": {}, "lease_fence": item["lease_fence"] })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        for _ in 0..5 {
            scheduler.tick().await.unwrap();
        }
        let status = env
            .backend
            .get_execution(&eid)
            .await
            .unwrap()
            .unwrap()
            .status;
        assert_eq!(status, WorkflowStatus::Completed, "{}", env.name);
        let before = events(&env, &eid).await.len();

        let (st, again) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {again}", env.name);
        assert_repeat_of(&env, sequence, &again);
        assert_eq!(
            again.get("waiting_for"),
            Some(&Value::Null),
            "{}: an ended run waits for nothing: {again}",
            env.name
        );
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// The deadline closes a request to new decisions, not to a repeat of the
/// one made in time.
#[tokio::test]
async fn a_repeat_after_the_deadline_of_a_request_decided_in_time_is_200() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-soon").await;
        let expires_at: DateTime<Utc> = request_context(&events(&env, &eid).await)["expires_at"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .expect("the request records its deadline")
            .with_timezone(&Utc);
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: decided in time: {body}", env.name);
        let sequence = the_decision(&env, &eid).await;

        let wait = expires_at - Utc::now() + Duration::milliseconds(50);
        if let Ok(wait) = wait.to_std() {
            tokio::time::sleep(wait).await;
        }
        assert!(Utc::now() > expires_at, "{}", env.name);
        let before = events(&env, &eid).await.len();

        let (st, again) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {again}", env.name);
        assert_repeat_of(&env, sequence, &again);
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// GUARD: passed before repeats were answered.
#[tokio::test]
async fn a_different_decision_by_the_same_principal_is_409() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let before = events(&env, &eid).await.len();

        let (st, body) = decide(&env, &env.bob, &eid, reject_body()).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// GUARD: passed before repeats were answered.
#[tokio::test]
async fn the_same_decision_by_another_principal_is_409() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let before = events(&env, &eid).await.len();

        let (st, body) = decide(&env, &env.carol, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// GUARD: passed before repeats were answered.
#[tokio::test]
async fn a_repeat_with_another_note_is_409() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let mut noted = approve_body(&digest);
        noted["comment"] = json!("checked the payee");
        let (st, body) = decide(&env, &env.bob, &eid, noted).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let before = events(&env, &eid).await.len();

        let mut other = approve_body(&digest);
        other["comment"] = json!("checked the amount");
        let (st, body) = decide(&env, &env.bob, &eid, other).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        // No note is another note too.
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// An empty or blank note is no note, so it repeats a decision made with none.
#[tokio::test]
async fn a_blank_note_repeats_a_decision_made_without_one() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let sequence = the_decision(&env, &eid).await;
        let before = events(&env, &eid).await.len();

        for blank in ["", "  \n\t"] {
            let mut again = approve_body(&digest);
            again["comment"] = json!(blank);
            let (st, again) = decide(&env, &env.bob, &eid, again).await;
            assert_eq!(st, StatusCode::OK, "{}: {blank:?}: {again}", env.name);
            assert_repeat_of(&env, sequence, &again);
        }
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// GUARD: passed before repeats were answered. The node asks again (its
/// action changed since the decision): the old decision answers the old
/// request only, so sending it again is stale, never a repeat.
#[tokio::test]
async fn a_repeat_after_a_new_request_for_the_node_is_409_stale() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);

        // A new request for the node, bound to another action, as the guard
        // raises when the node is dispatched again with a different payload.
        let evs = events(&env, &eid).await;
        let (tool_name, approver) = evs
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ToolApprovalRequired {
                    tool_name,
                    approver,
                    ..
                } => Some((tool_name.clone(), approver.clone())),
                _ => None,
            })
            .unwrap();
        let mut ctx = request_context(&evs);
        let next = "b".repeat(64);
        assert_ne!(next, digest);
        ctx["action_digest"] = json!(next);
        env.backend
            .append_event(Event::new(
                eid.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: "pay".into(),
                    tool_name,
                    approver,
                    context: ctx,
                },
            ))
            .await
            .unwrap();
        let before = events(&env, &eid).await.len();

        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("action_digest does not match"),
            "{}: stale: {body}",
            env.name
        );
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

#[tokio::test]
async fn a_repeat_without_the_digest_is_400() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let before = events(&env, &eid).await.len();

        let (st, body) = decide(
            &env,
            &env.bob,
            &eid,
            json!({ "decision": "approved", "node_id": "pay" }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{}: {body}", env.name);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("action_digest is required"),
            "{}: {body}",
            env.name
        );
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

#[tokio::test]
async fn a_repeated_rejection_needs_no_digest() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, reject_body()).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let sequence = the_decision(&env, &eid).await;
        let before = events(&env, &eid).await.len();

        let (st, again) = decide(&env, &env.bob, &eid, reject_body()).await;
        assert_eq!(st, StatusCode::OK, "{}: {again}", env.name);
        assert_repeat_of(&env, sequence, &again);

        // An echo a rejection does send must match the request.
        let mut echoed = reject_body();
        echoed["action_digest"] = json!(digest);
        let (st, again) = decide(&env, &env.bob, &eid, echoed).await;
        assert_eq!(st, StatusCode::OK, "{}: {again}", env.name);
        assert_repeat_of(&env, sequence, &again);
        let mut wrong = reject_body();
        wrong["action_digest"] = json!("b".repeat(64));
        let (st, body) = decide(&env, &env.bob, &eid, wrong).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);

        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// GUARD: passed before repeats were answered. An expiry is a rejection by
/// the deadline, not a decision anyone can repeat.
#[tokio::test]
async fn a_decision_after_an_expiry_is_409_expired_not_a_repeat() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-deadline").await;
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, Utc::now() + Duration::days(1))
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e.kind, EventKind::ApprovalExpired { .. }))
                .count(),
            1,
            "{}",
            env.name
        );
        let before = evs.len();

        for body in [approve_body(&digest), reject_body()] {
            let (st, resp) = decide(&env, &env.bob, &eid, body.clone()).await;
            assert_eq!(st, StatusCode::CONFLICT, "{}: {body}: {resp}", env.name);
            assert!(
                resp["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("expired"),
                "{}: {body}: {resp}",
                env.name
            );
        }
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}

/// Holds both decisions of an armed run at the conditional append until both
/// have arrived, so each has read the log while the request was open.
#[derive(Default)]
struct MeetBeforeAppend {
    armed: Mutex<Option<(ExecutionId, Arc<tokio::sync::Barrier>)>>,
    reports: AtomicUsize,
}

#[async_trait]
impl AfterWrite for MeetBeforeAppend {
    async fn after(&self, write: Write) {
        if let Write::BeforeAppendEventIf(eid) = write {
            let armed = self.armed.lock().unwrap().clone();
            if let Some((armed_eid, barrier)) = armed {
                if armed_eid == eid {
                    self.reports.fetch_add(1, Ordering::SeqCst);
                    // Bounded, so a decision that never arrives fails the
                    // count below instead of hanging the test.
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), barrier.wait())
                        .await;
                }
            }
        }
    }
}

#[tokio::test]
async fn two_identical_decisions_at_once_record_one() {
    for tb in support::backends().await {
        let hook = Arc::new(MeetBeforeAppend::default());
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(tb.dyn_backend(), hook.clone()));
        let env = env_on(tb.name(), hooked).await;

        for round in 0..100 {
            let (eid, digest) = start_and_request(&env, "wf-pay").await;
            hook.reports.store(0, Ordering::SeqCst);
            *hook.armed.lock().unwrap() =
                Some((eid.clone(), Arc::new(tokio::sync::Barrier::new(2))));
            let ((st_a, a), (st_b, b)) = tokio::join!(
                decide(&env, &env.bob, &eid, approve_body(&digest)),
                decide(&env, &env.bob, &eid, approve_body(&digest)),
            );
            *hook.armed.lock().unwrap() = None;

            // Both reached the append with the request open: one appended,
            // the other was refused there and found the first decision.
            assert_eq!(
                hook.reports.load(Ordering::SeqCst),
                2,
                "{} round {round}: the race did not open",
                env.name
            );
            let sequence = the_decision(&env, &eid).await;
            assert_eq!(st_a, StatusCode::OK, "{} round {round}: {a}", env.name);
            assert_eq!(st_b, StatusCode::OK, "{} round {round}: {b}", env.name);
            let repeats = [&a, &b].iter().filter(|r| r["repeat"] == true).count();
            assert_eq!(repeats, 1, "{} round {round}: {a} {b}", env.name);
            assert_eq!(a["sequence"], sequence, "{} round {round}: {a}", env.name);
            assert_eq!(b["sequence"], sequence, "{} round {round}: {b}", env.name);
        }
    }
}

/// A live run with one pending request on `pay`, written at store level.
async fn seeded_request(backend: &Arc<dyn StateBackend>) -> ExecutionId {
    let eid = ExecutionId::new();
    let now = Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "wf-dev".into(),
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
    for kind in [
        EventKind::WorkflowStarted {
            workflow_id: "wf-dev".into(),
            workflow_version: "0.1.0".into(),
            initial_input: json!({}),
            initiated_by: None,
        },
        EventKind::ToolApprovalRequired {
            node_id: "pay".into(),
            tool_name: "tool_pay".into(),
            approver: "human".into(),
            context: json!({ "action": "pay" }),
        },
    ] {
        backend
            .append_event(Event::new(eid.clone(), 0, kind))
            .await
            .unwrap();
    }
    eid
}

/// Dev mode has no principal: a repeat is matched by the recorded user.
#[tokio::test]
async fn a_dev_mode_repeat_matches_by_user_id() {
    for tb in support::backends().await {
        let backend = tb.dyn_backend();
        let state = support::make_state(backend.clone());
        let eid = seeded_request(&backend).await;
        let path = format!("/executions/{}/approve", exec_path(&eid));
        let by = |user: &str| json!({ "decision": "approved", "node_id": "pay", "user_id": user });

        let (st, first) = support::post(&state, &path, by("sam")).await;
        assert_eq!(st, StatusCode::OK, "{}: {first}", tb.name());
        let recorded = decisions(&backend.get_events(&eid).await.unwrap());
        assert_eq!(recorded.len(), 1, "{}", tb.name());
        let before = backend.get_events(&eid).await.unwrap().len();

        let (st, again) = support::post(&state, &path, by("sam")).await;
        assert_eq!(st, StatusCode::OK, "{}: {again}", tb.name());
        assert_eq!(again["repeat"], true, "{}: {again}", tb.name());
        assert_eq!(again["sequence"], recorded[0], "{}: {again}", tb.name());

        let (st, other) = support::post(&state, &path, by("kim")).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {other}", tb.name());
        assert_eq!(
            backend.get_events(&eid).await.unwrap().len(),
            before,
            "{}",
            tb.name()
        );
    }
}

#[tokio::test]
async fn the_mcp_approve_tool_answers_a_repeat_like_the_route() {
    for env in envs().await {
        let (eid, digest) = start_and_request(&env, "wf-pay").await;
        let (st, body) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let sequence = the_decision(&env, &eid).await;
        let before = events(&env, &eid).await.len();

        let (st, resp) = call(
            &env.app,
            "POST",
            "/mcp",
            &env.bob,
            Some(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "jamjet_approve", "arguments": {
                    "execution_id": exec_path(&eid), "decision": "approved",
                    "node_id": "pay", "action_digest": digest,
                }},
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{}: {resp}", env.name);
        assert_eq!(resp["result"]["isError"], false, "{}: {resp}", env.name);
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{}: {resp}", env.name));
        let answer: Value = serde_json::from_str(text).unwrap();
        assert_repeat_of(&env, sequence, &answer);

        let (st, route) = decide(&env, &env.bob, &eid, approve_body(&digest)).await;
        assert_eq!(st, StatusCode::OK, "{}: {route}", env.name);
        assert_eq!(answer, route, "{}: the same answer", env.name);
        assert_eq!(events(&env, &eid).await.len(), before, "{}", env.name);
    }
}
