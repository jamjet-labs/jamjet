//! A registered workflow version is immutable, on every backend.
//!
//! `store_workflow` used to be `INSERT OR REPLACE`, so re-registering a
//! version silently swapped its IR: the policy gate could be dropped from a
//! running workflow, or a node's body changed under an approval that still
//! matched (approval-integrity review, C2). A byte-identical re-store is still
//! a no-op (deployments re-register on every boot); a differing one is refused
//! with `VersionConflict` and the stored IR is left as it was.

use chrono::Utc;
use jamjet_state::backend::{StateBackend, StateBackendError, WorkflowDefinition};
use jamjet_state::tenant::TenantId;
use jamjet_state::{InMemoryBackend, SqliteBackend};
use serde_json::{json, Value};

fn def(ir: Value) -> WorkflowDefinition {
    WorkflowDefinition {
        workflow_id: "wf".into(),
        version: "1.0.0".into(),
        ir,
        created_at: Utc::now(),
        tenant_id: "default".into(),
    }
}

fn gated() -> Value {
    json!({ "workflow_id": "wf", "nodes": { "n": { "policy": { "require_approval_for": ["pay.*"] } } } })
}

fn ungated() -> Value {
    json!({ "workflow_id": "wf", "nodes": { "n": {} } })
}

async fn check(backend: &dyn StateBackend, name: &str) {
    backend.store_workflow(def(gated())).await.unwrap();

    // Same content, keys in a different order: canonical JSON is equal.
    let reordered: Value = serde_json::from_str(
        r#"{ "nodes": { "n": { "policy": { "require_approval_for": ["pay.*"] } } }, "workflow_id": "wf" }"#,
    )
    .unwrap();
    backend
        .store_workflow(def(reordered))
        .await
        .unwrap_or_else(|e| panic!("{name}: an identical re-store must be a no-op: {e}"));

    match backend.store_workflow(def(ungated())).await {
        Err(StateBackendError::VersionConflict(_)) => {}
        other => panic!("{name}: a differing re-store must be refused, got {other:?}"),
    }
    let stored = backend.get_workflow("wf", "1.0.0").await.unwrap().unwrap();
    assert_eq!(
        stored.ir,
        gated(),
        "{name}: the stored IR must be unchanged"
    );
}

#[tokio::test]
async fn the_memory_backend_refuses_to_replace_a_version() {
    check(&InMemoryBackend::new(), "memory").await;
}

#[tokio::test]
async fn the_sqlite_backend_refuses_to_replace_a_version() {
    let db = SqliteBackend::open("sqlite::memory:").await.unwrap();
    check(&db, "sqlite").await;
}

#[tokio::test]
async fn the_tenant_scoped_backend_refuses_to_replace_a_version() {
    let db = SqliteBackend::open("sqlite::memory:").await.unwrap();
    let scoped = db.for_tenant(TenantId::default());
    check(&scoped, "tenant_scoped").await;
}
