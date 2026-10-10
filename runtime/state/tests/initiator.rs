//! A continue-as-new segment inherits its run's initiator.
//!
//! Separation of duties keys on `WorkflowStarted.initiated_by`. A rollover
//! appends a fresh `WorkflowStarted` on the new segment; if it dropped the
//! initiator, every approval after the first rollover would escape the check.

use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    start_next_segment, Event, EventKind, InMemoryBackend, MaterializedState, SqliteBackend,
    StateBackend,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};

fn exec(id: &ExecutionId) -> WorkflowExecution {
    let now = chrono::Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "seg-wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({}),
        current_state: json!({}),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn carried() -> MaterializedState {
    MaterializedState {
        current_state: json!({ "counter": 1 }),
        status: WorkflowStatus::Running,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 2,
    }
}

/// Parsed from JSON so the test pins the wire field name.
fn started(initiated_by: Option<&str>) -> EventKind {
    let mut v = json!({
        "type": "workflow_started",
        "workflow_id": "seg-wf",
        "workflow_version": "1.0.0",
        "initial_input": {},
    });
    if let Some(p) = initiated_by {
        v["initiated_by"] = json!(p);
    }
    serde_json::from_value(v).unwrap()
}

async fn child_initiator(
    backend: &dyn StateBackend,
    parent_initiator: Option<&str>,
) -> serde_json::Value {
    let parent = ExecutionId::new();
    backend.create_execution(exec(&parent)).await.unwrap();
    backend
        .append_event(Event::new(parent.clone(), 1, started(parent_initiator)))
        .await
        .unwrap();

    let child = start_next_segment(
        backend,
        &parent,
        &carried(),
        "seg-wf",
        "1.0.0",
        1,
        "start",
        "general",
        "default",
    )
    .await
    .unwrap();

    let events = backend.get_events(&child).await.unwrap();
    let e = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::WorkflowStarted { .. }))
        .expect("child WorkflowStarted");
    serde_json::to_value(&e.kind).unwrap()["initiated_by"].clone()
}

#[tokio::test]
async fn memory_segment_inherits_the_initiator() {
    let b = InMemoryBackend::new();
    assert_eq!(
        child_initiator(&b, Some("token:alice")).await,
        json!("token:alice")
    );
}

#[tokio::test]
async fn sqlite_segment_inherits_the_initiator() {
    let b = SqliteBackend::open("sqlite::memory:").await.unwrap();
    assert_eq!(
        child_initiator(&b, Some("token:alice")).await,
        json!("token:alice")
    );
}

#[tokio::test]
async fn a_segment_of_a_dev_mode_run_has_no_initiator() {
    let b = InMemoryBackend::new();
    assert_eq!(child_initiator(&b, None).await, serde_json::Value::Null);
}

#[test]
fn an_old_workflow_started_deserializes_without_an_initiator() {
    let kind = started(None);
    assert!(serde_json::to_value(&kind)
        .unwrap()
        .get("initiated_by")
        .is_none());
}
