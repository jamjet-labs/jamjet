//! A tenant policy is checked when it is written and fails closed when read.
//!
//! `PUT /tenants/:id` stored any JSON as the policy, and reading it used
//! `.ok()`, so a policy of the wrong shape (`"blocked_tools": "send_wire"`) or
//! with a misspelt field (`blocked_tool`) silently meant no tenant policy at
//! all: the one layer a developer cannot rewrite, gone without a word
//! (round-5 review, M-7).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{InMemoryBackend, Tenant, TenantId, TenantStatus};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

mod support;

fn tool_ir() -> Value {
    json!({
        "workflow_id": "wf-tool", "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "gate",
        "nodes": { "gate": {
            "id": "gate",
            "kind": { "type": "tool", "tool_ref": "payments.transfer", "input_mapping": {}, "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}, "policy": null
        } },
        "edges": [ { "from": "gate", "to": "end", "condition": null } ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

struct Env {
    backend: Arc<InMemoryBackend>,
    app: axum::Router,
    alice: String,
    ops: String,
}

async fn env() -> Env {
    let backend = Arc::new(InMemoryBackend::new());
    if backend
        .get_tenant(&TenantId::default())
        .await
        .unwrap()
        .is_none()
    {
        let now = chrono::Utc::now();
        backend
            .create_tenant(Tenant {
                id: TenantId::default(),
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
    let (ops, _) = backend.create_token("ops", "operator").await.unwrap();
    Env {
        backend,
        app,
        alice,
        ops,
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

#[tokio::test]
async fn a_malformed_tenant_policy_is_refused_when_written() {
    let env = env().await;
    for bad in [
        json!({ "blocked_tool": ["send_wire"] }),
        json!({ "blocked_tools": "send_wire" }),
        json!(["send_wire"]),
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
    let tenant = env
        .backend
        .get_tenant(&TenantId::default())
        .await
        .unwrap()
        .unwrap();
    assert!(tenant.policy.is_none(), "nothing was stored");
    let (st, body) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        json!({ "policy": { "blocked_tools": ["send_wire"] } }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "a well-formed policy is accepted: {body}"
    );
}

/// Write a malformed policy past the route, as an older build or a hand edit
/// would have left it.
async fn store_malformed_policy(env: &Env) {
    let mut t = env
        .backend
        .get_tenant(&TenantId::default())
        .await
        .unwrap()
        .unwrap();
    t.policy = Some(json!({ "blocked_tool": ["*"] }));
    env.backend.update_tenant(t).await.unwrap();
}

async fn start(env: &Env) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/workflows",
        &env.alice,
        json!({ "ir": tool_ir() }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        &env.alice,
        json!({ "workflow_id": "wf-tool", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

fn refused_for_the_policy(events: &[jamjet_state::Event]) -> bool {
    events.iter().any(|e| {
        matches!(&e.kind, EventKind::PolicyViolation { rule, .. } if rule.contains("tenant policy"))
    })
}

#[tokio::test]
async fn a_stored_malformed_policy_refuses_nodes_on_the_claim_route() {
    let env = env().await;
    store_malformed_policy(&env).await;
    let eid = start(&env).await;
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["tool"] }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "claimed": false }),
        "an unreadable tenant policy let the node out"
    );
    assert!(refused_for_the_policy(
        &env.backend.get_events(&eid).await.unwrap()
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_malformed_policy_refuses_nodes_in_process() {
    let env = env().await;
    store_malformed_policy(&env).await;
    let eid = start(&env).await;
    let base = env.backend.clone() as Arc<dyn StateBackend>;
    let sb = base.clone();
    let sched = tokio::spawn(async move {
        Scheduler::new(sb)
            .with_poll_interval(Duration::from_millis(10))
            .run()
            .await
    });
    let workers = jamjet_worker::default_pool(base).spawn();
    let mut events = Vec::new();
    for _ in 0..500 {
        events = env.backend.get_events(&eid).await.unwrap();
        if events.iter().any(|e| {
            matches!(
                e.kind,
                EventKind::NodeFailed { .. } | EventKind::NodeCompleted { .. }
            )
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    sched.abort();
    for w in workers {
        w.abort();
    }
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. })),
        "the node ran under an unreadable tenant policy"
    );
    assert!(
        refused_for_the_policy(&events),
        "{:#?}",
        events.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}

/// `{"policy": {}}` clears a stored policy; `{"policy": null}` (or no field)
/// keeps it. The refusal and the 400 say so, since an operator fixing a
/// malformed policy is told to use PUT.
#[tokio::test]
async fn an_empty_policy_clears_and_null_keeps_and_the_messages_say_so() {
    let env = env().await;
    store_malformed_policy(&env).await;
    let eid = start(&env).await;
    let (_, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        &env.alice,
        json!({ "worker_id": "w", "queue_types": ["tool"] }),
    )
    .await;
    assert_eq!(body, json!({ "claimed": false }));
    let rule = env
        .backend
        .get_events(&eid)
        .await
        .unwrap()
        .into_iter()
        .find_map(|e| match e.kind {
            EventKind::PolicyViolation { rule, .. } => Some(rule),
            _ => None,
        })
        .unwrap();
    assert!(rule.contains(r#"{"policy": {}}"#), "{rule}");

    let (st, body) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        json!({ "policy": "nope" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("clears"), "{body}");

    let (st, _) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        json!({ "policy": null }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let kept = env
        .backend
        .get_tenant(&TenantId::default())
        .await
        .unwrap()
        .unwrap();
    assert!(kept.policy.is_some(), "null keeps the stored policy");

    let (st, _) = call(
        &env.app,
        "PUT",
        "/tenants/default",
        &env.ops,
        json!({ "policy": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let cleared = env
        .backend
        .get_tenant(&TenantId::default())
        .await
        .unwrap()
        .unwrap();
    assert!(cleared
        .policy_set()
        .unwrap()
        .is_none_or(|p| p.blocked_tools.is_empty() && p.require_approval_for.is_empty()));
}
