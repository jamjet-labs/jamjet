//! Round-trip test: a Snapshot carrying full materialized state
//! (status, completed_nodes, active_nodes, last_sequence) survives
//! write_snapshot → latest_snapshot intact.
//!
//! TDD: this test is written RED first (Snapshot has no such fields),
//! then turns GREEN after the struct + migration + backend widening.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{backend::StateBackend, snapshot::Snapshot, SqliteBackend};
use serde_json::json;
use std::collections::{HashMap, HashSet};

async fn open_test_db() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite")
}

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "test-wf".into(),
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

/// Full materialized state round-trips through write_snapshot → latest_snapshot.
///
/// Verifies: status, completed_nodes, active_nodes, last_sequence are all
/// persisted and reloaded faithfully by the SQLite backend.
#[tokio::test]
async fn test_snapshot_full_materialized_state_round_trips() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let mut completed_nodes = HashMap::new();
    completed_nodes.insert("n1".to_string(), json!(1));
    let mut active_nodes = HashSet::new();
    active_nodes.insert("n2".to_string());

    let snap = Snapshot {
        id: uuid::Uuid::new_v4(),
        execution_id: exec_id.clone(),
        at_sequence: 7,
        state: json!({"x": 42}),
        status: WorkflowStatus::Running,
        completed_nodes,
        active_nodes,
        last_sequence: 7,
        created_at: Utc::now(),
    };

    db.write_snapshot(snap).await.unwrap();

    let loaded = db.latest_snapshot(&exec_id).await.unwrap().unwrap();
    assert_eq!(loaded.at_sequence, 7);
    assert_eq!(loaded.status, WorkflowStatus::Running);
    assert_eq!(
        loaded.completed_nodes.get("n1"),
        Some(&json!(1)),
        "completed_nodes should round-trip"
    );
    assert!(
        loaded.active_nodes.contains("n2"),
        "active_nodes should round-trip"
    );
    assert_eq!(loaded.last_sequence, 7, "last_sequence should round-trip");
}

/// Snapshot::new (compat path) still compiles and returns sane defaults.
#[tokio::test]
async fn test_snapshot_new_compat_defaults() {
    let exec_id = ExecutionId::new();
    let snap = Snapshot::new(exec_id.clone(), 5, json!({"nodes_completed": ["a", "b"]}));
    assert_eq!(snap.at_sequence, 5);
    assert_eq!(snap.last_sequence, 5);
    assert_eq!(snap.status, WorkflowStatus::Pending);
    assert!(snap.completed_nodes.is_empty());
    assert!(snap.active_nodes.is_empty());
}
