//! The engine is single-tenant, so tokens of any other tenant are refused.
//!
//! Tokens now carry the tenant on their row (26cbbbc), but the scheduler, the
//! in-process workers, workflow lookup and work-item creation all run in the
//! default tenant. A token in another tenant could register a workflow version
//! that the default tenant's runs were then scheduled from, and its own runs
//! lost their tenant (and tenant policy) after the start node (round-4 review,
//! C1). Until the engine carries the tenant, such a token gets 403. These
//! tests stay as regression tests for when tenancy is built.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_api::routes::build_router_with_opts;
use jamjet_core::workflow::ExecutionId;
use jamjet_scheduler::Scheduler;
use jamjet_state::backend::StateBackend;
use jamjet_state::event::EventKind;
use jamjet_state::{SqliteBackend, Tenant, TenantId, TenantStatus};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

mod support;

fn node(id: &str, module: &str, function: &str, policy: Value) -> Value {
    json!({
        "id": id,
        "kind": { "type": "python_fn", "module": module, "function": function, "output_schema": "" },
        "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {},
        "policy": policy
    })
}

fn two_node_ir(wf: &str, second: &str, module: &str, function: &str, policy: Value) -> Value {
    json!({
        "workflow_id": wf, "version": "0.1.0", "name": null, "description": null,
        "state_schema": "", "start_node": "a",
        "nodes": {
            "a": node("a", "myapp.steps", "prep", Value::Null),
            second: node(second, module, function, policy),
        },
        "edges": [
            { "from": "a", "to": second, "condition": null },
            { "from": second, "to": "end", "condition": null }
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

struct Env {
    sqlite: Arc<SqliteBackend>,
    app: axum::Router,
    alice: String,
    mallory: String,
    sched: tokio::task::JoinHandle<()>,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.sched.abort();
    }
}

/// SQLite wired as `main.rs` wires it, with Mallory's token in tenant `acme`.
async fn env(acme_policy: Option<Value>) -> Env {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "jj_tenant_tok_{}.db",
        uuid::Uuid::new_v4().simple()
    ));
    let sqlite = Arc::new(
        SqliteBackend::open(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
    );
    let now = chrono::Utc::now();
    sqlite
        .for_tenant(TenantId::default())
        .create_tenant(Tenant {
            id: TenantId::from("acme"),
            name: "Acme".into(),
            status: TenantStatus::Active,
            policy: acme_policy,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let mut state = support::make_state(sqlite.clone() as Arc<dyn StateBackend>);
    let scoped = sqlite.clone();
    state.backend_for_fn = Arc::new(move |t: &TenantId| {
        Arc::new(scoped.for_tenant(t.clone())) as Arc<dyn StateBackend>
    });
    let app = build_router_with_opts(state, false);
    let (alice, _) = sqlite.create_token("alice", "developer").await.unwrap();
    let (mallory, _) = sqlite.create_token("mallory", "developer").await.unwrap();
    sqlx::query("UPDATE api_tokens SET tenant_id = 'acme' WHERE name = 'mallory'")
        .execute(&sqlite.pool())
        .await
        .unwrap();
    let sb = sqlite.clone() as Arc<dyn StateBackend>;
    let sched = tokio::spawn(async move {
        Scheduler::new(sb)
            .with_poll_interval(Duration::from_millis(10))
            .run()
            .await
    });
    Env {
        sqlite,
        app,
        alice,
        mallory,
        sched,
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

async fn register(env: &Env, token: &str, ir: Value) -> (StatusCode, Value) {
    call(
        &env.app,
        "POST",
        "/workflows",
        token,
        Some(json!({ "ir": ir })),
    )
    .await
}

async fn start(env: &Env, token: &str, wf: &str) -> ExecutionId {
    let (st, body) = call(
        &env.app,
        "POST",
        "/executions",
        token,
        Some(
            json!({ "workflow_id": wf, "workflow_version": "0.1.0", "input": { "to": "vendor" } }),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["execution_id"].as_str().unwrap().to_string();
    ExecutionId(uuid::Uuid::parse_str(id.strip_prefix("exec_").unwrap()).unwrap())
}

async fn claim(env: &Env, token: &str) -> Value {
    let (st, body) = call(
        &env.app,
        "POST",
        "/work-items/claim",
        token,
        Some(json!({ "worker_id": "w", "queue_types": ["python_tool"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

/// Run the start node `a` and wait for the scheduler to dispatch what follows.
async fn run_start_node(env: &Env, token: &str, eid: &ExecutionId) -> Vec<String> {
    let mut a = Value::Null;
    for _ in 0..1000 {
        a = claim(env, token).await;
        if a["claimed"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(a["work_item"]["node_id"], "a", "{a}");
    let (st, body) = call(
        &env.app,
        "POST",
        &format!("/work-items/{}/complete", a["work_item"]["id"].as_str().unwrap()),
        token,
        Some(json!({ "output": {}, "state_patch": {}, "lease_fence": a["work_item"]["lease_fence"] })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    for _ in 0..1000 {
        // Wait for a node after `a`: the start node can be scheduled twice
        // (a scheduler tick can land between `start_run`'s `WorkflowStarted`
        // and its own `NodeScheduled`), which is a separate, known race.
        let s = scheduled(env, eid).await;
        if s.iter().any(|n| n != "a") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    scheduled(env, eid).await
}

async fn scheduled(env: &Env, eid: &ExecutionId) -> Vec<String> {
    env.sqlite
        .get_events(eid)
        .await
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::NodeScheduled { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect()
}

fn refused_as_single_tenant(st: StatusCode, body: &Value) -> bool {
    st == StatusCode::FORBIDDEN
        && body
            .to_string()
            .contains("multi-tenant tokens are not supported")
}

/// Demo (a): a version registered first by another tenant was the one the
/// default tenant's runs were scheduled from.
#[tokio::test]
async fn another_tenant_cannot_substitute_a_workflow_version() {
    let env = env(None).await;
    let (m_st, m_body) = register(
        &env,
        &env.mallory,
        two_node_ir("payroll", "steal", "evil.mod", "steal", Value::Null),
    )
    .await;
    let (st, body) = register(
        &env,
        &env.alice,
        two_node_ir(
            "payroll",
            "pay",
            "payments",
            "send",
            json!({ "blocked_tools": [], "require_approval_for": ["*"], "model_allowlist": [] }),
        ),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    let eid = start(&env, &env.alice, "payroll").await;
    let s = run_start_node(&env, &env.alice, &eid).await;
    assert!(
        s.contains(&"pay".to_string()),
        "alice's run was scheduled from another tenant's graph: {s:?}"
    );
    assert!(refused_as_single_tenant(m_st, &m_body), "{m_st} {m_body}");
}

/// Demo (b): a run started in another tenant lost its tenant after the start
/// node, so that tenant's policy never applied to the rest of the run.
#[tokio::test]
async fn another_tenants_run_cannot_lose_its_tenant() {
    let env = env(Some(
        json!({ "blocked_tools": [], "require_approval_for": ["myapp.payments:*"], "model_allowlist": [] }),
    ))
    .await;
    let (st, body) = register(
        &env,
        &env.mallory,
        two_node_ir("wf2", "pay", "myapp.payments", "transfer", Value::Null),
    )
    .await;
    if st.is_success() {
        let eid = start(&env, &env.mallory, "wf2").await;
        run_start_node(&env, &env.mallory, &eid).await;
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT node_id, tenant_id FROM work_items WHERE execution_id LIKE ?")
                .bind(format!("%{}%", eid.0))
                .fetch_all(&env.sqlite.pool())
                .await
                .unwrap();
        assert!(
            rows.iter().all(|(_, t)| t == "acme"),
            "a later item left the run's tenant: {rows:?}"
        );
    }
    assert!(refused_as_single_tenant(st, &body), "{st} {body}");
}

/// Every route, reads included, refuses such a token, and so does the MCP bridge.
#[tokio::test]
async fn every_entry_point_refuses_a_non_default_tenant_token() {
    let env = env(None).await;
    let (st, body) = call(&env.app, "GET", "/executions", &env.mallory, None).await;
    assert!(refused_as_single_tenant(st, &body), "{st} {body}");
    let (st, body) = call(
        &env.app,
        "POST",
        "/mcp",
        &env.mallory,
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })),
    )
    .await;
    assert!(refused_as_single_tenant(st, &body), "{st} {body}");
    // A default-tenant token is unaffected.
    let (st, body) = call(&env.app, "GET", "/executions", &env.alice, None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
}
