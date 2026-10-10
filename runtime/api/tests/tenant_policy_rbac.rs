//! A tenant-level approval gate must bind the developers who start runs.
//!
//! Workflow and node policies are written by the same developer role that
//! starts runs, and a developer can always register a new version without a
//! gate. The tenant policy is the one layer that can bind an initiator against
//! their will, so writing it must be out of that role's reach (review C1): the
//! tenant routes are operator-only, and an operator may only touch the tenant
//! their token belongs to.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
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

const QUEUES: &[&str] = &["tool", "general", "python_tool", "model"];

fn ungated_ir(version: &str) -> Value {
    json!({
        "workflow_id": "wf-tenant-gate",
        "version": version,
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
                "policy": null
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

fn tenant_gate() -> Value {
    json!({ "blocked_tools": [], "require_approval_for": ["payments.*"], "model_allowlist": [] })
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
    method: Method,
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

fn parse_eid(s: &str) -> ExecutionId {
    let hex = s.strip_prefix("exec_").unwrap_or(s);
    ExecutionId(uuid::Uuid::parse_str(hex).unwrap())
}

async fn start(env: &Env) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        Method::POST,
        "/executions",
        &env.alice,
        Some(json!({
            "workflow_id": "wf-tenant-gate",
            "workflow_version": "0.1.0",
            "input": { "amount": 10 },
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    parse_eid(body["execution_id"].as_str().unwrap())
}

async fn claim(env: &Env) -> Value {
    let (st, body) = call(
        &env.app,
        Method::POST,
        "/work-items/claim",
        &env.alice,
        Some(json!({ "worker_id": "w", "queue_types": QUEUES })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

async fn gate_completions(env: &Env, eid: &ExecutionId) -> usize {
    env.backend
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter(
            |e| matches!(&e.kind, EventKind::NodeCompleted { node_id, .. } if node_id == "gate"),
        )
        .count()
}

/// The reviewer's demonstration, end to end: an operator gates the tenant, a
/// developer tries to remove the gate and then drives a run through it.
#[tokio::test]
async fn a_developer_cannot_drop_the_tenant_gate() {
    let env = env().await;
    let (st, body) = call(
        &env.app,
        Method::PUT,
        "/tenants/default",
        &env.ops,
        Some(json!({ "policy": tenant_gate() })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "operator sets the gate: {body}");
    let (st, body) = call(
        &env.app,
        Method::POST,
        "/workflows",
        &env.alice,
        Some(json!({ "ir": ungated_ir("0.1.0") })),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");

    // Control: the tenant gate holds the first run.
    let first = start(&env).await;
    assert_eq!(claim(&env).await, json!({ "claimed": false }));
    assert_eq!(gate_completions(&env, &first).await, 0);

    // Alice tries to drop the tenant policy.
    let (st, body) = call(
        &env.app,
        Method::PUT,
        "/tenants/default",
        &env.alice,
        Some(json!({ "policy": { "blocked_tools": [], "require_approval_for": [], "model_allowlist": [] } })),
    )
    .await;

    // Whatever the PUT answered, a new run must still be gated.
    let second = start(&env).await;
    let claimed = claim(&env).await;
    if claimed["claimed"] == true {
        let item = claimed["work_item"]["id"].as_str().unwrap();
        let _ = call(
            &env.app,
            Method::POST,
            &format!("/work-items/{item}/complete"),
            &env.alice,
            Some(json!({
                "output": { "ok": true },
                "state_patch": { "amount": 1_000_000 },
                "lease_fence": claimed["work_item"]["lease_fence"],
            })),
        )
        .await;
    }
    assert_eq!(
        gate_completions(&env, &second).await,
        0,
        "the gate completed after a developer PUT the tenant (answered {st}: {body})"
    );
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let tenant = env
        .backend
        .get_tenant(&TenantId::from("default"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tenant.policy,
        Some(tenant_gate()),
        "the policy is unchanged"
    );
}

/// Every tenant write needs the operator role.
#[tokio::test]
async fn tenant_writes_are_operator_only() {
    let env = env().await;
    let (st, body) = call(
        &env.app,
        Method::POST,
        "/tenants",
        &env.alice,
        Some(json!({ "id": "acme", "name": "Acme" })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "developer create: {body}");
    assert!(env
        .backend
        .get_tenant(&TenantId::from("acme"))
        .await
        .unwrap()
        .is_none());

    let (st, body) = call(
        &env.app,
        Method::POST,
        "/tenants",
        &env.ops,
        Some(json!({ "id": "acme", "name": "Acme" })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "operator create: {body}");

    let (st, body) = call(
        &env.app,
        Method::PUT,
        "/tenants/default",
        &env.ops,
        Some(json!({ "name": "renamed" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "operator update of own tenant: {body}");
}

/// An operator may not rewrite another tenant's policy: the target must be the
/// tenant the operator's token belongs to.
#[tokio::test]
async fn an_operator_cannot_update_another_tenant() {
    let env = env().await;
    let now = chrono::Utc::now();
    env.backend
        .create_tenant(Tenant {
            id: TenantId::from("acme"),
            name: "Acme".into(),
            status: TenantStatus::Active,
            policy: Some(tenant_gate()),
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();

    let (st, body) = call(
        &env.app,
        Method::PUT,
        "/tenants/acme",
        &env.ops,
        Some(json!({ "policy": { "blocked_tools": [], "require_approval_for": [], "model_allowlist": [] } })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let acme = env
        .backend
        .get_tenant(&TenantId::from("acme"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(acme.policy, Some(tenant_gate()));

    // Reads are scoped the same way: another tenant's policy is not shown.
    let (st, body) = call(&env.app, Method::GET, "/tenants/acme", &env.ops, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    let (st, body) = call(&env.app, Method::GET, "/tenants", &env.ops, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["default"], "{body}");
}

/// A developer cannot read the tenant routes either: they say who is gated on
/// what, and the comment on them always said operator-only.
#[tokio::test]
async fn tenant_reads_are_operator_only() {
    let env = env().await;
    for uri in ["/tenants", "/tenants/default"] {
        let (st, body) = call(&env.app, Method::GET, uri, &env.alice, None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
}
