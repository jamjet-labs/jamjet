//! Integration tests for InMemoryBackend.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, WorkflowDefinition},
    event::{Event, EventKind},
    snapshot::Snapshot,
    InMemoryBackend, WorkItem,
};
use serde_json::json;
use uuid::Uuid;

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-test".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "x": 1 }),
        current_state: json!({ "x": 1 }),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
    }
}

#[tokio::test]
async fn test_workflow_crud() {
    let backend = InMemoryBackend::new();
    let def = WorkflowDefinition {
        workflow_id: "wf-1".into(),
        version: "1.0.0".into(),
        ir: json!({"nodes": []}),
        created_at: Utc::now(),
        tenant_id: "default".into(),
    };
    backend.store_workflow(def).await.unwrap();
    let loaded = backend.get_workflow("wf-1", "1.0.0").await.unwrap();
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().workflow_id, "wf-1");
    assert!(backend
        .get_workflow("wf-1", "2.0.0")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_execution_lifecycle() {
    let backend = InMemoryBackend::new();
    let id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&id))
        .await
        .unwrap();
    let exec = backend.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Running);
    backend
        .update_execution_status(&id, WorkflowStatus::Completed)
        .await
        .unwrap();
    let exec = backend.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Completed);
}

#[tokio::test]
async fn test_list_executions_with_filter() {
    let backend = InMemoryBackend::new();
    for _ in 0..3 {
        backend
            .create_execution(sample_execution(&ExecutionId::new()))
            .await
            .unwrap();
    }
    let all = backend.list_executions(None, 100, 0).await.unwrap();
    assert_eq!(all.len(), 3);
    let running = backend
        .list_executions(Some(WorkflowStatus::Running), 100, 0)
        .await
        .unwrap();
    assert_eq!(running.len(), 3);
    let completed = backend
        .list_executions(Some(WorkflowStatus::Completed), 100, 0)
        .await
        .unwrap();
    assert_eq!(completed.len(), 0);
    let page = backend.list_executions(None, 2, 0).await.unwrap();
    assert_eq!(page.len(), 2);
    let page2 = backend.list_executions(None, 2, 2).await.unwrap();
    assert_eq!(page2.len(), 1);
}

#[tokio::test]
async fn test_event_log() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();

    let e1 = Event::new(
        exec_id.clone(),
        0,
        EventKind::WorkflowStarted {
            workflow_id: "wf-test".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({ "x": 1 }),
        },
    );
    let seq1 = backend.append_event(e1).await.unwrap();
    assert!(seq1 > 0);

    let e2 = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeScheduled {
            node_id: "n1".into(),
            queue_type: "default".into(),
        },
    );
    let seq2 = backend.append_event(e2).await.unwrap();
    assert!(seq2 > seq1);

    let events = backend.get_events(&exec_id).await.unwrap();
    assert_eq!(events.len(), 2);

    let since = backend.get_events_since(&exec_id, seq1).await.unwrap();
    assert_eq!(since.len(), 1);
    assert_eq!(since[0].sequence, seq2);

    let latest = backend.latest_sequence(&exec_id).await.unwrap();
    assert_eq!(latest, seq2);
}

#[tokio::test]
async fn test_snapshots() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    assert!(backend.latest_snapshot(&exec_id).await.unwrap().is_none());
    let snap = Snapshot::new(exec_id.clone(), 5, json!({"state": "checkpoint"}));
    backend.write_snapshot(snap).await.unwrap();
    let latest = backend.latest_snapshot(&exec_id).await.unwrap().unwrap();
    assert_eq!(latest.at_sequence, 5);
}

#[tokio::test]
async fn test_work_queue() {
    let backend = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    let item = WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
        node_id: "n1".into(),
        queue_type: "default".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "default".into(),
    };
    let item_id = backend.enqueue_work_item(item).await.unwrap();
    let claimed = backend
        .claim_work_item("w1", &["default"])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, item_id);
    assert_eq!(claimed.worker_id.as_deref(), Some("w1"));
    // Second claim should find nothing (item is already claimed)
    assert!(backend
        .claim_work_item("w2", &["default"])
        .await
        .unwrap()
        .is_none());
    backend.complete_work_item(item_id).await.unwrap();
}

#[tokio::test]
async fn test_patch_append_array() {
    let backend = InMemoryBackend::new();
    let id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&id))
        .await
        .unwrap();
    backend
        .patch_append_array(&id, "results", json!("first"))
        .await
        .unwrap();
    backend
        .patch_append_array(&id, "results", json!("second"))
        .await
        .unwrap();
    let exec = backend.get_execution(&id).await.unwrap().unwrap();
    let results = exec.current_state["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], json!("first"));
    assert_eq!(results[1], json!("second"));
}

#[tokio::test]
async fn test_api_tokens() {
    let backend = InMemoryBackend::new();
    let (plaintext, token) = backend.create_token("test-token", "admin").await.unwrap();
    assert!(plaintext.starts_with("jj_"));
    assert_eq!(token.name, "test-token");
    let validated = backend.validate_token(&plaintext).await.unwrap().unwrap();
    assert_eq!(validated.role, "admin");
    assert!(backend.validate_token("invalid").await.unwrap().is_none());
}
