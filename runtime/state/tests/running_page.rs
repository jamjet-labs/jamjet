//! `list_running_executions_after`: the keyset page the scheduler walks to
//! reach every running run. Checked on each backend that implements it (the
//! trait default, which the test doubles use, is not covered here).

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
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = backend
            .list_running_executions_after(after.as_deref(), 3)
            .await
            .unwrap();
        pages += 1;
        assert!(page.executions.len() <= 3);
        assert!(page
            .executions
            .iter()
            .all(|e| e.status == WorkflowStatus::Running));
        seen.extend(page.executions.iter().map(|e| e.execution_id.0.to_string()));
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, running, "every running run once, in id order");
    // 3 + 3 + 1, then one page that scans nothing.
    assert_eq!(pages, 4);
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

/// A running row that does not parse is skipped, and the rows after it are
/// still reached. The page used to fail as a whole, so one bad row stopped
/// the scheduler's walk at it on every tick.
#[tokio::test]
async fn an_unreadable_row_does_not_hide_the_rows_after_it() {
    let b = SqliteBackend::open("sqlite::memory:").await.unwrap();
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..3 {
        let e = execution(WorkflowStatus::Running);
        ids.push(e.execution_id.0.to_string());
        b.create_execution(e).await.unwrap();
    }
    ids.sort();
    sqlx::query("UPDATE workflow_executions SET started_at = 'not a time' WHERE execution_id = ?")
        .bind(&ids[1])
        .execute(&b.pool())
        .await
        .unwrap();

    let mut seen: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let page = b
            .list_running_executions_after(after.as_deref(), 1)
            .await
            .expect("an unreadable row is skipped, not an error");
        seen.extend(page.executions.iter().map(|e| e.execution_id.0.to_string()));
        // Walk the way the scheduler does: follow the cursor.
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, vec![ids[0].clone(), ids[2].clone()]);
}
