//! In-process workers apply tenant policy on SQLite, as the claim route does.
//!
//! The embedded worker pool runs on the base `SqliteBackend`, which had no
//! `get_tenant`: the trait default answers "no such tenant", so the tenant
//! layer of the policy chain was empty for every in-process node (`tool`,
//! `mcp_tool`, `a2a_task`, `model`, `agent`), while the HTTP claim route, on a
//! tenant-scoped backend, applied it (round-4 review, I3). The suites used the
//! memory backend, which does implement it, so they never saw the difference.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{SqliteBackend, TenantId};
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

/// The base backend reads the tenant record, policy included.
#[tokio::test]
async fn the_base_sqlite_backend_reads_tenants() {
    let sqlite = SqliteBackend::open("sqlite::memory:").await.unwrap();
    let scoped = sqlite.for_tenant(TenantId::default());
    let mut t = scoped
        .get_tenant(&TenantId::default())
        .await
        .unwrap()
        .unwrap();
    t.policy =
        Some(json!({ "blocked_tools": ["x"], "require_approval_for": [], "model_allowlist": [] }));
    scoped.update_tenant(t).await.unwrap();

    let base = sqlite.get_tenant(&TenantId::default()).await.unwrap();
    assert_eq!(
        base.and_then(|t| t.policy).unwrap()["blocked_tools"][0],
        "x"
    );
    assert!(sqlite
        .get_tenant(&TenantId::from("nope"))
        .await
        .unwrap()
        .is_none());
    assert!(sqlite
        .list_tenants()
        .await
        .unwrap()
        .iter()
        .any(|t| t.id.as_str() == "default"));
}

/// A tool node under a tenant gate, run by the embedded worker pool on
/// SQLite, is held for approval and does not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_process_node_under_a_tenant_gate_is_held_on_sqlite() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jj_sqlite_tenant_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let mut state = support::make_state(sqlite.clone() as Arc<dyn StateBackend>);
    let scoped = sqlite.clone();
    state.backend_for_fn = Arc::new(move |t: &TenantId| {
        Arc::new(scoped.for_tenant(t.clone())) as Arc<dyn StateBackend>
    });
    let app = build_router_with_opts(state, false);
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (ops, _) = sqlite.create_token("ops", "operator").await.unwrap();

    let (st, body) = call(
        &app,
        "PUT",
        "/tenants/default",
        &ops,
        json!({ "policy": { "blocked_tools": [], "require_approval_for": ["payments.*"], "model_allowlist": [] } }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let (st, body) = call(
        &app,
        "POST",
        "/workflows",
        &alice,
        json!({ "ir": tool_ir() }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let (st, body) = call(
        &app,
        "POST",
        "/executions",
        &alice,
        json!({ "workflow_id": "wf-tool", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap();
    let eid = ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap());

    // The engine as `main.rs` runs it: scheduler and embedded pool on the BASE backend.
    let base = sqlite.clone() as Arc<dyn StateBackend>;
    let sched_backend = base.clone();
    let sched = tokio::spawn(async move {
        Scheduler::new(sched_backend)
            .with_poll_interval(Duration::from_millis(10))
            .run()
            .await
    });
    let workers = jamjet_worker::default_pool(base).spawn();

    let mut events = Vec::new();
    for _ in 0..500 {
        events = sqlite.get_events(&eid).await.unwrap();
        if events.iter().any(|e| {
            matches!(
                e.kind,
                EventKind::ToolApprovalRequired { .. }
                    | EventKind::NodeFailed { .. }
                    | EventKind::NodeCompleted { .. }
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
    let kinds: Vec<_> = events.iter().map(|e| &e.kind).collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::ToolApprovalRequired { .. })),
        "the tenant gate did not hold the in-process node: {kinds:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, EventKind::NodeCompleted { .. })),
        "{kinds:#?}"
    );
}
