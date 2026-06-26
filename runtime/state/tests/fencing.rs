//! Lease-fencing tests: a zombie worker cannot double-commit; the fence is
//! monotonic across reclaim and survives a simulated store failover.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, StateBackendError, WorkItem},
    event::{Event, EventKind},
    SqliteBackend,
};
use serde_json::json;
use std::path::PathBuf;
use uuid::Uuid;

fn temp_db_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    let unique = Uuid::new_v4().to_string().replace('-', "");
    path.push(format!("jamjet_fence_{unique}.db"));
    path
}

async fn open_db(path: &PathBuf) -> SqliteBackend {
    let url = format!("sqlite://{}", path.display());
    SqliteBackend::open(&url).await.expect("open test db")
}

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-fence".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
    }
}

fn sample_item(execution_id: &ExecutionId) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
        node_id: "n1".into(),
        queue_type: "model".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "default".into(),
        lease_fence: 0,
    }
}

#[tokio::test]
async fn claim_mints_nonzero_fence() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let eid = ExecutionId::new();
    db.create_execution(sample_execution(&eid)).await.unwrap();
    db.enqueue_work_item(sample_item(&eid)).await.unwrap();

    let claimed = db.claim_work_item("worker-A", &["model"]).await.unwrap().unwrap();
    assert!(claimed.lease_fence > 0, "claim must mint a non-zero fence");

    std::fs::remove_file(&path).ok();
}
