//! `GET /workers` reports the workers that hold a live lease. It returned a
//! hard-coded empty list.

mod support;

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::{InMemoryBackend, SqliteBackend};
use serde_json::json;
use std::sync::Arc;
use support::{get, make_state};

async fn seed(backend: &dyn StateBackend) -> ExecutionId {
    let now = chrono::Utc::now();
    let id = ExecutionId::new();
    backend
        .create_execution(WorkflowExecution {
            execution_id: id.clone(),
            workflow_id: "wf".into(),
            workflow_version: "1".into(),
            status: WorkflowStatus::Running,
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
    for node in ["a", "b", "c"] {
        backend
            .enqueue_work_item(WorkItem {
                id: uuid::Uuid::new_v4(),
                execution_id: id.clone(),
                node_id: node.into(),
                queue_type: "general".into(),
                payload: json!({}),
                attempt: 0,
                max_attempts: 3,
                created_at: now,
                lease_expires_at: None,
                worker_id: None,
                tenant_id: "default".into(),
                lease_fence: 0,
            })
            .await
            .unwrap();
    }
    // Two items to w1, one to w2.
    for worker in ["w1", "w1", "w2"] {
        backend
            .claim_work_item(worker, &["general"])
            .await
            .unwrap()
            .expect("claimable");
    }
    id
}

async fn check(backend: Arc<dyn StateBackend>) {
    let id = seed(backend.as_ref()).await;
    let state = make_state(backend);
    let (status, body) = get(&state, "/workers").await;
    assert_eq!(status, 200);
    let workers = body["workers"].as_array().unwrap();
    let summary: Vec<(String, usize)> = workers
        .iter()
        .map(|w| {
            (
                w["worker_id"].as_str().unwrap().to_string(),
                w["items"].as_array().unwrap().len(),
            )
        })
        .collect();
    assert_eq!(summary, vec![("w1".into(), 2), ("w2".into(), 1)], "{body}");
    assert_eq!(workers[0]["items"][0]["execution_id"], json!(id));
    assert!(workers[0]["items"][0]["lease_expires_at"].is_string());
}

#[tokio::test]
async fn get_workers_lists_lease_holders_on_memory() {
    check(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn get_workers_lists_lease_holders_on_sqlite() {
    check(Arc::new(
        SqliteBackend::open("sqlite::memory:").await.unwrap(),
    ))
    .await;
}
