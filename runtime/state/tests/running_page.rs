//! `list_running_executions_after`: the keyset page the scheduler walks to
//! reach every running run. Checked on each backend that implements it, and
//! on the trait default.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::tenant::TenantId;
use jamjet_state::{InMemoryBackend, SqliteBackend};

fn execution(status: WorkflowStatus) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: ExecutionId::new(),
        workflow_id: "wf".into(),
        workflow_version: "1".into(),
        status,
        initial_input: serde_json::json!({}),
        current_state: serde_json::json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

/// 7 running and 2 completed runs; walk pages of 3 and collect the ids.
async fn walk(backend: &dyn StateBackend) {
    let mut running: Vec<String> = Vec::new();
    for _ in 0..7 {
        let e = execution(WorkflowStatus::Running);
        running.push(e.execution_id.0.to_string());
        backend.create_execution(e).await.unwrap();
    }
    for _ in 0..2 {
        backend
            .create_execution(execution(WorkflowStatus::Completed))
            .await
            .unwrap();
    }
    running.sort();

    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<ExecutionId> = None;
    let mut pages = 0;
    loop {
        let page = backend
            .list_running_executions_after(after.as_ref(), 3)
            .await
            .unwrap();
        pages += 1;
        assert!(page.len() <= 3);
        assert!(page.iter().all(|e| e.status == WorkflowStatus::Running));
        seen.extend(page.iter().map(|e| e.execution_id.0.to_string()));
        if page.len() < 3 {
            break;
        }
        after = page.last().map(|e| e.execution_id.clone());
    }
    assert_eq!(seen, running, "every running run once, in id order");
    assert_eq!(pages, 3);
}

#[tokio::test]
async fn sqlite_pages_every_running_run() {
    let b = SqliteBackend::open("sqlite::memory:").await.unwrap();
    walk(&b).await;
}

#[tokio::test]
async fn tenant_scoped_sqlite_pages_every_running_run() {
    let b = SqliteBackend::open("sqlite::memory:").await.unwrap();
    walk(&b.for_tenant(TenantId::from("default"))).await;
}

#[tokio::test]
async fn memory_pages_every_running_run() {
    walk(&InMemoryBackend::new()).await;
}
