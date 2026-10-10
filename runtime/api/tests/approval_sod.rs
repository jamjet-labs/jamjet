//! Separation of duties: whoever started a run may not approve its actions.
//!
//! Nothing recorded who started a run, so the same token could start a run and
//! approve every gate in it, which makes the approval a formality. The run's
//! initiator is now recorded on `WorkflowStarted.initiated_by`, and an
//! `Approved` decision by that same principal is refused with 403. Rejecting
//! your own run is always allowed: refusing to act is never the risk.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::{StateBackend, WorkflowDefinition};
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

fn make_state(backend: Arc<InMemoryBackend>) -> AppState {
    let backend_clone = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    AppState {
        backend: backend.clone() as Arc<dyn StateBackend>,
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| {
            backend_clone.clone() as Arc<dyn StateBackend>
        }),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        enricher: Arc::new(AuditEnricher::new(Arc::clone(&audit))),
        audit,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    }
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

async fn store_workflow(backend: &Arc<InMemoryBackend>) {
    backend
        .store_workflow(WorkflowDefinition {
            workflow_id: "wf".into(),
            version: "0.1.0".into(),
            // A loadable IR: starting a run builds the start node's payload.
            ir: support::python_fn_start_ir("wf"),
            created_at: chrono::Utc::now(),
            tenant_id: "default".into(),
        })
        .await
        .unwrap();
}

/// Start a run through the real route as `token`, then park node `gate`.
async fn start_and_park(
    app: axum::Router,
    backend: &Arc<InMemoryBackend>,
    token: Option<&str>,
) -> ExecutionId {
    let (status, body) = send(
        app,
        "POST",
        "/executions",
        token,
        json!({ "workflow_id": "wf", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let eid = parse_eid(body["execution_id"].as_str().unwrap());
    let seq = backend.latest_sequence(&eid).await.unwrap() + 1;
    backend
        .append_event(Event::new(
            eid.clone(),
            seq,
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

fn parse_eid(s: &str) -> ExecutionId {
    let hex = s.strip_prefix("exec_").unwrap_or(s);
    ExecutionId(uuid::Uuid::parse_str(hex).unwrap())
}

async fn started_json(backend: &Arc<InMemoryBackend>, eid: &ExecutionId) -> Value {
    let events = backend.get_events(eid).await.unwrap();
    let e = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::WorkflowStarted { .. }))
        .expect("WorkflowStarted");
    serde_json::to_value(&e.kind).unwrap()
}

async fn decisions(backend: &Arc<InMemoryBackend>, eid: &ExecutionId) -> usize {
    backend
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .count()
}

async fn approve(
    app: axum::Router,
    eid: &ExecutionId,
    token: Option<&str>,
    decision: &str,
) -> StatusCode {
    send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        token,
        json!({ "decision": decision }),
    )
    .await
    .0
}

#[tokio::test]
async fn the_run_records_who_started_it() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, info) = backend.create_token("alice", "developer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let eid = start_and_park(app, &backend, Some(&alice)).await;

    assert_eq!(
        started_json(&backend, &eid).await["initiated_by"],
        format!("token:{}", info.id)
    );
}

#[tokio::test]
async fn the_initiator_cannot_approve_their_own_run() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);
    let eid = start_and_park(app.clone(), &backend, Some(&alice)).await;

    let status = approve(app, &eid, Some(&alice), "approved").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(decisions(&backend, &eid).await, 0, "no event on refusal");
}

#[tokio::test]
async fn the_initiator_cannot_approve_through_the_mcp_bridge_either() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);
    let eid = start_and_park(app.clone(), &backend, Some(&alice)).await;

    let (_, resp) = send(
        app,
        "POST",
        "/mcp",
        Some(&alice),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_approve", "arguments": {
                "execution_id": eid.to_string(), "decision": "approved"
            }},
        }),
    )
    .await;

    assert_eq!(resp["result"]["isError"], true, "{resp}");
    assert_eq!(decisions(&backend, &eid).await, 0);
}

#[tokio::test]
async fn a_run_started_through_the_mcp_bridge_records_its_initiator() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, info) = backend.create_token("alice", "developer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (_, resp) = send(
        app,
        "POST",
        "/mcp",
        Some(&alice),
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_run_workflow", "arguments": {
                "workflow_id": "wf", "workflow_version": "0.1.0", "input": {}
            }},
        }),
    )
    .await;
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let eid = parse_eid(
        serde_json::from_str::<Value>(text).unwrap()["execution_id"]
            .as_str()
            .unwrap(),
    );

    assert_eq!(
        started_json(&backend, &eid).await["initiated_by"],
        format!("token:{}", info.id)
    );
}

#[tokio::test]
async fn a_different_token_can_approve() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);
    let eid = start_and_park(app.clone(), &backend, Some(&alice)).await;

    let status = approve(app, &eid, Some(&bob), "approved").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(decisions(&backend, &eid).await, 1);
}

#[tokio::test]
async fn a_second_token_with_the_same_name_is_still_a_different_principal() {
    // SoD keys on the token id, not the display name.
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice1, _) = backend.create_token("alice", "developer").await.unwrap();
    let (alice2, _) = backend.create_token("alice", "developer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);
    let eid = start_and_park(app.clone(), &backend, Some(&alice1)).await;

    assert_eq!(
        approve(app, &eid, Some(&alice2), "approved").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_initiator_may_reject_their_own_run() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);
    let eid = start_and_park(app.clone(), &backend, Some(&alice)).await;

    let status = approve(app, &eid, Some(&alice), "rejected").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(decisions(&backend, &eid).await, 1);
}

#[tokio::test]
async fn dev_mode_records_no_initiator_and_does_not_enforce() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let app = build_router_with_opts(make_state(backend.clone()), true);
    let eid = start_and_park(app.clone(), &backend, None).await;

    assert!(started_json(&backend, &eid)
        .await
        .get("initiated_by")
        .is_none());
    assert_eq!(approve(app, &eid, None, "approved").await, StatusCode::OK);
}

#[tokio::test]
async fn the_opt_out_lets_the_initiator_approve() {
    let backend = Arc::new(InMemoryBackend::new());
    store_workflow(&backend).await;
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let mut state = make_state(backend.clone());
    state.approval_policy = jamjet_api::approvals::ApprovalPolicy {
        allow_self_approval: true,
    };
    let app = build_router_with_opts(state, false);
    let eid = start_and_park(app.clone(), &backend, Some(&alice)).await;

    assert_eq!(
        approve(app, &eid, Some(&alice), "approved").await,
        StatusCode::OK
    );
    assert_eq!(decisions(&backend, &eid).await, 1);
}

#[test]
fn the_opt_out_is_off_unless_explicitly_set() {
    // Default (and the env reader's fallback) enforce separation of duties.
    assert!(!jamjet_api::approvals::ApprovalPolicy::default().allow_self_approval);
    // No test in this binary sets the variable, so this reads the unset case.
    std::env::remove_var("JAMJET_ALLOW_SELF_APPROVAL");
    assert!(!jamjet_api::approvals::ApprovalPolicy::from_env().allow_self_approval);
}

// ── Runs with no recorded initiator (approval-integrity round 2, I3) ────────
//
// A run started in dev mode, or logged before initiators were recorded, has no
// initiator, so separation of duties cannot be proven for it. Outside dev mode
// an approval of such a run is refused unless the self-approval opt-out is set;
// before this, the check was simply skipped and the run stayed self-approvable
// for life.

/// A run started on the dev-mode router (no initiator), then decided on the
/// auth router against the same backend, as after a restart into auth mode.
async fn run_without_initiator(backend: &Arc<InMemoryBackend>) -> ExecutionId {
    store_workflow(backend).await;
    let dev = build_router_with_opts(make_state(backend.clone()), true);
    let eid = start_and_park(dev, backend, None).await;
    assert!(started_json(backend, &eid)
        .await
        .get("initiated_by")
        .is_none());
    eid
}

#[tokio::test]
async fn approving_a_run_with_no_initiator_is_refused_outside_dev_mode() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = run_without_initiator(&backend).await;
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);

    let (status, body) = send(
        app,
        "POST",
        &format!("/executions/{eid}/approve"),
        Some(&bob),
        json!({ "decision": "approved", "node_id": "gate" }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let msg = body.to_string();
    assert!(
        msg.contains("JAMJET_ALLOW_SELF_APPROVAL"),
        "names the escape hatch: {msg}"
    );
    assert!(msg.contains("initiator"), "says why: {msg}");
    assert_eq!(decisions(&backend, &eid).await, 0);
}

#[tokio::test]
async fn rejecting_a_run_with_no_initiator_is_allowed() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = run_without_initiator(&backend).await;
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let app = build_router_with_opts(make_state(backend.clone()), false);
    assert_eq!(
        approve(app, &eid, Some(&bob), "rejected").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_opt_out_allows_approving_a_run_with_no_initiator() {
    let backend = Arc::new(InMemoryBackend::new());
    let eid = run_without_initiator(&backend).await;
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let mut state = make_state(backend.clone());
    state.approval_policy = jamjet_api::approvals::ApprovalPolicy {
        allow_self_approval: true,
    };
    let app = build_router_with_opts(state, false);
    assert_eq!(
        approve(app, &eid, Some(&bob), "approved").await,
        StatusCode::OK
    );
}

/// Outside dev mode a decision with no authenticated principal is refused
/// outright, whatever the self-approval policy. Both transports always supply
/// one, so this is unreachable today; it must stay refused if a new caller
/// forgets, rather than fall through to "nobody to compare, allow".
#[tokio::test]
async fn a_decision_without_a_principal_is_refused_outside_dev_mode() {
    use jamjet_api::approvals::{submit_approval, ApprovalPolicy, ApprovalSubmission, SubmitError};
    use jamjet_state::event::ApprovalDecision;
    for (decision, allow_self_approval) in [
        (ApprovalDecision::Approved, false),
        (ApprovalDecision::Approved, true),
        (ApprovalDecision::Rejected, false),
    ] {
        let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
        let eid = ExecutionId::new();
        let now = chrono::Utc::now();
        backend
            .create_execution(jamjet_core::workflow::WorkflowExecution {
                execution_id: eid.clone(),
                workflow_id: "wf".into(),
                workflow_version: "0.1.0".into(),
                status: jamjet_core::workflow::WorkflowStatus::Running,
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
                initiated_by: Some("token:alice".into()),
            },
            EventKind::ToolApprovalRequired {
                node_id: "gate".into(),
                tool_name: "t".into(),
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
        let result = submit_approval(
            &backend,
            &eid,
            ApprovalSubmission {
                node_id: Some("gate".into()),
                user_id: "nobody".into(),
                principal: None,
                decision: decision.clone(),
                comment: None,
                state_patch: None,
                action_digest: None,
                dev_mode: false,
            },
            ApprovalPolicy {
                allow_self_approval,
            },
        )
        .await;
        assert!(
            matches!(result, Err(SubmitError::PrincipalMissing)),
            "{decision:?} allow_self_approval={allow_self_approval}: {result:?}"
        );
        let decided = backend
            .get_events(&eid)
            .await
            .unwrap()
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
            .count();
        assert_eq!(decided, 0);
    }
}
