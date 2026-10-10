//! An approval binds the exact input the node is dispatched with.
//!
//! The scheduler builds a node's `payload.input` with a shallow top-level fold
//! of the run's state; the materializer deep-merges (RFC 7396). The digest and
//! the reviewer's view used to come from the materialized state, so with input
//! `{limits: {max: 100}}` and an upstream node writing `{limits: {}}`, the
//! reviewer was shown and approved `limits.max = 100` while the function ran
//! with no limit (review I1). The action basis is now the dispatched payload
//! itself.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::InMemoryBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

mod support;

fn ir() -> Value {
    let node = |id: &str, module: &str, function: &str, policy: Value| {
        json!({
            "id": id,
            "kind": { "type": "python_fn", "module": module, "function": function, "output_schema": "" },
            "retry_policy": null,
            "node_timeout_secs": null,
            "description": null,
            "labels": {},
            "policy": policy
        })
    };
    json!({
        "workflow_id": "wf-limits",
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "a",
        "nodes": {
            "a": node("a", "myapp.steps", "relax", Value::Null),
            "gate": node("gate", "myapp.payments", "transfer", json!({
                "blocked_tools": [], "require_approval_for": ["myapp.payments:*"], "model_allowlist": []
            })),
        },
        "edges": [
            { "from": "a", "to": "gate", "condition": null },
            { "from": "gate", "to": "end", "condition": null }
        ],
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
    scheduler: tokio::task::JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.scheduler.abort();
    }
}

async fn env() -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    let state = support::make_state(backend.clone() as Arc<dyn StateBackend>);
    let app = build_router_with_opts(state, false);
    let (alice, _) = backend.create_token("alice", "developer").await.unwrap();
    let (bob, _) = backend.create_token("bob", "reviewer").await.unwrap();
    let (ops, _) = backend.create_token("ops", "operator").await.unwrap();
    let sched_backend = backend.clone() as Arc<dyn StateBackend>;
    let scheduler = tokio::spawn(async move {
        Scheduler::new(sched_backend)
            .with_poll_interval(Duration::from_millis(10))
            .run()
            .await
    });
    Env {
        backend,
        app,
        alice,
        bob,
        ops,
        scheduler,
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

async fn gate_requests(env: &Env, eid: &ExecutionId) -> usize {
    env.backend
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "gate"))
        .count()
}

/// Claim until `pred` holds for the answer, letting the scheduler dispatch.
async fn claim_until(env: &Env, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..300 {
        let answer = claim(env).await;
        if pred(&answer) {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Start the run, run `a` (it empties `limits`), and let the gate request
/// approval. Returns the run and the pending request.
async fn held_gate(env: &Env) -> (String, ExecutionId, Value) {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        Some(json!({ "ir": ir() })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        Some(json!({
            "workflow_id": "wf-limits",
            "workflow_version": "0.1.0",
            "input": { "limits": { "max": 100 } },
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());

    let a = claim_until(env, "node a", |c| c["claimed"] == true).await;
    assert_eq!(a["work_item"]["node_id"], "a");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!(
            "/work-items/{}/complete",
            a["work_item"]["id"].as_str().unwrap()
        ),
        &env.alice,
        Some(json!({
            "output": {},
            "state_patch": { "limits": {} },
            "lease_fence": a["work_item"]["lease_fence"],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    for _ in 0..300 {
        assert_eq!(
            claim(env).await["claimed"],
            false,
            "the gate was handed out"
        );
        if gate_requests(env, &eid).await > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(gate_requests(env, &eid).await, 1);

    let (st, view) = call(
        &env.app,
        "GET",
        &format!("/executions/{id}/approvals"),
        &env.bob,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{view}");
    (id, eid, view["pending"][0].clone())
}

/// The reviewer's demonstration, end to end.
#[tokio::test]
async fn the_reviewer_sees_and_approves_the_input_the_node_runs_on() {
    let env = env().await;
    let (id, _eid, pending) = held_gate(&env).await;

    // What the reviewer is shown is the input the node will be called with.
    let basis = &pending["context"]["action_basis"];
    assert_eq!(
        basis["payload"]["input"],
        json!({ "limits": {} }),
        "the listing must show the dispatched input: {pending}"
    );
    assert_eq!(basis["payload"]["module"], "myapp.payments", "{pending}");

    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/executions/{id}/approve"),
        &env.bob,
        Some(json!({
            "decision": "approved",
            "node_id": "gate",
            "action_digest": pending["context"]["action_digest"],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    // And exactly that payload runs.
    let released = claim_until(&env, "the approved gate", |c| c["claimed"] == true).await;
    assert_eq!(released["work_item"]["node_id"], "gate");
    assert_eq!(
        released["work_item"]["payload"]["input"],
        json!({ "limits": {} })
    );
}

/// An item for the approved node whose payload is not the one reviewed is not
/// covered by the approval: it is held and asks again.
#[tokio::test]
async fn an_item_with_a_different_payload_is_not_covered() {
    let env = env().await;
    let (id, eid, pending) = held_gate(&env).await;
    // Stop the scheduler before approving, so the engine never dispatches the
    // reviewed item and the only item for the gate is the forged one.
    env.scheduler.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/executions/{id}/approve"),
        &env.bob,
        Some(json!({
            "decision": "approved",
            "node_id": "gate",
            "action_digest": pending["context"]["action_digest"],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items",
        &env.ops,
        Some(json!({
            "execution_id": id,
            "node_id": "gate",
            "queue_type": "python_tool",
            "payload": {
                "workflow_id": "wf-limits",
                "workflow_version": "0.1.0",
                "node_id": "gate",
                "module": "myapp.payments",
                "function": "transfer",
                "input": { "limits": { "max": 1_000_000_000 } },
            },
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let mut released = None;
    for _ in 0..5 {
        let c = claim(&env).await;
        if c["claimed"] == true && c["work_item"]["node_id"] == "gate" {
            released = Some(c);
            break;
        }
    }
    assert!(
        released.is_none(),
        "a payload nobody reviewed ran under the approval: {released:?}"
    );
    assert_eq!(gate_requests(&env, &eid).await, 2, "it asks again");
}

/// The input changes after the decision and before the scheduler re-dispatches
/// the node: the new item's payload is not the reviewed one, so it is held and
/// asks again, and the second approval releases the new input.
#[tokio::test]
async fn an_input_changed_after_approval_asks_again() {
    let mut env = env().await;
    let (id, eid, pending) = held_gate(&env).await;
    // Pause the engine, decide, then move the input under it.
    env.scheduler.abort();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/executions/{id}/approve"),
        &env.bob,
        Some(json!({
            "decision": "approved",
            "node_id": "gate",
            "action_digest": pending["context"]["action_digest"],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let seq = env.backend.latest_sequence(&eid).await.unwrap() + 1;
    env.backend
        .append_event(jamjet_state::Event::new(
            eid.clone(),
            seq,
            EventKind::NodeCompleted {
                node_id: "sibling".into(),
                output: json!({}),
                state_patch: json!({ "limits": { "max": 1_000_000_000 } }),
                duration_ms: 0,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: None,
            },
        ))
        .await
        .unwrap();
    let sched_backend = env.backend.clone() as Arc<dyn StateBackend>;
    env.scheduler = tokio::spawn(async move {
        Scheduler::new(sched_backend)
            .with_poll_interval(Duration::from_millis(10))
            .run()
            .await
    });

    for _ in 0..300 {
        let c = claim(&env).await;
        assert_ne!(
            c["work_item"]["node_id"], "gate",
            "the gate ran on an input nobody approved: {c}"
        );
        if gate_requests(&env, &eid).await == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(gate_requests(&env, &eid).await, 2, "it asks again");

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
        pending["context"]["action_basis"]["payload"]["input"],
        json!({ "limits": { "max": 1_000_000_000 } }),
        "{pending}"
    );
}
