//! The agent-tool-dispatch flag must not skip a node's own policy gate.
//!
//! A flagged python_fn went straight to the per-call dispatch guard, which
//! evaluates only the tool calls in its input. The node's own names (handler
//! and node id) were never checked, and with no calls the guard allowed it.
//! The flag is author-controlled, so a developer could put it on any
//! function and walk past a tenant `["*"]` gate (round-3 review, C1).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{InMemoryBackend, Tenant, TenantId, TenantStatus};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

mod support;

fn flagged_ir(workflow_id: &str, module: &str, function: &str, policy: Value) -> Value {
    json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "dispatch",
        "nodes": {
            "dispatch": {
                "id": "dispatch",
                "kind": {
                    "type": "python_fn",
                    "module": module,
                    "function": function,
                    "output_schema": "",
                    "agent_tool_dispatch": true
                },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {},
                "policy": policy
            }
        },
        "edges": [ { "from": "dispatch", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
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
    if backend
        .get_tenant(&TenantId::from("default"))
        .await
        .unwrap()
        .is_none()
    {
        let now = chrono::Utc::now();
        backend
            .create_tenant(Tenant {
                id: TenantId::from("default"),
                name: "default".into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .unwrap();
    }
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

async fn start(env: &Env, workflow_id: &str, input: Value) -> (String, ExecutionId) {
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        Some(json!({ "workflow_id": workflow_id, "workflow_version": "0.1.0", "input": input })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());
    (id, eid)
}

async fn claim(env: &Env) -> Value {
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        Some(json!({ "worker_id": "w", "queue_types": ["python_tool"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

async fn requests(env: &Env, eid: &ExecutionId) -> usize {
    env.backend
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. }))
        .count()
}

/// The reviewer's demonstration. A tenant `["*"]` gate, and a developer puts
/// the dispatch flag on an ordinary payments function.
#[tokio::test]
async fn the_dispatch_flag_cannot_walk_past_a_tenant_gate() {
    let env = env().await;
    let (st, body) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        Some(json!({ "policy": { "blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": [] } })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let (reg_status, reg_body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        Some(json!({ "ir": flagged_ir("wf-pay", "payments.transfer", "send", Value::Null) })),
    )
    .await;
    if reg_status.is_success() {
        let (_, eid) = start(
            &env,
            "wf-pay",
            json!({ "amount": 1_000_000, "to": "mallory" }),
        )
        .await;
        assert_eq!(
            claim(&env).await,
            json!({ "claimed": false }),
            "a flagged payments function was handed out past the tenant gate"
        );
        assert_eq!(requests(&env, &eid).await, 1);
    }
    assert_eq!(
        reg_status,
        StatusCode::BAD_REQUEST,
        "the flag on a non-dispatcher must be refused: {reg_body}"
    );
    assert!(
        reg_body.to_string().contains("agent_tool_dispatch"),
        "{reg_body}"
    );
}

/// A real dispatcher under a node-level gate: held even with no calls, and the
/// approval binds the whole payload the function runs on, not just the calls.
#[tokio::test]
async fn a_gated_dispatcher_is_held_and_its_approval_binds_the_full_payload() {
    let env = env().await;
    let policy =
        json!({ "blocked_tools": [], "require_approval_for": ["dispatch"], "model_allowlist": [] });
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        Some(json!({ "ir": flagged_ir("wf-adk", "jamjet.agents.tool_runtime", "dispatch_tool_calls", policy) })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let input = json!({ "tool_calls": [], "budget": 5 });
    let (id, eid) = start(&env, "wf-adk", input.clone()).await;

    assert_eq!(
        claim(&env).await,
        json!({ "claimed": false }),
        "empty calls must not bypass the node gate"
    );
    assert_eq!(requests(&env, &eid).await, 1);

    let (st, view) = call(
        &env.app,
        "GET",
        &format!("/executions/{id}/approvals"),
        &env.bob,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{view}");
    let pending = &view["pending"][0];
    assert_eq!(
        pending["context"]["action_basis"]["payload"]["input"], input,
        "the node approval binds the full input: {pending}"
    );
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/executions/{id}/approve"),
        &env.bob,
        Some(json!({ "decision": "approved", "node_id": "dispatch", "action_digest": pending["context"]["action_digest"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // A payload other than the reviewed one is not covered.
    let mut forged = env
        .backend
        .get_events(&eid)
        .await
        .unwrap()
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { context, .. } => {
                Some(context["action_basis"]["payload"].clone())
            }
            _ => None,
        })
        .unwrap();
    forged["input"]["budget"] = json!(5_000_000);
    let (st, _) = call(
        &env.app,
        "POST",
        "/work-items",
        &env.ops,
        Some(json!({ "execution_id": id, "node_id": "dispatch", "queue_type": "python_tool", "payload": forged })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(
        claim(&env).await["claimed"],
        false,
        "a changed input is not covered"
    );
    assert_eq!(requests(&env, &eid).await, 2);
}

/// Register the ADK dispatcher under `policy`, start it with `input`, let the
/// claim hold it, approve the node, and re-dispatch the reviewed payload.
/// Returns the run and the claim answer for the re-dispatched item.
async fn approve_node_then_redispatch(
    env: &Env,
    policy: Value,
    input: Value,
) -> (ExecutionId, Value) {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        Some(json!({ "ir": flagged_ir("wf-adk", "jamjet.agents.tool_runtime", "dispatch_tool_calls", policy) })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (id, eid) = start(env, "wf-adk", input).await;
    assert_eq!(claim(env).await["claimed"], false);
    let (_, view) = call(
        &env.app,
        "GET",
        &format!("/executions/{id}/approvals"),
        &env.bob,
        None,
    )
    .await;
    let pending = view["pending"][0].clone();
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/executions/{id}/approve"),
        &env.bob,
        Some(json!({ "decision": "approved", "node_id": "dispatch", "action_digest": pending["context"]["action_digest"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, _) = call(
        &env.app,
        "POST",
        "/work-items",
        &env.ops,
        Some(json!({
            "execution_id": id, "node_id": "dispatch", "queue_type": "python_tool",
            "payload": pending["context"]["action_basis"]["payload"],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    (eid, claim(env).await)
}

fn send_wire_call() -> Value {
    json!({ "tool_calls": [ { "id": "c1", "name": "send_wire", "arguments": { "amount": 10 } } ] })
}

/// The node approval binds the full payload, calls included, so a call that is
/// also gated needs no second approval.
#[tokio::test]
async fn a_node_approval_covers_the_calls_in_its_payload() {
    let env = env().await;
    let policy = json!({ "blocked_tools": [], "require_approval_for": ["dispatch", "send_wire"], "model_allowlist": [] });
    let (eid, released) = approve_node_then_redispatch(&env, policy, send_wire_call()).await;
    assert_eq!(released["claimed"], true, "{released}");
    assert_eq!(
        requests(&env, &eid).await,
        1,
        "one approval, not one per layer"
    );
}

/// A per-call block still applies after the node itself was approved.
#[tokio::test]
async fn a_blocked_call_stays_blocked_after_a_node_approval() {
    let env = env().await;
    let policy = json!({ "blocked_tools": ["send_wire"], "require_approval_for": ["dispatch"], "model_allowlist": [] });
    let (eid, released) = approve_node_then_redispatch(&env, policy, send_wire_call()).await;
    assert_eq!(released, json!({ "claimed": false }), "{released}");
    let events = env.backend.get_events(&eid).await.unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.kind, EventKind::PolicyViolation { decision, .. } if decision == "blocked")),
        "the block is recorded"
    );
}

/// A flagged non-dispatcher stored before registration refused the flag is
/// still held by a tenant gate: the guard checks the node's own names first.
#[tokio::test]
async fn a_stored_flagged_function_is_still_held_by_a_tenant_gate() {
    let env = env().await;
    let (st, body) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        Some(json!({ "policy": { "blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": [] } })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    env.backend
        .store_workflow(jamjet_state::WorkflowDefinition {
            workflow_id: "wf-pay".into(),
            version: "0.1.0".into(),
            ir: flagged_ir("wf-pay", "payments.transfer", "send", Value::Null),
            created_at: chrono::Utc::now(),
            tenant_id: "default".into(),
        })
        .await
        .unwrap();
    let (_, eid) = start(
        &env,
        "wf-pay",
        json!({ "amount": 1_000_000, "to": "mallory" }),
    )
    .await;
    assert_eq!(claim(&env).await, json!({ "claimed": false }));
    assert_eq!(requests(&env, &eid).await, 1);
}

// ── The dispatcher resolves call names through the run's `tools` map ─────────
//
// `jamjet.agents.tool_runtime` imports `tools[name]` (a `module:function` ref
// from the run's state) when the map has an entry for the call, and only
// otherwise looks the name up in its registry. Per-call policy matched the
// call's NAME only, so a blocked tool ran under another name (round-4 review,
// I1). A call is now matched on its name and on the handler it resolves to.

async fn set_tenant_policy(env: &Env, blocked: &[&str], approve: &[&str]) {
    let (st, body) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        Some(json!({ "policy": { "blocked_tools": blocked, "require_approval_for": approve, "model_allowlist": [] } })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

async fn register_dispatcher(env: &Env) {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        Some(json!({ "ir": flagged_ir("wf-adk", "jamjet.agents.tool_runtime", "dispatch_tool_calls", Value::Null) })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
}

fn lookup_call(tools: Value) -> Value {
    json!({
        "tool_calls": [ { "id": "1", "name": "lookup", "arguments": { "to": "mallory" } } ],
        "tools": tools,
    })
}

/// The reviewer's demonstration.
#[tokio::test]
async fn a_blocked_tool_cannot_run_under_another_name() {
    let env = env().await;
    set_tenant_policy(&env, &["send_wire"], &[]).await;
    register_dispatcher(&env).await;
    let (_, eid) = start(
        &env,
        "wf-adk",
        lookup_call(json!({ "lookup": "bank.wires:send_wire" })),
    )
    .await;
    assert_eq!(
        claim(&env).await,
        json!({ "claimed": false }),
        "a blocked tool ran under another name through input.tools"
    );
    let events = env.backend.get_events(&eid).await.unwrap();
    assert!(events.iter().any(
        |e| matches!(&e.kind, EventKind::PolicyViolation { decision, .. } if decision == "blocked")
    ));
}

/// Gating works the same way, and the approval names the resolved handler.
#[tokio::test]
async fn a_gate_on_the_handler_holds_the_call_and_binds_it() {
    let env = env().await;
    set_tenant_policy(&env, &[], &["bank.wires.*"]).await;
    register_dispatcher(&env).await;
    let (id, _) = start(
        &env,
        "wf-adk",
        lookup_call(json!({ "lookup": "bank.wires:send_wire" })),
    )
    .await;
    assert_eq!(claim(&env).await, json!({ "claimed": false }));
    let (_, view) = call(
        &env.app,
        "GET",
        &format!("/executions/{id}/approvals"),
        &env.bob,
        None,
    )
    .await;
    let calls = &view["pending"][0]["context"]["calls"];
    assert_eq!(calls[0]["name"], "lookup", "{view}");
    assert_eq!(calls[0]["handler"], "bank.wires:send_wire", "{view}");
}

/// A map entry that cannot be read as `module:function` fails closed.
#[tokio::test]
async fn an_unreadable_tools_map_entry_blocks() {
    for tools in [
        json!({ "lookup": 7 }),
        json!({ "lookup": "no_colon" }),
        json!("not a map"),
    ] {
        let env = env().await;
        set_tenant_policy(&env, &["send_wire"], &[]).await;
        register_dispatcher(&env).await;
        start(&env, "wf-adk", lookup_call(tools.clone())).await;
        assert_eq!(claim(&env).await, json!({ "claimed": false }), "{tools}");
    }
}

/// Control: with no map entry the registry resolves the call by its name,
/// which is matched, so an unrelated call still runs.
#[tokio::test]
async fn a_call_with_no_map_entry_is_matched_on_its_name() {
    let env = env().await;
    set_tenant_policy(&env, &["send_wire"], &[]).await;
    register_dispatcher(&env).await;
    start(
        &env,
        "wf-adk",
        lookup_call(json!({ "other": "bank.wires:send_wire" })),
    )
    .await;
    assert_eq!(claim(&env).await["claimed"], true);
}

/// An attribute path past the function (`send_wire.__call__`) or a dunder
/// module segment cannot be matched reliably, so the engine refuses it as
/// unverifiable (round-5 review, I-1). Python refuses it too; this is the
/// engine's own line.
#[tokio::test]
async fn an_underscore_attribute_path_in_the_tools_map_blocks() {
    for (entry, label) in [
        ("bank.wires:send_wire.__call__", "dunder attribute"),
        ("bank.wires:send_wire.__wrapped__", "wrapped"),
        ("bank.wires:_send_wire", "private function"),
        ("bank.__init__:send_wire", "dunder module segment"),
    ] {
        let env = env().await;
        set_tenant_policy(&env, &["send_wire"], &[]).await;
        register_dispatcher(&env).await;
        let (_, eid) = start(&env, "wf-adk", lookup_call(json!({ "lookup": entry }))).await;
        assert_eq!(
            claim(&env).await,
            json!({ "claimed": false }),
            "{label}: {entry}"
        );
        let events = env.backend.get_events(&eid).await.unwrap();
        assert!(
            events.iter().any(|e| matches!(&e.kind, EventKind::PolicyViolation { rule, .. } if rule.contains("tools entry"))),
            "{label}: the refusal names the tools entry"
        );
    }
}

/// Control: an ordinary module path, including a private module, is accepted.
#[tokio::test]
async fn an_ordinary_or_private_module_path_is_accepted() {
    let env = env().await;
    set_tenant_policy(&env, &["send_wire"], &[]).await;
    register_dispatcher(&env).await;
    start(
        &env,
        "wf-adk",
        lookup_call(json!({ "lookup": "myapp._tools:lookup_rate" })),
    )
    .await;
    assert_eq!(claim(&env).await["claimed"], true);
}
