//! Shared helpers for the spike-regression route tests (2026-09-06).
#![allow(dead_code)]
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jamjet_agents::InMemoryAgentRegistry;
use jamjet_api::{routes::build_router_with_opts, state::AppState};
use jamjet_audit::{AuditEnricher, NoopAuditBackend};
use jamjet_state::backend::StateBackend;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

pub mod hooked;

pub fn make_state(backend: Arc<dyn StateBackend>) -> AppState {
    let backend_for_fn = backend.clone();
    let audit: Arc<dyn jamjet_audit::AuditBackend> = Arc::new(NoopAuditBackend);
    let enricher = Arc::new(AuditEnricher::new(Arc::clone(&audit)));
    AppState {
        backend: backend.clone(),
        backend_for_fn: Arc::new(move |_t: &jamjet_state::TenantId| backend_for_fn.clone()),
        agents: Arc::new(InMemoryAgentRegistry::new()),
        audit,
        enricher,
        protocols: jamjet_api::state::default_protocol_registry(),
        cron_store: None,
        approval_policy: Default::default(),
    }
}

pub async fn post(state: &AppState, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

pub async fn get(state: &AppState, path: &str) -> (StatusCode, Value) {
    let resp = build_router_with_opts(state.clone(), true)
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// A one-node workflow whose START node is a `python_fn`, exactly the shape
/// PAS uses for its first real step.
pub fn python_fn_start_ir(workflow_id: &str) -> Value {
    json!({
        "workflow_id": workflow_id,
        "version": "0.1.0",
        "name": null,
        "description": null,
        "state_schema": "",
        "start_node": "py_start",
        "nodes": {
            "py_start": {
                "id": "py_start",
                "kind": { "type": "python_fn", "module": "myapp.tools", "function": "run_check", "output_schema": "" },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {}
            }
        },
        "edges": [ { "from": "py_start", "to": "end", "condition": null } ],
        "retry_policies": {},
        "timeouts": {},
        "models": {},
        "tools": {},
        "mcp_servers": {},
        "remote_agents": {},
        "labels": {}
    })
}

/// Register `ir` and start one execution of it with `input`; returns the
/// `exec_…` id string the API hands out.
pub async fn register_and_start(state: &AppState, ir: Value, input: Value) -> String {
    let workflow_id = ir["workflow_id"].as_str().unwrap().to_string();
    let version = ir["version"].as_str().unwrap().to_string();
    let (st, body) = post(state, "/workflows", json!({ "ir": ir })).await;
    assert!(st.is_success(), "register failed: {st} {body}");
    let (st, body) = post(
        state,
        "/executions",
        json!({ "workflow_id": workflow_id, "workflow_version": version, "input": input }),
    )
    .await;
    assert!(st.is_success(), "start failed: {st} {body}");
    body["execution_id"]
        .as_str()
        .expect("execution_id in start response")
        .to_string()
}

/// A backend a route test runs against, with a way to read a work item's
/// settlement that does not depend on the backend's own read path.
///
/// The route tests used to run on `InMemoryBackend` only, and memory and
/// SQLite disagreed on what "settle" means (PR #140 review, finding I2). A
/// test that loops over [`backends`] cannot pass on one and fail on the other.
pub enum TestBackend {
    Memory(Arc<jamjet_state::InMemoryBackend>),
    Sqlite {
        backend: Arc<jamjet_state::SqliteBackend>,
        url: String,
    },
}

impl TestBackend {
    pub fn name(&self) -> &'static str {
        match self {
            TestBackend::Memory(_) => "memory",
            TestBackend::Sqlite { .. } => "sqlite",
        }
    }

    pub fn dyn_backend(&self) -> Arc<dyn StateBackend> {
        match self {
            TestBackend::Memory(b) => b.clone(),
            TestBackend::Sqlite { backend, .. } => backend.clone(),
        }
    }

    /// Where the item stands: `"pending"` or `"claimed"` while it is live,
    /// then the settled status (`"completed"`, `"failed"`, ...). Memory drops
    /// a settled item, which reads as `"settled"`.
    pub async fn item_status(&self, id: uuid::Uuid) -> String {
        match self {
            TestBackend::Memory(b) => match b.get_work_item(id).await.unwrap() {
                Some(item) if item.worker_id.is_some() => "claimed".into(),
                Some(_) => "pending".into(),
                None => "settled".into(),
            },
            TestBackend::Sqlite { url, .. } => {
                let pool = sqlx::SqlitePool::connect(url).await.unwrap();
                let (status,): (String,) =
                    sqlx::query_as("SELECT status FROM work_items WHERE id = ?")
                        .bind(id.to_string())
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                pool.close().await;
                status
            }
        }
    }

    /// Whether the item has been settled, in whatever way the backend
    /// records that.
    pub async fn is_settled(&self, id: uuid::Uuid) -> bool {
        !matches!(self.item_status(id).await.as_str(), "pending" | "claimed")
    }

    /// Backdate the item's lease so the next claim poll finds it expired.
    pub async fn expire_lease(&self, id: uuid::Uuid) {
        match self {
            TestBackend::Memory(b) => b.force_lease_expired_for_test(id),
            TestBackend::Sqlite { backend, .. } => {
                backend.force_lease_expired_for_test(id).await.unwrap()
            }
        }
    }
}

/// A fresh in-memory backend and a fresh SQLite database on disk.
pub async fn backends() -> Vec<TestBackend> {
    let mut path = std::env::temp_dir();
    path.push(format!("jamjet_route_{}.db", uuid::Uuid::new_v4().simple()));
    let url = format!("sqlite://{}", path.display());
    let sqlite = jamjet_state::SqliteBackend::open(&url)
        .await
        .expect("open test db");
    vec![
        TestBackend::Memory(Arc::new(jamjet_state::InMemoryBackend::new())),
        TestBackend::Sqlite {
            backend: Arc::new(sqlite),
            url,
        },
    ]
}
