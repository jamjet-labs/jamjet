//! Durability integration tests — verifies that workflow state survives a
//! simulated runtime crash and can be correctly recovered from the event log.
//!
//! Test F.4: "Workflow resumes correctly after runtime kill (SQLite)"

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, WorkflowDefinition},
    event::{Event, EventKind},
    materializer::{apply_events, materialize},
    snapshot::Snapshot,
    SqliteBackend,
};
use serde_json::json;
use std::path::PathBuf;

// ── helpers ──────────────────────────────────────────────────────────────────

fn temp_db_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    let unique = uuid::Uuid::new_v4().to_string().replace('-', "");
    path.push(format!("jamjet_test_{unique}.db"));
    path
}

async fn open_db(path: &PathBuf) -> SqliteBackend {
    let url = format!("sqlite://{}", path.display());
    SqliteBackend::open(&url)
        .await
        .expect("failed to open test db")
}

fn sample_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "wf-crash-test".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "x": 1 }),
        current_state: json!({ "x": 1 }),
        started_at: now,
        updated_at: now,
        completed_at: None,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// F.2 — State machine transitions are persisted and queryable.
#[tokio::test]
async fn test_state_machine_transitions() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let id = ExecutionId::new();
    db.create_execution(sample_execution(&id)).await.unwrap();

    // Pending → Running
    db.update_execution_status(&id, WorkflowStatus::Running)
        .await
        .unwrap();
    let exec = db.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Running);

    // Running → Paused
    db.update_execution_status(&id, WorkflowStatus::Paused)
        .await
        .unwrap();
    let exec = db.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Paused);

    // Paused → Running → Completed
    db.update_execution_status(&id, WorkflowStatus::Running)
        .await
        .unwrap();
    db.update_execution_status(&id, WorkflowStatus::Completed)
        .await
        .unwrap();
    let exec = db.get_execution(&id).await.unwrap().unwrap();
    assert_eq!(exec.status, WorkflowStatus::Completed);
    assert!(exec.completed_at.is_some());

    std::fs::remove_file(&path).ok();
}

/// F.4 — Crash recovery: write events to SQLite, drop the connection (simulating a crash),
/// reopen the database, reload from events, verify state is intact.
#[tokio::test]
async fn test_crash_recovery_from_event_log() {
    let path = temp_db_path();
    let id = ExecutionId::new();

    // ── Phase 1: write state to SQLite ──────────────────────────────────────
    {
        let db = open_db(&path).await;
        db.create_execution(sample_execution(&id)).await.unwrap();

        let events = vec![
            Event::new(
                id.clone(),
                1,
                EventKind::WorkflowStarted {
                    workflow_id: "wf-crash-test".into(),
                    workflow_version: "1.0.0".into(),
                    initial_input: json!({ "x": 1 }),
                },
            ),
            Event::new(
                id.clone(),
                2,
                EventKind::NodeScheduled {
                    node_id: "step1".into(),
                    queue_type: "tool".into(),
                },
            ),
            Event::new(
                id.clone(),
                3,
                EventKind::NodeStarted {
                    node_id: "step1".into(),
                    worker_id: "worker-1".into(),
                    attempt: 0,
                },
            ),
            Event::new(
                id.clone(),
                4,
                EventKind::NodeCompleted {
                    node_id: "step1".into(),
                    output: json!("hello"),
                    state_patch: json!({ "greeting": "hello" }),
                    duration_ms: 42,
                    gen_ai_system: None,
                    gen_ai_model: None,
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                    cost_usd: None,
                    provenance: None,
                },
            ),
            Event::new(
                id.clone(),
                5,
                EventKind::NodeScheduled {
                    node_id: "step2".into(),
                    queue_type: "tool".into(),
                },
            ),
            // ← Crash happens here, step2 never completes
        ];

        for event in events {
            db.append_event(event).await.unwrap();
        }

        // db is dropped here — simulates crash (connection closed)
    }

    // ── Phase 2: recover from SQLite ────────────────────────────────────────
    {
        let db = open_db(&path).await;

        // Execution record is still there.
        let exec = db.get_execution(&id).await.unwrap().unwrap();
        assert_eq!(exec.workflow_id, "wf-crash-test");

        // Event log is intact.
        let events = db.get_events(&id).await.unwrap();
        assert_eq!(events.len(), 5);
        assert_eq!(db.latest_sequence(&id).await.unwrap(), 5);

        // Materialize state from events.
        let mat = materialize(&db, &id).await.unwrap();

        // step1 completed, step2 scheduled but not completed.
        assert_eq!(mat.current_state["greeting"], "hello");
        assert!(mat.completed_nodes.contains_key("step1"));
        assert!(mat.active_nodes.contains("step2"));
        assert_eq!(mat.status, WorkflowStatus::Running);
        assert_eq!(mat.last_sequence, 5);
    }

    std::fs::remove_file(&path).ok();
}

/// Snapshot + delta recovery.
#[tokio::test]
async fn test_recovery_with_snapshot() {
    let path = temp_db_path();
    let id = ExecutionId::new();

    {
        let db = open_db(&path).await;
        db.create_execution(sample_execution(&id)).await.unwrap();

        // Write 5 events, then snapshot, then 2 more events.
        for seq in 1..=5i64 {
            let event = Event::new(
                id.clone(),
                seq,
                EventKind::NodeCompleted {
                    node_id: format!("n{seq}"),
                    output: json!(seq),
                    state_patch: json!({ format!("n{seq}"): seq }),
                    duration_ms: 1,
                    gen_ai_system: None,
                    gen_ai_model: None,
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: None,
                    cost_usd: None,
                    provenance: None,
                },
            );
            db.append_event(event).await.unwrap();
        }

        // Write snapshot at sequence 5.
        let snap = Snapshot::new(
            id.clone(),
            5,
            json!({ "n1": 1, "n2": 2, "n3": 3, "n4": 4, "n5": 5 }),
        );
        db.write_snapshot(snap).await.unwrap();

        // 2 more events after snapshot.
        db.append_event(Event::new(
            id.clone(),
            6,
            EventKind::NodeCompleted {
                node_id: "n6".into(),
                output: json!(6),
                state_patch: json!({ "n6": 6 }),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
            },
        ))
        .await
        .unwrap();
        db.append_event(Event::new(
            id.clone(),
            7,
            EventKind::WorkflowCompleted {
                final_state: json!({ "done": true }),
            },
        ))
        .await
        .unwrap();
    }

    // Re-open and recover.
    {
        let db = open_db(&path).await;
        let mat = materialize(&db, &id).await.unwrap();

        // WorkflowCompleted at seq 7 replaces current_state with final_state.
        // The intermediate state (n1..n6 from snapshot + delta) is superseded.
        assert_eq!(mat.current_state["done"], true);
        assert_eq!(mat.status, WorkflowStatus::Completed);
        // n6 was applied from delta event (seq 6) before WorkflowCompleted.
        assert!(mat.completed_nodes.contains_key("n6"));
        assert_eq!(mat.last_sequence, 7);
    }

    std::fs::remove_file(&path).ok();
}

/// F.1 — IR graph validation tests (already in ir/src/validate.rs — cross-check here).
#[tokio::test]
async fn test_event_sequencing() {
    let path = temp_db_path();
    let db = open_db(&path).await;
    let id = ExecutionId::new();
    db.create_execution(sample_execution(&id)).await.unwrap();

    // Append events with explicit sequences and verify ordering.
    for seq in [3i64, 1, 2] {
        db.append_event(Event::new(
            id.clone(),
            seq,
            EventKind::NodeScheduled {
                node_id: format!("n{seq}"),
                queue_type: "tool".into(),
            },
        ))
        .await
        .unwrap();
    }

    let events = db.get_events(&id).await.unwrap();
    // Events should come back in sequence order.
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].sequence, 2);
    assert_eq!(events[2].sequence, 3);

    // get_events_since(1) should return sequences 2 and 3 only.
    let since = db.get_events_since(&id, 1).await.unwrap();
    assert_eq!(since.len(), 2);
    assert_eq!(since[0].sequence, 2);

    std::fs::remove_file(&path).ok();
}
