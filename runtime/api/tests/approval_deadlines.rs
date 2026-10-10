//! Approval deadlines (gap 3 of the 2026-07-14 approval gaps).
//!
//! A request raised under a policy with `approval_timeout_secs` records when it
//! expires. The scheduler's sweep expires it then: `ApprovalExpired`, treated
//! exactly as a rejection by `system:deadline`, so the node fails the way a
//! rejected one does. It never approves. A decision that arrives after the
//! deadline is refused (409) and records nothing, and a decision racing the
//! sweep yields exactly one outcome, because both append only while the
//! request is still pending, in one step.
//!
//! The sweep takes the time it runs at, so these tests move the clock instead
//! of sleeping.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::{Event, InMemoryBackend, Tenant, TenantId, TenantStatus};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

mod support;

use support::hooked::{AfterWrite, HookedBackend, Write};

fn ir(workflow_policy: Value, node_policy: Value) -> Value {
    json!({
        "workflow_id": "wf-deadline", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "pay",
        "policy": workflow_policy,
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
    app: axum::Router,
    alice: String,
    bob: String,
    ops: String,
}

async fn env_on(name: &'static str, backend: Arc<dyn StateBackend>) -> Env {
    let state = support::make_state(backend.clone());
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let (ops, _) = backend.create_token("ops", "operator").await.unwrap();
    Env {
        name,
        backend,
        app,
        alice,
        bob,
        ops,
    }
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

fn exec_path(eid: &ExecutionId) -> String {
    format!("exec_{}", eid.0.simple())
}

/// Register `ir`, start a run as alice and claim it, which raises the
/// approval request. Returns the run.
async fn start_and_request(env: &Env, ir: Value) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": ir }),
    )
    .await;
    assert!(st.is_success(), "{}: {st} {body}", env.name);
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-deadline", "workflow_version": "0.1.0", "input": { "amount": 5 } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{}: {body}", env.name);
    let id = body["execution_id"].as_str().unwrap();
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
    let (st, claimed) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["python_tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}: {claimed}", env.name);
    assert_eq!(
        claimed,
        json!({ "claimed": false }),
        "{}: a gated node is withheld until approved",
        env.name
    );
    eid
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

fn ts(v: &Value, key: &str) -> Option<DateTime<Utc>> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc))
}

fn count(events: &[Event], pred: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|e| pred(&e.kind)).count()
}

fn expired(k: &EventKind) -> bool {
    matches!(k, EventKind::ApprovalExpired { .. })
}

fn escalated(k: &EventKind) -> bool {
    matches!(k, EventKind::ApprovalEscalated { .. })
}

fn received(k: &EventKind) -> bool {
    matches!(k, EventKind::ApprovalReceived { .. })
}

async fn approve(env: &Env, eid: &ExecutionId, digest: &str) -> (StatusCode, Value) {
    call(
        &env.app,
        "POST",
        &format!("/executions/{}/approve", exec_path(eid)),
        &env.bob,
        json!({ "decision": "approved", "node_id": "pay", "action_digest": digest }),
    )
    .await
}

fn close_to(actual: Option<DateTime<Utc>>, expected: DateTime<Utc>, what: &str) {
    let actual = actual.unwrap_or_else(|| panic!("{what} is missing"));
    let off = (actual - expected).num_seconds().abs();
    assert!(off <= 5, "{what}: {actual} is not near {expected}");
}

#[tokio::test]
async fn a_request_records_its_deadline_and_the_listing_shows_it() {
    for env in envs().await {
        let before = Utc::now();
        let eid = start_and_request(
            &env,
            ir(
                Value::Null,
                gate(json!({ "approval_timeout_secs": 600, "escalate_after_secs": 60 })),
            ),
        )
        .await;
        let ctx = request_context(&events(&env, &eid).await);
        close_to(
            ts(&ctx, "expires_at"),
            before + Duration::seconds(600),
            "expires_at",
        );
        close_to(
            ts(&ctx, "escalate_at"),
            before + Duration::seconds(60),
            "escalate_at",
        );
        assert!(
            ctx["expires_at"].as_str().unwrap().ends_with('Z'),
            "{}: expires_at is UTC: {ctx}",
            env.name
        );

        let (st, listing) = call(
            &env.app,
            "GET",
            &format!("/executions/{}/approvals", exec_path(&eid)),
            &env.bob,
            Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{listing}");
        let pending = &listing["pending"][0];
        assert_eq!(
            pending["expires_at"], ctx["expires_at"],
            "{}: {listing}",
            env.name
        );
        assert_eq!(
            pending["escalate_at"], ctx["escalate_at"],
            "{}: {listing}",
            env.name
        );
    }
}

#[tokio::test]
async fn with_no_deadline_a_request_never_expires() {
    for env in envs().await {
        let eid = start_and_request(&env, ir(Value::Null, gate(json!({})))).await;
        let ctx = request_context(&events(&env, &eid).await);
        assert!(ctx.get("expires_at").is_none(), "{}: {ctx}", env.name);
        assert!(ctx.get("escalate_at").is_none(), "{}: {ctx}", env.name);

        let scheduler = Scheduler::new(env.backend.clone());
        let far = Utc::now() + Duration::days(3650);
        scheduler.sweep_approval_deadlines(&eid, far).await.unwrap();
        scheduler.tick().await.unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, expired), 0, "{}", env.name);
        assert_eq!(count(&evs, escalated), 0, "{}", env.name);
        let (st, listing) = call(
            &env.app,
            "GET",
            &format!("/executions/{}/approvals", exec_path(&eid)),
            &env.bob,
            Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(listing["pending"].as_array().unwrap().len(), 1, "{listing}");
        assert!(
            listing["pending"][0].get("expires_at").is_none(),
            "{listing}"
        );
    }
}

#[tokio::test]
async fn expiry_rejects_the_request_and_fails_the_node() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        let ctx = request_context(&events(&env, &eid).await);
        let expires_at = ts(&ctx, "expires_at").unwrap();
        let scheduler = Scheduler::new(env.backend.clone());

        // One millisecond before the deadline: nothing.
        scheduler
            .sweep_approval_deadlines(&eid, expires_at - Duration::milliseconds(1))
            .await
            .unwrap();
        assert_eq!(count(&events(&env, &eid).await, expired), 0, "{}", env.name);

        // At the deadline: expired, by system:deadline, never approved.
        scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        let exp: Vec<&EventKind> = evs.iter().map(|e| &e.kind).filter(|k| expired(k)).collect();
        assert_eq!(exp.len(), 1, "{}: {evs:#?}", env.name);
        match exp[0] {
            EventKind::ApprovalExpired {
                node_id,
                principal,
                expires_at: recorded,
                ..
            } => {
                assert_eq!(node_id, "pay");
                assert_eq!(principal, "system:deadline");
                assert_eq!(*recorded, expires_at);
            }
            _ => unreachable!(),
        }

        // The scheduler fails the node and the run, as for a rejection.
        scheduler.tick().await.unwrap();
        scheduler.tick().await.unwrap();
        let evs = events(&env, &eid).await;
        assert!(
            evs.iter().any(|e| matches!(&e.kind,
                EventKind::NodeFailed { node_id, retryable: false, error, .. }
                    if node_id == "pay" && error.contains("system:deadline"))),
            "{}: the node fails on expiry: {evs:#?}",
            env.name
        );
        assert!(
            evs.iter()
                .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })),
            "{}: the run fails",
            env.name
        );
        assert!(
            !evs.iter().any(|e| matches!(
                &e.kind,
                EventKind::ApprovalReceived {
                    decision: ApprovalDecision::Approved,
                    ..
                }
            )),
            "{}: expiry never approves",
            env.name
        );
        assert!(
            !evs.iter()
                .any(|e| matches!(&e.kind, EventKind::NodeCompleted { .. })),
            "{}: the node never ran",
            env.name
        );

        // The listing shows it decided, rejected by system:deadline.
        let (_, listing) = call(
            &env.app,
            "GET",
            &format!("/executions/{}/approvals", exec_path(&eid)),
            &env.bob,
            Value::Null,
        )
        .await;
        assert!(
            listing["pending"].as_array().unwrap().is_empty(),
            "{listing}"
        );
        assert_eq!(listing["decided"][0]["status"], "rejected", "{listing}");
        assert_eq!(
            listing["decided"][0]["user_id"], "system:deadline",
            "{listing}"
        );
    }
}

#[tokio::test]
async fn a_decision_after_the_deadline_is_refused_and_records_nothing() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        let ctx = request_context(&events(&env, &eid).await);
        let digest = ctx["action_digest"].as_str().unwrap().to_string();
        let expires_at = ts(&ctx, "expires_at").unwrap();
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        let before = events(&env, &eid).await.len();

        let (st, body) = approve(&env, &eid, &digest).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert!(
            body.to_string().contains("expired"),
            "{}: the refusal says why: {body}",
            env.name
        );
        let (st, body) = call(
            &env.app,
            "POST",
            &format!("/executions/{}/approve", exec_path(&eid)),
            &env.bob,
            json!({ "decision": "rejected", "node_id": "pay" }),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert_eq!(
            events(&env, &eid).await.len(),
            before,
            "{}: nothing recorded",
            env.name
        );
    }
}

/// Review M-3: with no node named and the run's only request expired, the
/// answer is Expired, not "nothing pending". A late approval with no digest
/// is Expired too: the deadline is checked before the digest rules.
#[tokio::test]
async fn a_late_decision_without_a_node_or_digest_says_expired() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, Utc::now() + Duration::days(1))
            .await
            .unwrap();
        for body in [
            json!({ "decision": "approved" }),
            json!({ "decision": "rejected" }),
            json!({ "decision": "approved", "node_id": "pay" }),
        ] {
            let (st, resp) = call(
                &env.app,
                "POST",
                &format!("/executions/{}/approve", exec_path(&eid)),
                &env.bob,
                body.clone(),
            )
            .await;
            assert_eq!(st, StatusCode::CONFLICT, "{}: {body}: {resp}", env.name);
            assert!(
                resp.to_string().contains("expired at its deadline"),
                "{}: {body}: {resp}",
                env.name
            );
        }
    }
}

/// A request whose deadline has passed is refused even before any sweep has
/// expired it: the deadline, not the sweep's timing, decides.
#[tokio::test]
async fn a_decision_past_the_deadline_is_refused_before_the_sweep_runs() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        // Re-raise the request as if it had been made an hour ago, with the
        // same binding: its deadline is in the past and no sweep has run.
        let evs = events(&env, &eid).await;
        let mut ctx = request_context(&evs);
        let digest = ctx["action_digest"].as_str().unwrap().to_string();
        ctx["expires_at"] = json!((Utc::now() - Duration::seconds(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        env.backend
            .append_event(Event::new(
                eid.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: "pay".into(),
                    tool_name: "pay".into(),
                    approver: "human".into(),
                    context: ctx,
                },
            ))
            .await
            .unwrap();
        let before = events(&env, &eid).await.len();
        let (st, body) = approve(&env, &eid, &digest).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        assert_eq!(
            events(&env, &eid).await.len(),
            before,
            "{}: nothing recorded",
            env.name
        );
    }
}

/// An unreadable deadline is treated as passed: refuse, and expire it.
#[tokio::test]
async fn an_unreadable_deadline_fails_closed() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        let mut ctx = request_context(&events(&env, &eid).await);
        let digest = ctx["action_digest"].as_str().unwrap().to_string();
        ctx["expires_at"] = json!("next tuesday");
        env.backend
            .append_event(Event::new(
                eid.clone(),
                0,
                EventKind::ToolApprovalRequired {
                    node_id: "pay".into(),
                    tool_name: "pay".into(),
                    approver: "human".into(),
                    context: ctx,
                },
            ))
            .await
            .unwrap();
        let (st, body) = approve(&env, &eid, &digest).await;
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, Utc::now())
            .await
            .unwrap();
        assert_eq!(count(&events(&env, &eid).await, expired), 1, "{}", env.name);
    }
}

/// Runs an expiry sweep, far past every deadline, right after the approve
/// route reads the log: the latest point a sweep can land before the
/// decision is recorded.
struct SweepAfterRead {
    scheduler: Scheduler,
    armed: Mutex<Option<ExecutionId>>,
}

#[async_trait::async_trait]
impl AfterWrite for SweepAfterRead {
    async fn after(&self, write: Write) {
        if let Write::GetEvents(eid) = write {
            let armed = self.armed.lock().unwrap().take();
            if armed.as_ref() == Some(&eid) {
                self.scheduler
                    .sweep_approval_deadlines(&eid, Utc::now() + Duration::days(30))
                    .await
                    .unwrap();
            }
        }
    }
}

#[tokio::test]
async fn a_decision_racing_the_expiry_yields_exactly_one_outcome() {
    for tb in support::backends().await {
        let inner = tb.dyn_backend();
        let hook = Arc::new(SweepAfterRead {
            scheduler: Scheduler::new(inner.clone()),
            armed: Mutex::new(None),
        });
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(inner.clone(), hook.clone()));
        let env = env_on(tb.name(), hooked).await;
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        let digest = request_context(&events(&env, &eid).await)["action_digest"]
            .as_str()
            .unwrap()
            .to_string();

        // The expiry lands between the route's read and its append.
        *hook.armed.lock().unwrap() = Some(eid.clone());
        let (st, body) = approve(&env, &eid, &digest).await;
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, expired), 1, "{}: {evs:#?}", env.name);
        assert_eq!(
            count(&evs, received),
            0,
            "{}: the decision lost the race and recorded nothing",
            env.name
        );
        assert_eq!(st, StatusCode::CONFLICT, "{}: {body}", env.name);
    }
}

/// Records a human approval through the route right after the sweep reads
/// the log: a decision landing between the sweep's read and its append.
struct ApproveAfterRead {
    app: std::sync::OnceLock<(axum::Router, String)>,
    armed: Mutex<Option<(ExecutionId, String)>>,
    outcome: Mutex<Option<StatusCode>>,
}

#[async_trait::async_trait]
impl AfterWrite for ApproveAfterRead {
    async fn after(&self, write: Write) {
        if let Write::GetEvents(eid) = write {
            let armed = self.armed.lock().unwrap().take();
            if let Some((armed_eid, digest)) = armed {
                if armed_eid == eid {
                    let (app, bob) = self.app.get().unwrap();
                    let (st, _) = call(
                        app,
                        "POST",
                        &format!("/executions/{}/approve", exec_path(&eid)),
                        bob,
                        json!({ "decision": "approved", "node_id": "pay", "action_digest": digest }),
                    )
                    .await;
                    *self.outcome.lock().unwrap() = Some(st);
                }
            }
        }
    }
}

#[tokio::test]
async fn an_expiry_racing_a_decision_records_nothing() {
    for tb in support::backends().await {
        let inner = tb.dyn_backend();
        let hook = Arc::new(ApproveAfterRead {
            app: std::sync::OnceLock::new(),
            armed: Mutex::new(None),
            outcome: Mutex::new(None),
        });
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(inner.clone(), hook.clone()));
        let env = env_on(tb.name(), hooked.clone()).await;
        let _ = hook.app.set((env.app.clone(), env.bob.clone()));
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        let digest = request_context(&events(&env, &eid).await)["action_digest"]
            .as_str()
            .unwrap()
            .to_string();

        // The sweep reads the log with the request pending and past its
        // deadline (its clock is a month ahead); the decision, made on the
        // real clock before the deadline, lands before the sweep appends.
        *hook.armed.lock().unwrap() = Some((eid.clone(), digest));
        Scheduler::new(hooked.clone())
            .sweep_approval_deadlines(&eid, Utc::now() + Duration::days(30))
            .await
            .unwrap();
        assert_eq!(
            *hook.outcome.lock().unwrap(),
            Some(StatusCode::OK),
            "{}: the decision ran inside the sweep",
            env.name
        );
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, received), 1, "{}", env.name);
        assert_eq!(
            count(&evs, expired),
            0,
            "{}: the sweep lost the race and recorded nothing: {evs:#?}",
            env.name
        );
    }
}

/// Stalls the next conditional append of an armed run, as a lock wait or a
/// GC pause would.
struct StallBeforeAppend {
    armed: Mutex<Option<(ExecutionId, std::time::Duration)>>,
}

#[async_trait::async_trait]
impl AfterWrite for StallBeforeAppend {
    async fn after(&self, write: Write) {
        if let Write::BeforeAppendEventIf(eid) = write {
            let armed = self.armed.lock().unwrap().take();
            if let Some((a, d)) = armed {
                if a == eid {
                    tokio::time::sleep(d).await;
                }
            }
        }
    }
}

/// Review I-1 (ported probe): a decision checked 300 ms before the deadline
/// whose append is delayed 700 ms lands after it. The deadline is checked
/// again inside the append, under the write lock: the decision is refused as
/// expired, nothing is recorded, and the sweep then expires the request.
#[tokio::test]
async fn a_decision_whose_append_is_delayed_past_the_deadline_is_refused() {
    let mut failures = Vec::new();
    for tb in support::backends().await {
        let inner = tb.dyn_backend();
        let hook = Arc::new(StallBeforeAppend {
            armed: Mutex::new(None),
        });
        let hooked: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(inner.clone(), hook.clone()));
        let env = env_on(tb.name(), hooked).await;
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 1 }))),
        )
        .await;
        let ctx = request_context(&events(&env, &eid).await);
        let digest = ctx["action_digest"].as_str().unwrap().to_string();
        let expires_at = ts(&ctx, "expires_at").unwrap();
        let wait = (expires_at - Utc::now() - Duration::milliseconds(300))
            .to_std()
            .unwrap();
        tokio::time::sleep(wait).await;
        *hook.armed.lock().unwrap() = Some((eid.clone(), std::time::Duration::from_millis(700)));
        let (st, body) = approve(&env, &eid, &digest).await;
        assert!(Utc::now() > expires_at);
        let evs = events(&env, &eid).await;
        let late = count(&evs, received);
        if late != 0
            || st != StatusCode::CONFLICT
            || !body.to_string().contains("expired at its deadline")
        {
            failures.push(format!(
                "{}: status {st}, {late} decision(s) recorded after the deadline: {body}",
                env.name
            ));
            continue;
        }
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, Utc::now())
            .await
            .unwrap();
        assert_eq!(count(&events(&env, &eid).await, expired), 1, "{}", env.name);
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test]
async fn a_decision_before_the_sweep_wins_and_the_sweep_records_nothing() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(
                Value::Null,
                gate(json!({ "approval_timeout_secs": 600, "escalate_after_secs": 60 })),
            ),
        )
        .await;
        let ctx = request_context(&events(&env, &eid).await);
        let (st, body) = approve(&env, &eid, ctx["action_digest"].as_str().unwrap()).await;
        assert_eq!(st, StatusCode::OK, "{}: {body}", env.name);
        let scheduler = Scheduler::new(env.backend.clone());
        scheduler
            .sweep_approval_deadlines(&eid, Utc::now() + Duration::days(30))
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, received), 1, "{}", env.name);
        assert_eq!(
            count(&evs, expired),
            0,
            "{}: a decided request does not expire",
            env.name
        );
        assert_eq!(count(&evs, escalated), 0, "{}: nor escalate", env.name);
    }
}

#[tokio::test]
async fn escalation_is_recorded_once_and_expiry_still_follows() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(
                Value::Null,
                gate(json!({ "approval_timeout_secs": 600, "escalate_after_secs": 60 })),
            ),
        )
        .await;
        let ctx = request_context(&events(&env, &eid).await);
        let escalate_at = ts(&ctx, "escalate_at").unwrap();
        let expires_at = ts(&ctx, "expires_at").unwrap();
        let scheduler = Scheduler::new(env.backend.clone());

        scheduler
            .sweep_approval_deadlines(&eid, escalate_at - Duration::milliseconds(1))
            .await
            .unwrap();
        assert_eq!(
            count(&events(&env, &eid).await, escalated),
            0,
            "{}",
            env.name
        );

        for offset in [0, 1, 30] {
            scheduler
                .sweep_approval_deadlines(&eid, escalate_at + Duration::seconds(offset))
                .await
                .unwrap();
        }
        // A restarted scheduler does not escalate again.
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, escalate_at + Duration::seconds(31))
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, escalated), 1, "{}: {evs:#?}", env.name);
        assert_eq!(
            count(&evs, expired),
            0,
            "{}: escalation is not expiry",
            env.name
        );
        let still_pending = jamjet_state::approvals::pending_approvals(&evs);
        assert_eq!(
            still_pending.len(),
            1,
            "{}: escalation leaves it pending",
            env.name
        );

        scheduler
            .sweep_approval_deadlines(&eid, expires_at)
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, expired), 1, "{}", env.name);
        assert_eq!(count(&evs, escalated), 1, "{}", env.name);
    }
}

/// A sweep that first runs after both times have passed expires the request
/// and does not escalate it: there is nothing left to escalate.
#[tokio::test]
async fn a_late_first_sweep_expires_without_escalating() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(
                Value::Null,
                gate(json!({ "approval_timeout_secs": 600, "escalate_after_secs": 60 })),
            ),
        )
        .await;
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, Utc::now() + Duration::days(1))
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, expired), 1, "{}", env.name);
        assert_eq!(count(&evs, escalated), 0, "{}", env.name);
    }
}

#[tokio::test]
async fn a_replayed_expiry_records_nothing() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 600 }))),
        )
        .await;
        let far = Utc::now() + Duration::days(1);
        for _ in 0..3 {
            Scheduler::new(env.backend.clone())
                .sweep_approval_deadlines(&eid, far)
                .await
                .unwrap();
        }
        assert_eq!(count(&events(&env, &eid).await, expired), 1, "{}", env.name);
    }
}

/// A clock behind the request (a sweep from a node whose clock lags) expires
/// nothing.
#[tokio::test]
async fn a_sweep_with_a_clock_behind_the_request_does_nothing() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(
                Value::Null,
                gate(json!({ "approval_timeout_secs": 600, "escalate_after_secs": 60 })),
            ),
        )
        .await;
        Scheduler::new(env.backend.clone())
            .sweep_approval_deadlines(&eid, Utc::now() - Duration::hours(1))
            .await
            .unwrap();
        let evs = events(&env, &eid).await;
        assert_eq!(
            count(&evs, expired) + count(&evs, escalated),
            0,
            "{}",
            env.name
        );
    }
}

/// The scheduler's own loop runs the sweep: a tick after the deadline
/// expires the request, the next fails the node, the next the run. The one
/// test that waits on the wall clock, since the loop reads it.
#[tokio::test]
async fn the_scheduler_tick_runs_the_sweep() {
    for env in envs().await {
        let eid = start_and_request(
            &env,
            ir(Value::Null, gate(json!({ "approval_timeout_secs": 1 }))),
        )
        .await;
        let scheduler = Scheduler::new(env.backend.clone());
        scheduler.tick().await.unwrap();
        assert_eq!(count(&events(&env, &eid).await, expired), 0, "{}", env.name);
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        // Expire, fail the node, then fail the run: one tick each.
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
        }
        let evs = events(&env, &eid).await;
        assert_eq!(count(&evs, expired), 1, "{}: {evs:#?}", env.name);
        assert!(
            evs.iter()
                .any(|e| matches!(e.kind, EventKind::WorkflowFailed { .. })),
            "{}",
            env.name
        );
    }
}

/// Counts full reads of a run's log.
#[derive(Default)]
struct CountReads(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl AfterWrite for CountReads {
    async fn after(&self, write: Write) {
        if let Write::GetEvents(_) = write {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// Review M-6: the tick reads a run's whole log for the deadline sweep only
/// when the run holds a request that carries a deadline. A run held on a
/// request with no deadline is not swept.
#[tokio::test]
async fn only_a_run_with_a_deadline_is_swept() {
    for (policy, want_sweep) in [
        (gate(json!({})), false),
        (gate(json!({ "approval_timeout_secs": 600 })), true),
    ] {
        let env = envs().await.remove(0);
        let eid = start_and_request(&env, ir(Value::Null, policy.clone())).await;
        assert!(!request_context(&events(&env, &eid).await).is_null());
        let reads = Arc::new(CountReads::default());
        let counted: Arc<dyn StateBackend> =
            Arc::new(HookedBackend::new(env.backend.clone(), reads.clone()));
        let scheduler = Scheduler::new(counted);
        for _ in 0..3 {
            scheduler.tick().await.unwrap();
        }
        let n = reads.0.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(n > 0, want_sweep, "{policy}: {n} full log reads in 3 ticks");
    }
}

async fn memory_env_with_tenant_policy(policy: Value) -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    let now = Utc::now();
    backend
        .create_tenant(Tenant {
            id: TenantId::default(),
            name: "default".into(),
            status: TenantStatus::Active,
            policy: Some(policy),
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    env_on("memory", backend as Arc<dyn StateBackend>).await
}

/// Review I-2: the tenant's deadline is a ceiling. Every level that sets a
/// timeout is a limit and the shortest wins; the same for escalation, which
/// is dropped (never the timeout extended) when it would not come first. A
/// workflow or node policy can only shorten.
#[tokio::test]
async fn a_lower_level_can_only_shorten_the_deadline() {
    let one_year = 31_536_000u64;
    for (tenant, workflow, node, want_expiry, want_escalate) in [
        // A one-year node policy neither lengthens the tenant's deadline nor
        // drops its escalation.
        (
            json!({ "require_approval_for": ["*"], "approval_timeout_secs": 100, "escalate_after_secs": 10 }),
            Value::Null,
            json!({ "approval_timeout_secs": one_year }),
            100,
            Some(10),
        ),
        (
            json!({ "require_approval_for": ["*"], "approval_timeout_secs": 100, "escalate_after_secs": 10 }),
            json!({ "approval_timeout_secs": one_year, "escalate_after_secs": one_year - 1 }),
            Value::Null,
            100,
            Some(10),
        ),
        // A lower level shortens: the shortest timeout and the shortest
        // escalation apply.
        (
            json!({ "require_approval_for": ["*"], "approval_timeout_secs": 100 }),
            json!({ "approval_timeout_secs": 200, "escalate_after_secs": 20 }),
            json!({ "approval_timeout_secs": 50 }),
            50,
            Some(20),
        ),
        // The shortest escalation would not come before the shortest timeout:
        // the escalation is dropped, the timeout stays.
        (
            json!({ "require_approval_for": ["*"], "approval_timeout_secs": 100, "escalate_after_secs": 90 }),
            Value::Null,
            json!({ "approval_timeout_secs": 30 }),
            30,
            None,
        ),
        // Only the tenant sets one.
        (
            json!({ "require_approval_for": ["*"], "approval_timeout_secs": 100 }),
            Value::Null,
            Value::Null,
            100,
            None,
        ),
    ] {
        let env = memory_env_with_tenant_policy(tenant.clone()).await;
        let before = Utc::now();
        let eid = start_and_request(&env, ir(workflow.clone(), node.clone())).await;
        let ctx = request_context(&events(&env, &eid).await);
        let case = format!("tenant {tenant} / workflow {workflow} / node {node}");
        close_to(
            ts(&ctx, "expires_at"),
            before + Duration::seconds(want_expiry),
            &format!("expires_at for {case}"),
        );
        match want_escalate {
            Some(s) => close_to(
                ts(&ctx, "escalate_at"),
                before + Duration::seconds(s),
                &format!("escalate_at for {case}"),
            ),
            None => assert!(ctx.get("escalate_at").is_none(), "{case}: {ctx}"),
        }
    }
}

#[tokio::test]
async fn registration_refuses_an_invalid_deadline() {
    let env = envs().await.remove(0);
    for (workflow, node) in [
        (json!({ "approval_timeout_secs": 0 }), Value::Null),
        (
            Value::Null,
            gate(json!({ "approval_timeout_secs": 60, "escalate_after_secs": 60 })),
        ),
        (
            Value::Null,
            gate(json!({ "approval_timeout_secs": 60, "escalate_after_secs": 61 })),
        ),
        (Value::Null, gate(json!({ "escalate_after_secs": 10 }))),
        (
            Value::Null,
            gate(json!({ "approval_timeout_secs": 60, "escalate_after_secs": 0 })),
        ),
        (
            json!({ "approval_timeout_secs": 400_000_000u64 }),
            Value::Null,
        ),
    ] {
        let (st, body) = call(
            &env.app,
            "POST",
            "/workflows",
            &env.alice,
            json!({ "ir": ir(workflow.clone(), node.clone()) }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{workflow} / {node}: {body}");
    }
}

#[tokio::test]
async fn registration_refuses_the_retired_timeouts_approval_timeout() {
    let env = envs().await.remove(0);
    let mut with_old = ir(Value::Null, gate(json!({})));
    with_old["timeouts"] = json!({ "approval_timeout": 600 });
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": with_old }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body.to_string().contains("approval_timeout_secs"),
        "the refusal names the setting to use: {body}"
    );
    // A null, as the SDK compilers emit, is accepted.
    let mut with_null = ir(Value::Null, gate(json!({})));
    with_null["timeouts"] = json!({ "approval_timeout": null });
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": with_null }),
    )
    .await;
    assert!(st.is_success(), "{body}");
}

/// Review M-1: a version already registered with the retired key (before
/// this release) re-registers idempotently, as deployments do on every boot;
/// only a NEW registration carrying it is refused.
#[tokio::test]
async fn an_identical_reregistration_with_the_retired_key_is_idempotent() {
    let env = envs().await.remove(0);
    let mut old = ir(Value::Null, gate(json!({})));
    old["timeouts"] = json!({ "approval_timeout": 600 });
    env.backend
        .store_workflow(jamjet_state::backend::WorkflowDefinition {
            workflow_id: "wf-deadline".into(),
            version: "0.1.0".into(),
            ir: old.clone(),
            created_at: Utc::now(),
            tenant_id: "default".into(),
        })
        .await
        .unwrap();
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": old.clone() }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let mut next = old;
    next["version"] = json!("0.2.0");
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": next }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_tenant_policy_with_an_invalid_deadline_is_refused() {
    let env = memory_env_with_tenant_policy(json!({})).await;
    for bad in [
        json!({ "approval_timeout_secs": 0 }),
        json!({ "approval_timeout_secs": 10, "escalate_after_secs": 10 }),
        json!({ "escalate_after_secs": 5 }),
        json!({ "approval_timeout_secs": "600" }),
    ] {
        let (st, body) = call(
            &env.app,
            "PUT",
            "/tenants/default",
            &env.ops,
            json!({ "policy": bad }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    let (st, body) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        json!({ "policy": { "approval_timeout_secs": 600, "escalate_after_secs": 60 } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}
