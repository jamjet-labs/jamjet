//! The worker protocol must not be a way around an approval gate.
//!
//! The approve route refuses the run's initiator (separation of duties), but
//! until this change the same token could reach the gated node through the
//! worker protocol instead: enqueue an item for it (`POST /work-items`), claim
//! it (`POST /work-items/claim` evaluated policy only for agent tool dispatch
//! nodes) and complete it (`POST /work-items/:id/complete` appended
//! `NodeCompleted` with no approval check). The scheduler treats that
//! `NodeCompleted` as the node having run, so downstream nodes ran too.
//!
//! Three guards close it, each tested on its own below:
//! - `POST /work-items` is operator-only (the engine is the only legitimate
//!   producer of work items);
//! - the claim route runs the same policy and approval decision as the
//!   in-process worker, for every node kind, on every queue;
//! - completion refuses a gated node whose current occurrence is not approved.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{Event, InMemoryBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

/// Every queue an attacker might name, including the in-process `tool` queue
/// the engine put the gated node on.
const QUEUES: &[&str] = &["tool", "general", "attacker_q", "python_tool", "model"];

fn gated_ir(policy: Value) -> Value {
    json!({
        "workflow_id": "wf-gate",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "gate",
        "nodes": {
            "gate": {
                "id": "gate",
                "kind": {
                    "type": "tool",
                    "tool_ref": "payments.transfer",
                    "input_mapping": {},
                    "output_schema": ""
                },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": policy
            }
        },
        "edges": [ { "from": "gate", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

fn approval_policy() -> Value {
    json!({ "blocked_tools": [], "require_approval_for": ["payments.*"], "model_allowlist": [] })
}

fn block_policy() -> Value {
    json!({ "blocked_tools": ["payments.*"], "require_approval_for": [], "model_allowlist": [] })
}

struct Env {
    backend: Arc<InMemoryBackend>,
    app: axum::Router,
    alice: String,
    bob: String,
    ops: String,
}

async fn env() -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let (ops, _) = backend.create_token("ops", "operator").await.unwrap();
    Env {
        backend,
        app,
        alice,
        bob,
        ops,
    }
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

fn parse_eid(s: &str) -> ExecutionId {
    let hex = s.strip_prefix("exec_").unwrap_or(s);
    ExecutionId(uuid::Uuid::parse_str(hex).unwrap())
}

/// Alice registers the workflow and starts the run, so she is its initiator.
async fn start(env: &Env, policy: Value) -> (String, ExecutionId) {
    start_with(env, policy, json!({ "amount": 10 })).await
}

async fn start_with(env: &Env, policy: Value, input: Value) -> (String, ExecutionId) {
    let (st, body) = send(
        &env.app,
        "/workflows",
        &env.alice,
        json!({ "ir": gated_ir(policy) }),
    )
    .await;
    assert!(st.is_success(), "register: {st} {body}");
    let (st, body) = send(
        &env.app,
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-gate", "workflow_version": "0.1.0", "input": input }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    let eid = parse_eid(&id);
    (id, eid)
}

/// The in-process worker parks the gate: it appends the approval request.
/// Done here through the engine's own path (claim on the gated item through the
/// route would be the external equivalent and is tested separately).
async fn park(env: &Env, eid: &ExecutionId) {
    let seq = env.backend.latest_sequence(eid).await.unwrap() + 1;
    env.backend
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
}

async fn events(env: &Env, eid: &ExecutionId) -> Vec<Event> {
    env.backend.get_events(eid).await.unwrap()
}

fn gate_completions(ev: &[Event]) -> usize {
    ev.iter()
        .filter(
            |e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "gate"),
        )
        .count()
}

fn approval_requests(ev: &[Event]) -> usize {
    ev.iter()
        .filter(|e| matches!(&e.kind, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "gate"))
        .count()
}

async fn claim(env: &Env, token: &str) -> Value {
    let (st, body) = send(
        &env.app,
        "/work-items/claim",
        token,
        json!({ "worker_id": "w", "queue_types": QUEUES }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

/// The reviewer's exact attack, end to end. The initiator is refused on the
/// approve route, then drives the gated node through the worker protocol.
/// Whatever each step answers, the gate must not complete.
#[tokio::test]
async fn the_initiator_cannot_complete_a_gated_node_through_the_worker_protocol() {
    let env = env().await;
    let (id, eid) = start(&env, approval_policy()).await;
    park(&env, &eid).await;

    // Separation of duties on the approve route.
    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.alice,
        json!({ "decision": "approved", "node_id": "gate" }),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");

    // 1. Enqueue the gated node on a queue of the attacker's choosing.
    let (enq_status, enq) = send(
        &env.app,
        "/work-items",
        &env.alice,
        json!({
            "execution_id": id,
            "node_id": "gate",
            "queue_type": "attacker_q",
            "payload": { "workflow_id": "wf-gate", "workflow_version": "0.1.0" },
        }),
    )
    .await;

    // 2. Claim from every queue, the in-process `tool` queue included.
    let claimed = claim(&env, &env.alice).await;

    // 3. Complete whatever is reachable: the claimed item with its fence, the
    //    enqueued item without one.
    let mut attempts: Vec<(String, Option<i64>)> = Vec::new();
    if claimed["claimed"] == true {
        attempts.push((
            claimed["work_item"]["id"].as_str().unwrap().to_string(),
            claimed["work_item"]["lease_fence"].as_i64(),
        ));
    }
    if let Some(item) = enq["work_item_id"].as_str() {
        attempts.push((item.to_string(), None));
    }
    for (item, fence) in attempts {
        let mut body = json!({
            "output": { "ok": true },
            "state_patch": { "amount": 1_000_000 },
        });
        if let Some(f) = fence {
            body["lease_fence"] = json!(f);
        }
        let _ = send(
            &env.app,
            &format!("/work-items/{item}/complete"),
            &env.alice,
            body,
        )
        .await;
    }

    let ev = events(&env, &eid).await;
    assert_eq!(
        gate_completions(&ev),
        0,
        "the gate completed without an approval (enqueue answered {enq_status})"
    );
    assert!(
        !ev.iter()
            .any(|e| matches!(e.kind, EventKind::ApprovalReceived { .. })),
        "no approval was ever recorded"
    );
}

/// Guard 1: only an operator may enqueue a work item by hand.
#[tokio::test]
async fn a_developer_cannot_enqueue_a_work_item() {
    let env = env().await;
    let (id, eid) = start(&env, approval_policy()).await;
    let body = json!({
        "execution_id": id,
        "node_id": "gate",
        "queue_type": "attacker_q",
        "payload": { "workflow_id": "wf-gate", "workflow_version": "0.1.0" },
    });
    let before = events(&env, &eid).await.len();

    let (st, resp) = send(&env.app, "/work-items", &env.alice, body.clone()).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{resp}");
    assert_eq!(
        events(&env, &eid).await.len(),
        before,
        "a refused enqueue must not schedule anything"
    );

    let (st, resp) = send(&env.app, "/work-items", &env.ops, body).await;
    assert_eq!(st, StatusCode::CREATED, "operator may enqueue: {resp}");
}

/// Guard 2: the claim route gates a plain gated node exactly like the worker:
/// no payload, one approval request, and the payload once approved.
#[tokio::test]
async fn an_external_claim_of_a_gated_node_is_held_until_approved() {
    let env = env().await;
    let (id, eid) = start(&env, approval_policy()).await;

    // The engine's own item for the start node sits on the in-process queue.
    let first = claim(&env, &env.alice).await;
    assert_eq!(
        first,
        json!({ "claimed": false }),
        "the gate was handed out"
    );
    let ev = events(&env, &eid).await;
    assert_eq!(approval_requests(&ev), 1, "the claim must request approval");
    let request_digest = ev
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => context
                .get("action_digest")
                .and_then(|d| d.as_str())
                .map(String::from),
            _ => None,
        })
        .expect("the request carries the action digest");

    // A second claim finds nothing and does not duplicate the request.
    assert_eq!(claim(&env, &env.alice).await["claimed"], false);
    assert_eq!(approval_requests(&events(&env, &eid).await), 1);

    // A different principal approves. The scheduler would now re-dispatch the
    // node; an operator re-enqueue stands in for it here.
    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "gate", "action_digest": request_digest }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    // Same payload as the engine dispatched: the approval binds it.
    let (st, _) = send(
        &env.app,
        "/work-items",
        &env.ops,
        json!({
            "execution_id": id,
            "node_id": "gate",
            "queue_type": "tool",
            "payload": { "workflow_id": "wf-gate", "workflow_version": "0.1.0", "node_id": "gate" },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let released = claim(&env, &env.alice).await;
    assert_eq!(released["claimed"], true, "an approved gate is released");
    assert_eq!(released["work_item"]["node_id"], "gate");

    // And it completes normally.
    let item = released["work_item"]["id"].as_str().unwrap();
    let (st, body) = send(
        &env.app,
        &format!("/work-items/{item}/complete"),
        &env.alice,
        json!({
            "output": { "ok": true },
            "state_patch": {},
            "lease_fence": released["work_item"]["lease_fence"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(gate_completions(&events(&env, &eid).await), 1);

    // The approval covered that occurrence only. A later item for the node
    // cannot be completed under it.
    let (st, enq) = send(
        &env.app,
        "/work-items",
        &env.ops,
        json!({
            "execution_id": id,
            "node_id": "gate",
            "queue_type": "attacker_q",
            "payload": { "workflow_id": "wf-gate", "workflow_version": "0.1.0" },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let again = enq["work_item_id"].as_str().unwrap();
    let (st, body) = send(
        &env.app,
        &format!("/work-items/{again}/complete"),
        &env.alice,
        json!({ "output": {}, "state_patch": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(gate_completions(&events(&env, &eid).await), 1);
}

/// Guard 3 is read-only: completing a gated item nobody asked about is
/// refused and does not create an approval request as a side effect.
#[tokio::test]
async fn completing_a_never_requested_gated_item_is_refused_without_requesting() {
    let env = env().await;
    let (_id, eid) = start(&env, approval_policy()).await;
    let item = env
        .backend
        .claim_work_item_with_lease("in-process", &["tool"], None)
        .await
        .unwrap()
        .expect("the start item");
    let (st, resp) = send(
        &env.app,
        &format!("/work-items/{}/complete", item.id),
        &env.alice,
        json!({ "output": {}, "state_patch": {}, "lease_fence": item.lease_fence }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{resp}");
    let ev = events(&env, &eid).await;
    assert_eq!(gate_completions(&ev), 0);
    assert_eq!(
        approval_requests(&ev),
        0,
        "a refused completion must not request approval"
    );
}

/// Guard 2, block arm: a blocked node is never handed out and the denial is
/// on the audit surface, as on the worker path.
#[tokio::test]
async fn an_external_claim_of_a_blocked_node_is_refused_and_audited() {
    let env = env().await;
    let (_id, eid) = start(&env, block_policy()).await;

    assert_eq!(claim(&env, &env.alice).await, json!({ "claimed": false }));
    let ev = events(&env, &eid).await;
    assert!(
        ev.iter().any(|e| matches!(
            &e.kind,
            EventKind::PolicyViolation { node_id, decision, .. } if node_id == "gate" && decision == "blocked"
        )),
        "the block must be recorded"
    );
    // Failed, not re-queued: a later claim still finds nothing.
    assert_eq!(claim(&env, &env.alice).await["claimed"], false);
}

/// Guard 3: completing the engine's own item for a gated node, without ever
/// claiming it through the route (the legacy unfenced path), is refused.
#[tokio::test]
async fn completing_an_unapproved_gated_item_is_refused() {
    let env = env().await;
    let (_id, eid) = start(&env, approval_policy()).await;
    park(&env, &eid).await;

    // Read the engine's item id the way a worker that leased it would.
    let item = env
        .backend
        .claim_work_item_with_lease("in-process", &["tool"], None)
        .await
        .unwrap()
        .expect("the start item");

    for fence in [None, Some(item.lease_fence)] {
        let mut body = json!({ "output": {}, "state_patch": { "amount": 1_000_000 } });
        if let Some(f) = fence {
            body["lease_fence"] = json!(f);
        }
        let (st, resp) = send(
            &env.app,
            &format!("/work-items/{}/complete", item.id),
            &env.alice,
            body,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "fence {fence:?}: {resp}");
        assert_eq!(resp["completed"], false);
    }
    assert_eq!(gate_completions(&events(&env, &eid).await), 0);
}

/// Guard 3 must not refuse ordinary work: an ungated node completes.
#[tokio::test]
async fn completing_an_ungated_item_still_works() {
    let env = env().await;
    let (_id, eid) = start(&env, json!(null)).await;
    let released = claim(&env, &env.alice).await;
    assert_eq!(released["claimed"], true, "{released}");
    let item = released["work_item"]["id"].as_str().unwrap();
    let (st, body) = send(
        &env.app,
        &format!("/work-items/{item}/complete"),
        &env.alice,
        json!({ "output": {}, "state_patch": {}, "lease_fence": released["work_item"]["lease_fence"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(gate_completions(&events(&env, &eid).await), 1);
}

// ── Approving what you saw (approval-integrity round 2, I2) ─────────────────
//
// Outside dev mode an `approved` decision must echo the binding of the pending
// request it answers (its `action_digest`, or `calls_hash` for an agent tool
// dispatch), so a reviewer never approves a request that replaced the one on
// their screen. And the pending listing shows what is bound: the state the node
// will run on and the node's definition.

async fn get(env: &Env, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = env
        .app
        .clone()
        .oneshot(
            Request::get(uri)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
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

/// Start a gated run and let the claim gate request approval.
async fn held_run(env: &Env, input: Value) -> (String, ExecutionId, Value) {
    let (id, eid) = start_with(env, approval_policy(), input).await;
    assert_eq!(claim(env, &env.alice).await["claimed"], false);
    let (st, view) = get(env, &format!("/executions/{id}/approvals"), &env.bob).await;
    assert_eq!(st, StatusCode::OK, "{view}");
    let pending = view["pending"][0].clone();
    assert_eq!(pending["node_id"], "gate", "{view}");
    (id, eid, pending)
}

fn decisions(ev: &[Event]) -> usize {
    ev.iter()
        .filter(|e| matches!(e.kind, EventKind::ApprovalReceived { .. }))
        .count()
}

#[tokio::test]
async fn an_approval_without_the_digest_is_refused_outside_dev_mode() {
    let env = env().await;
    let (id, eid, pending) = held_run(&env, json!({ "amount": 10 })).await;

    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "gate" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("action_digest"), "{body}");
    assert_eq!(decisions(&events(&env, &eid).await), 0);

    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.bob,
        json!({
            "decision": "approved",
            "node_id": "gate",
            "action_digest": pending["context"]["action_digest"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_rejection_needs_no_digest() {
    let env = env().await;
    let (id, _eid, _) = held_run(&env, json!({ "amount": 10 })).await;
    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.bob,
        json!({ "decision": "rejected", "node_id": "gate" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_mcp_bridge_requires_the_digest_outside_dev_mode() {
    let env = env().await;
    let (id, eid, _) = held_run(&env, json!({ "amount": 10 })).await;
    let (st, resp) = send(
        &env.app,
        "/mcp",
        &env.bob,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "jamjet_approve", "arguments": {
                "execution_id": id, "decision": "approved", "node_id": "gate",
            } },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{resp}");
    assert_eq!(resp["result"]["isError"], true, "{resp}");
    assert_eq!(decisions(&events(&env, &eid).await), 0);
}

/// An agent tool dispatch request binds `calls_hash`; echoing it is how a
/// reviewer approves one, and omitting it is refused like a missing digest.
#[tokio::test]
async fn a_dispatch_request_is_approved_by_echoing_its_calls_hash() {
    let env = env().await;
    let (id, eid) = start(&env, approval_policy()).await;
    let seq = env.backend.latest_sequence(&eid).await.unwrap() + 1;
    env.backend
        .append_event(Event::new(
            eid.clone(),
            seq,
            EventKind::ToolApprovalRequired {
                node_id: "gate".into(),
                tool_name: "send_wire".into(),
                approver: "human".into(),
                context: json!({ "node_id": "gate", "calls_hash": "h-123" }),
            },
        ))
        .await
        .unwrap();
    let approve = |digest: Option<&str>| {
        let mut body = json!({ "decision": "approved", "node_id": "gate" });
        if let Some(d) = digest {
            body["action_digest"] = json!(d);
        }
        body
    };
    let uri = format!("/executions/{id}/approve");
    let (st, body) = send(&env.app, &uri, &env.bob, approve(None)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    let (st, body) = send(&env.app, &uri, &env.bob, approve(Some("h-other"))).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    let (st, body) = send(&env.app, &uri, &env.bob, approve(Some("h-123"))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

/// A request recorded before bindings existed binds nothing, so there is
/// nothing to echo. Approving it is allowed: it authorises nothing by itself
/// (the worker asks again with a digest), and refusing would strand runs that
/// were parked before an upgrade.
#[tokio::test]
async fn a_legacy_request_without_a_binding_can_still_be_answered() {
    let env = env().await;
    let (id, eid) = start(&env, approval_policy()).await;
    park(&env, &eid).await;
    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "gate" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_pending_listing_shows_the_bound_payload_and_node() {
    let env = env().await;
    let (_id, _eid, pending) = held_run(&env, json!({ "amount": 10 })).await;
    let basis = &pending["context"]["action_basis"];
    // A tool node is dispatched without the run's state: its payload names the
    // workflow and the node, and that is exactly what the approval binds.
    assert_eq!(
        basis["payload"],
        json!({ "workflow_id": "wf-gate", "workflow_version": "0.1.0", "node_id": "gate" }),
        "{pending}"
    );
    assert_eq!(basis["payload_truncated"], false, "{pending}");
    assert_eq!(basis["step"], 0, "{pending}");
    assert_eq!(
        basis["node"]["kind"]["tool_ref"], "payments.transfer",
        "{pending}"
    );
    assert!(pending["context"]["action_digest"].is_string());
}

#[tokio::test]
async fn a_large_bound_payload_is_truncated_in_the_listing_not_in_the_digest() {
    let env = env().await;
    // A python_fn node is dispatched with the run's state as its input, so a
    // large state makes a large payload.
    let mut ir = gated_ir(
        json!({ "blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": [] }),
    );
    ir["workflow_id"] = json!("wf-gate-py");
    ir["nodes"]["gate"]["kind"] = json!({
        "type": "python_fn", "module": "myapp.payments", "function": "transfer", "output_schema": ""
    });
    let (st, body) = send(&env.app, "/workflows", &env.alice, json!({ "ir": ir })).await;
    assert!(st.is_success(), "{st} {body}");
    let big = "x".repeat(64 * 1024);
    let (st, body) = send(
        &env.app,
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-gate-py", "workflow_version": "0.1.0", "input": { "blob": big } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    assert_eq!(claim(&env, &env.alice).await["claimed"], false);
    let (st, view) = get(&env, &format!("/executions/{id}/approvals"), &env.bob).await;
    assert_eq!(st, StatusCode::OK, "{view}");
    let basis = &view["pending"][0]["context"]["action_basis"];
    assert_eq!(basis["payload_truncated"], true, "{basis}");
    assert!(
        basis["payload"].is_null(),
        "the preview is dropped, not cut mid-value"
    );
    assert!(basis["payload_bytes"].as_u64().unwrap() > 64 * 1024);
}

/// A request recorded before requests carried their occurrence step is matched
/// at completion by exact digest, and the digest covers the item's payload: an
/// approval of this payload lets the result be recorded, an approval of any
/// other payload does not.
#[tokio::test]
async fn a_legacy_request_is_matched_at_completion_on_the_item_payload() {
    for (covers_this_payload, expected) in [(true, StatusCode::OK), (false, StatusCode::CONFLICT)] {
        let env = env().await;
        let (_id, eid) = start(&env, approval_policy()).await;
        let item = env
            .backend
            .claim_work_item_with_lease("in-process", &["tool"], None)
            .await
            .unwrap()
            .expect("the start item");
        let node_def: jamjet_ir::workflow::NodeDef =
            serde_json::from_value(gated_ir(approval_policy())["nodes"]["gate"].clone()).unwrap();
        let definition =
            jamjet_state::approvals::definition_hash(&serde_json::to_value(&node_def).unwrap());
        let bound_payload = if covers_this_payload {
            item.payload.clone()
        } else {
            json!({ "workflow_id": "wf-gate", "workflow_version": "0.1.0", "node_id": "gate", "input": { "amount": 1 } })
        };
        let digest = jamjet_state::approvals::derive_action_digest(
            env.backend.as_ref(),
            &eid,
            "gate",
            &definition,
            &bound_payload,
        )
        .await
        .unwrap();
        for kind in [
            EventKind::ToolApprovalRequired {
                node_id: "gate".into(),
                tool_name: "payments.transfer".into(),
                approver: "human".into(),
                // No `action_step`: the shape before occurrences were recorded.
                context: json!({ "node_id": "gate", "action_digest": digest }),
            },
            serde_json::from_value(json!({
                "type": "approval_received",
                "node_id": "gate",
                "user_id": "bob",
                "decision": "approved",
                "comment": null,
                "state_patch": null,
                "approved_digest": digest,
            }))
            .unwrap(),
        ] {
            let seq = env.backend.latest_sequence(&eid).await.unwrap() + 1;
            env.backend
                .append_event(Event::new(eid.clone(), seq, kind))
                .await
                .unwrap();
        }

        let (st, resp) = send(
            &env.app,
            &format!("/work-items/{}/complete", item.id),
            &env.alice,
            json!({ "output": {}, "state_patch": {}, "lease_fence": item.lease_fence }),
        )
        .await;
        assert_eq!(
            st, expected,
            "covers_this_payload={covers_this_payload}: {resp}"
        );
    }
}

/// The legacy unfenced completion cannot show the item was claimed through the
/// gate, so outside dev mode it is refused for a gated node even when an
/// approval covers the occurrence. The fenced completion still works.
#[tokio::test]
async fn an_approved_gated_node_cannot_be_completed_without_a_fence() {
    let env = env().await;
    let (id, eid, pending) = held_run(&env, json!({ "amount": 10 })).await;
    let (st, body) = send(
        &env.app,
        &format!("/executions/{id}/approve"),
        &env.bob,
        json!({ "decision": "approved", "node_id": "gate", "action_digest": pending["context"]["action_digest"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _) = send(
        &env.app,
        "/work-items",
        &env.ops,
        json!({
            "execution_id": id,
            "node_id": "gate",
            "queue_type": "tool",
            "payload": { "workflow_id": "wf-gate", "workflow_version": "0.1.0", "node_id": "gate" },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let released = claim(&env, &env.alice).await;
    assert_eq!(released["claimed"], true, "{released}");
    let item = released["work_item"]["id"].as_str().unwrap();

    let (st, body) = send(
        &env.app,
        &format!("/work-items/{item}/complete"),
        &env.alice,
        json!({ "output": {}, "state_patch": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(gate_completions(&events(&env, &eid).await), 0);

    let (st, body) = send(
        &env.app,
        &format!("/work-items/{item}/complete"),
        &env.alice,
        json!({ "output": {}, "state_patch": {}, "lease_fence": released["work_item"]["lease_fence"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(gate_completions(&events(&env, &eid).await), 1);
}

/// The refusal is for gated nodes only: an ungated node's legacy unfenced
/// completion still records its result.
#[tokio::test]
async fn an_ungated_node_can_still_be_completed_without_a_fence() {
    let env = env().await;
    let (_id, eid) = start(&env, json!(null)).await;
    let released = claim(&env, &env.alice).await;
    assert_eq!(released["claimed"], true, "{released}");
    let item = released["work_item"]["id"].as_str().unwrap();
    let (st, body) = send(
        &env.app,
        &format!("/work-items/{item}/complete"),
        &env.alice,
        json!({ "output": {}, "state_patch": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(gate_completions(&events(&env, &eid).await), 1);
}

/// Dev mode keeps the legacy unfenced completion for an approved gated node.
#[tokio::test]
async fn dev_mode_still_accepts_an_unfenced_completion_of_an_approved_node() {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, true);
    let (st, body) = send(
        &app,
        "/workflows",
        "",
        json!({ "ir": gated_ir(approval_policy()) }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = send(
        &app,
        "/executions",
        "",
        json!({ "workflow_id": "wf-gate", "workflow_version": "0.1.0", "input": { "amount": 10 } }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    let eid = parse_eid(&id);
    let claim_body = json!({ "worker_id": "w", "queue_types": QUEUES });
    let (_, first) = send(&app, "/work-items/claim", "", claim_body.clone()).await;
    assert_eq!(first["claimed"], false, "{first}");
    let (st, body) = send(
        &app,
        &format!("/executions/{id}/approve"),
        "",
        json!({ "decision": "approved", "node_id": "gate" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _) = send(
        &app,
        "/work-items",
        "",
        json!({
            "execution_id": id,
            "node_id": "gate",
            "queue_type": "tool",
            "payload": { "workflow_id": "wf-gate", "workflow_version": "0.1.0", "node_id": "gate" },
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, released) = send(&app, "/work-items/claim", "", claim_body).await;
    assert_eq!(released["claimed"], true, "{released}");
    let item = released["work_item"]["id"].as_str().unwrap();
    let (st, body) = send(
        &app,
        &format!("/work-items/{item}/complete"),
        "",
        json!({ "output": {}, "state_patch": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ev = backend.get_events(&eid).await.unwrap();
    assert_eq!(gate_completions(&ev), 1);
}

/// `__budget` is the engine's bookkeeping, left out of the approval digest and
/// read back by the budget enforcer. A worker's reported `state_patch` must
/// not write it: that would be a hidden change to what later nodes receive and
/// a way to reset the run's budget.
#[tokio::test]
async fn an_external_result_cannot_write_engine_bookkeeping() {
    let env = env().await;
    let (_id, eid) = start(&env, json!(null)).await;
    let released = claim(&env, &env.alice).await;
    assert_eq!(released["claimed"], true, "{released}");
    let item = released["work_item"]["id"].as_str().unwrap();
    let (st, body) = send(
        &env.app,
        &format!("/work-items/{item}/complete"),
        &env.alice,
        json!({
            "output": {},
            "state_patch": { "amount": 11, "__budget": { "total_cost_usd": 0.0, "tool_call_count": 0 } },
            "lease_fence": released["work_item"]["lease_fence"],
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let patch = events(&env, &eid)
        .await
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::NodeCompleted { state_patch, .. } => Some(state_patch),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        patch,
        json!({ "amount": 11 }),
        "the engine key is dropped, the rest kept"
    );
    let state = jamjet_state::materialize(env.backend.as_ref(), &eid)
        .await
        .unwrap()
        .current_state;
    assert!(state.get("__budget").is_none(), "{state}");
    let exec = env.backend.get_execution(&eid).await.unwrap().unwrap();
    assert!(
        exec.current_state.get("__budget").is_none(),
        "{}",
        exec.current_state
    );
}
