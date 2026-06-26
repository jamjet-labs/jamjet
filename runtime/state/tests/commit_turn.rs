//! Tests for `commit_turn`: the fenced commit that also writes a per-turn
//! snapshot in the same `BEGIN IMMEDIATE` transaction.
//!
//! Also covers: Snapshot round-trip, replay-equivalence property.
//!
//! TDD: tests are written RED first, then turn GREEN after the fix.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    apply_events, materialize,
    backend::{StateBackend, StateBackendError, WorkItem},
    event::{Event, EventKind},
    snapshot::Snapshot,
    SqliteBackend,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

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

/// Replay-equivalence property: materialize() from a mid-run snapshot plus
/// tail events must equal apply_events() over the full event log.
///
/// Sequence of events:
///   1: WorkflowStarted
///   2: NodeScheduled n1
///   3: NodeCompleted n1  (patch {a:1})   ← snapshot cut here (K=3)
///   4: NodeScheduled n2
///   5: NodeCompleted n2  (patch {b:2})
///
/// The snapshot carries completed_nodes={n1:"r1"}, so after seeding from it
/// and applying events 4-5, completed_nodes must contain BOTH n1 and n2.
///
/// RED before Task 2: materialize seeds derived maps empty, so n1 is dropped.
#[tokio::test]
async fn test_materialize_equals_full_apply_after_snapshot() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let initial = json!({});

    // Append all five events; the backend auto-assigns sequences 1-5.
    let kinds: Vec<EventKind> = vec![
        EventKind::WorkflowStarted {
            workflow_id: "test-wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: initial.clone(),
        },
        EventKind::NodeScheduled {
            node_id: "n1".into(),
            queue_type: "tool".into(),
        },
        EventKind::NodeCompleted {
            node_id: "n1".into(),
            output: json!("r1"),
            state_patch: json!({"a": 1}),
            duration_ms: 10,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
        },
        EventKind::NodeScheduled {
            node_id: "n2".into(),
            queue_type: "tool".into(),
        },
        EventKind::NodeCompleted {
            node_id: "n2".into(),
            output: json!("r2"),
            state_patch: json!({"b": 2}),
            duration_ms: 10,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
        },
    ];

    for kind in &kinds {
        db.append_event(Event::new(exec_id.clone(), 0, kind.clone()))
            .await
            .unwrap();
    }

    // Load events back from DB (sequences are now DB-assigned 1-5).
    let all_events = db.get_events(&exec_id).await.unwrap();
    assert_eq!(all_events.len(), 5, "expected 5 events in the log");

    // Reference result: apply ALL events from the initial state.
    let full = apply_events(initial.clone(), &all_events, &WorkflowStatus::Running);

    // Snapshot cut at K=3 (WorkflowStarted + NodeScheduled n1 + NodeCompleted n1).
    let state_at_k3 = apply_events(initial.clone(), &all_events[0..3], &WorkflowStatus::Running);
    // state_at_k3.last_sequence == 3; completed_nodes = {n1: "r1"}
    assert_eq!(state_at_k3.last_sequence, 3);
    assert!(
        state_at_k3.completed_nodes.contains_key("n1"),
        "n1 must be in completed_nodes at K=3"
    );

    let snap = Snapshot::from_materialized(exec_id.clone(), &state_at_k3);
    assert_eq!(snap.at_sequence, 3, "snapshot at_sequence must be 3");
    db.write_snapshot(snap).await.unwrap();

    // materialize() loads the snapshot (at_sequence=3) and replays events 4 & 5.
    let mat = materialize(&db, &exec_id).await.unwrap();

    assert_eq!(
        mat.current_state, full.current_state,
        "current_state mismatch after snapshot-seeded replay"
    );
    assert_eq!(mat.status, full.status, "status mismatch");
    assert_eq!(
        mat.completed_nodes, full.completed_nodes,
        "completed_nodes mismatch: n1 must survive the snapshot cut"
    );
    assert_eq!(mat.active_nodes, full.active_nodes, "active_nodes mismatch");
    assert_eq!(mat.last_sequence, full.last_sequence, "last_sequence mismatch");
}

// ── Helper fixtures ───────────────────────────────────────────────────────────

fn sample_work_item(execution_id: &ExecutionId) -> WorkItem {
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

fn node_completed_kind(node: &str) -> EventKind {
    EventKind::NodeCompleted {
        node_id: node.into(),
        output: json!({"ok": true}),
        state_patch: json!({}),
        duration_ms: 1,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
    }
}

fn build_snapshot(exec_id: &ExecutionId) -> Snapshot {
    let mut completed_nodes = HashMap::new();
    completed_nodes.insert("n1".to_string(), json!("result"));
    let mut active_nodes = HashSet::new();
    active_nodes.insert("n2".to_string());

    Snapshot {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
        at_sequence: 0, // will be overwritten by commit_turn
        state: json!({"x": 1}),
        status: WorkflowStatus::Running,
        completed_nodes,
        active_nodes,
        last_sequence: 0, // will be overwritten by commit_turn
        created_at: Utc::now(),
    }
}

// ── Task 3 tests ─────────────────────────────────────────────────────────────

/// `commit_turn` with `Some(snapshot)`: settles the work item, appends exactly
/// one terminal event, AND writes the snapshot (with at_sequence == the assigned
/// event sequence) in the same transaction. `latest_snapshot` returns it.
#[tokio::test]
async fn commit_turn_with_snapshot_writes_event_and_snapshot() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id)).await.unwrap();
    db.enqueue_work_item(sample_work_item(&exec_id)).await.unwrap();

    let item = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();

    let event = Event::new(exec_id.clone(), 0, node_completed_kind("n1"));
    let snap = build_snapshot(&exec_id);

    let seq = db
        .commit_turn(item.id, item.lease_fence, event, Some(snap))
        .await
        .expect("commit_turn with snapshot must succeed");

    assert!(seq >= 1, "returned sequence must be at least 1");

    // Exactly one event appended.
    let events = db.get_events(&exec_id).await.unwrap();
    assert_eq!(events.len(), 1, "exactly one event must exist");
    assert!(
        matches!(events[0].kind, EventKind::NodeCompleted { .. }),
        "event must be NodeCompleted"
    );
    assert_eq!(events[0].sequence, seq, "event sequence must match returned seq");

    // Snapshot was written with at_sequence == the terminal event's sequence.
    let loaded_snap = db
        .latest_snapshot(&exec_id)
        .await
        .unwrap()
        .expect("snapshot must exist after commit_turn");

    assert_eq!(
        loaded_snap.at_sequence, seq,
        "snapshot.at_sequence must equal the terminal event sequence"
    );
    assert_eq!(
        loaded_snap.last_sequence, seq,
        "snapshot.last_sequence must equal the terminal event sequence"
    );
    assert_eq!(
        loaded_snap.status,
        WorkflowStatus::Running,
        "snapshot status must round-trip"
    );
    assert!(
        loaded_snap.completed_nodes.contains_key("n1"),
        "completed_nodes must round-trip"
    );
    assert!(
        loaded_snap.active_nodes.contains("n2"),
        "active_nodes must round-trip"
    );
}

/// Stale-fence path: `commit_turn` with a snapshot and a stale fence must
/// return `FenceLost` and write NEITHER an event NOR a snapshot.
#[tokio::test]
async fn commit_turn_stale_fence_writes_nothing() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id)).await.unwrap();
    db.enqueue_work_item(sample_work_item(&exec_id)).await.unwrap();

    // Worker A claims (fence F1). Expire + re-claim as worker B (fence F2).
    // Worker A is now a zombie holding stale fence F1.
    let zombie = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();

    // Backdate the lease so claim_work_item's stale-expiry path fires.
    sqlx::query(
        "UPDATE work_items SET lease_expires_at = '2020-01-01T00:00:00+00:00' WHERE id = ?",
    )
    .bind(zombie.id.to_string())
    .execute(&db.pool())
    .await
    .unwrap();

    let _b = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();

    // Zombie tries to commit with stale fence and a snapshot.
    let event = Event::new(exec_id.clone(), 0, node_completed_kind("n1"));
    let snap = build_snapshot(&exec_id);

    let err = db
        .commit_turn(zombie.id, zombie.lease_fence, event, Some(snap))
        .await
        .expect_err("stale-fence commit_turn must fail closed");

    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );

    // Zero events written.
    assert_eq!(
        db.get_events(&exec_id).await.unwrap().len(),
        0,
        "stale-fence commit_turn must emit zero events"
    );

    // No snapshot written.
    assert!(
        db.latest_snapshot(&exec_id).await.unwrap().is_none(),
        "stale-fence commit_turn must write no snapshot"
    );
}
