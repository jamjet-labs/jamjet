//! `POST /work-items/claim` with `execution_id` (and `node_id`) claims only
//! that execution's (node's) item. A worker that owns an app queue and drives
//! one execution at a time (the Spring integration's recorded calls) used to
//! get the oldest item on the queue, so concurrent calls claimed each other's
//! nodes. Without the fields the claim is unchanged: oldest first.
mod support;

use jamjet_ir::workflow::WorkflowIr;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::{InMemoryBackend, SqliteBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use support::{make_state, post};

const QUEUE: &str = "app-calls";

fn ir() -> Value {
    let node = |id: &str| {
        json!({
            "id": id,
            "kind": { "type": "external_fn", "queue": QUEUE, "handler": id, "output_schema": "" },
            "retry_policy": null, "node_timeout_secs": null, "description": null, "labels": {}
        })
    };
    json!({
        "workflow_id": "wf-calls",
        "version": "0.1.0",
        "name": null, "description": null, "state_schema": "",
        "start_node": "first",
        "nodes": { "first": node("first"), "second": node("second") },
        "edges": [
            { "from": "first", "to": "second", "condition": null },
            { "from": "second", "to": "end", "condition": null }
        ],
        "retry_policies": {}, "timeouts": {}, "models": {}, "tools": {},
        "mcp_servers": {}, "remote_agents": {}, "labels": {}
    })
}

async fn start(state: &jamjet_api::state::AppState) -> String {
    let (st, body) = post(
        state,
        "/executions",
        json!({ "workflow_id": "wf-calls", "workflow_version": "0.1.0", "input": {} }),
    )
    .await;
    assert!(st.is_success(), "{st} {body}");
    body["execution_id"].as_str().unwrap().to_string()
}

async fn claim(state: &jamjet_api::state::AppState, extra: Value) -> (u16, Value) {
    let mut req = json!({ "worker_id": "spring-worker", "queue_types": [QUEUE] });
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    let (st, body) = post(state, "/work-items/claim", req).await;
    (st.as_u16(), body)
}

/// Enqueue `second` for `execution_id`, as the scheduler would once `first`
/// completed, so one execution holds two items on the queue.
async fn enqueue_second(backend: &Arc<dyn StateBackend>, execution_id: &str) {
    let ir: WorkflowIr = serde_json::from_value(ir()).unwrap();
    let eid = jamjet_core::workflow::ExecutionId(
        uuid::Uuid::parse_str(execution_id.trim_start_matches("exec_")).unwrap(),
    );
    let node = ir.node("second").unwrap();
    let payload = jamjet_scheduler::payload::build_work_item_payload(
        &ir,
        "second",
        node,
        &serde_json::Map::new(),
        "wf-calls",
        "0.1.0",
    );
    backend
        .enqueue_work_item(WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid,
            node_id: "second".into(),
            queue_type: QUEUE.into(),
            payload,
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            tenant_id: "default".into(),
            lease_fence: 0,
        })
        .await
        .unwrap();
}

async fn check(backend: Arc<dyn StateBackend>) {
    let state = make_state(backend.clone());
    let (st, body) = post(&state, "/workflows", json!({ "ir": ir() })).await;
    assert!(st.is_success(), "{st} {body}");
    let a = start(&state).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let b = start(&state).await;
    enqueue_second(&backend, &a).await;

    // The later execution's item, though A's is older.
    let (st, body) = claim(&state, json!({ "execution_id": b })).await;
    assert_eq!(st, 200);
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["execution_id"], json!(b));

    // One node of an execution that has two items on the queue.
    let (_, body) = claim(&state, json!({ "execution_id": a, "node_id": "second" })).await;
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["node_id"], "second");

    // An execution with nothing on the queue gets nothing.
    let foreign = format!("exec_{}", uuid::Uuid::new_v4().simple());
    let (st, body) = claim(&state, json!({ "execution_id": foreign })).await;
    assert_eq!(st, 200);
    assert_eq!(body, json!({ "claimed": false }));

    // No filter: the oldest item, as before (A's `first`).
    let (_, body) = claim(&state, json!({})).await;
    assert_eq!(body["claimed"], true, "{body}");
    assert_eq!(body["work_item"]["execution_id"], json!(a));
    assert_eq!(body["work_item"]["node_id"], "first");

    // A node filter needs an execution, and an execution id must parse.
    let (st, _) = claim(&state, json!({ "node_id": "first" })).await;
    assert_eq!(st, 400);
    let (st, _) = claim(&state, json!({ "execution_id": "not-an-id" })).await;
    assert_eq!(st, 400);
}

#[tokio::test]
async fn claim_by_execution_on_memory() {
    check(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn claim_by_execution_on_sqlite() {
    let path = std::env::temp_dir().join(format!("jjclaim-{}.db", uuid::Uuid::new_v4()));
    let backend = SqliteBackend::open(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    check(Arc::new(backend)).await;
    let _ = std::fs::remove_file(&path);
}

/// The tenant-scoped backend (what a token's requests use) applies the
/// filter too, inside its tenant: naming another tenant's execution claims
/// nothing, while that tenant can claim it.
#[tokio::test]
async fn claim_by_execution_on_the_tenant_scoped_backend() {
    use jamjet_state::{ClaimFilter, Tenant, TenantId, TenantStatus};
    let path = std::env::temp_dir().join(format!("jjclaim-{}.db", uuid::Uuid::new_v4()));
    let base = SqliteBackend::open(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let scoped: Arc<dyn StateBackend> = Arc::new(base.for_tenant(TenantId::from("default")));
    check(scoped.clone()).await;

    let acme = base.for_tenant(TenantId::from("acme"));
    let now = chrono::Utc::now();
    acme.create_tenant(Tenant {
        id: "acme".into(),
        name: "acme".into(),
        status: TenantStatus::Active,
        policy: None,
        limits: None,
        created_at: now,
        updated_at: now,
    })
    .await
    .unwrap();
    let eid = jamjet_core::workflow::ExecutionId::new();
    acme.create_execution(jamjet_core::workflow::WorkflowExecution {
        execution_id: eid.clone(),
        workflow_id: "wf-calls".into(),
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
    acme.enqueue_work_item(WorkItem {
        id: uuid::Uuid::new_v4(),
        execution_id: eid.clone(),
        node_id: "first".into(),
        queue_type: QUEUE.into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: now,
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "acme".into(),
        lease_fence: 0,
    })
    .await
    .unwrap();
    let filter = ClaimFilter {
        execution_id: Some(eid),
        node_id: None,
    };
    assert!(scoped
        .claim_work_item_filtered("w", &[QUEUE], None, &filter)
        .await
        .unwrap()
        .is_none());
    assert!(acme
        .claim_work_item_filtered("w", &[QUEUE], None, &filter)
        .await
        .unwrap()
        .is_some());
    let _ = std::fs::remove_file(&path);
}
