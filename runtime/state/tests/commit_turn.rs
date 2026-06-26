//! Tests for `commit_turn`: the fenced commit that also writes a per-turn
//! snapshot in the same `BEGIN IMMEDIATE` transaction.
//!
//! Also covers: Snapshot round-trip, replay-equivalence property.
//!
//! TDD: tests are written RED first, then turn GREEN after the fix.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::tenant::TenantId;
use jamjet_state::{
    apply_events, apply_events_seeded,
    backend::{StateBackend, StateBackendError, WorkItem},
    event::{Event, EventKind},
    materialize,
    snapshot::Snapshot,
    InMemoryBackend, MaterializedState, SqliteBackend,
};
use proptest::prelude::*;
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
    assert_eq!(
        mat.last_sequence, full.last_sequence,
        "last_sequence mismatch"
    );
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
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    db.enqueue_work_item(sample_work_item(&exec_id))
        .await
        .unwrap();

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
    assert_eq!(
        events[0].sequence, seq,
        "event sequence must match returned seq"
    );

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
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    db.enqueue_work_item(sample_work_item(&exec_id))
        .await
        .unwrap();

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

// ── Task 5 helpers ────────────────────────────────────────────────────────────

fn arb_nc_spec() -> impl Strategy<Value = (usize, usize, i64)> {
    // (node_pool_index 0-3, key_pool_index 0-3, patch_value 0-99)
    (0..4usize, 0..4usize, 0i64..100i64)
}

/// Build a NodeCompleted event kind for use in the proptest.
fn nc_kind_for_spec(ni: usize, ki: usize, pv: i64) -> EventKind {
    let node_pool = ["n0", "n1", "n2", "n3"];
    let key_pool = ["a", "b", "c", "d"];
    EventKind::NodeCompleted {
        node_id: node_pool[ni].to_string(),
        output: json!({"ok": true}),
        state_patch: json!({key_pool[ki]: pv}),
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

// ── Group 1: Replay-equivalence proptest ─────────────────────────────────────
//
// Generates random sequences of NodeCompleted events with repeated node ids
// and small JSON patches. For EVERY cut K in 0..=N, asserts that:
//   apply_events_seeded(Snapshot::from_materialized(fold[0..K]), events[K..])
//       == apply_events(initial, &all_events)
// on all four fields: current_state, status, completed_nodes, active_nodes.
//
// This is a pure fold (no DB), so it can run synchronously.
proptest! {
    #[test]
    fn replay_equivalence(
        specs in prop::collection::vec(arb_nc_spec(), 1..=12)
    ) {
        let exec_id = ExecutionId::new();
        let events: Vec<Event> = specs
            .iter()
            .enumerate()
            .map(|(i, &(ni, ki, pv))| {
                Event::new(exec_id.clone(), (i + 1) as i64, nc_kind_for_spec(ni, ki, pv))
            })
            .collect();

        let initial = json!({});
        let full = apply_events(initial.clone(), &events, &WorkflowStatus::Running);

        for k in 0..=events.len() {
            // Fold events[0..k] to get the mid-run materialized state.
            let mid = apply_events(initial.clone(), &events[0..k], &WorkflowStatus::Running);

            // Mirror what the worker does: build a Snapshot from that state.
            let snap = Snapshot::from_materialized(exec_id.clone(), &mid);

            // Seed a fresh MaterializedState from the snapshot and fold the tail.
            let seed = MaterializedState {
                current_state: snap.state,
                status: snap.status,
                completed_nodes: snap.completed_nodes,
                active_nodes: snap.active_nodes,
                last_sequence: snap.last_sequence,
            };
            let resumed = apply_events_seeded(seed, &events[k..]);

            prop_assert_eq!(
                &resumed.current_state,
                &full.current_state,
                "current_state mismatch at k={}",
                k
            );
            prop_assert_eq!(
                resumed.status,
                full.status.clone(),
                "status mismatch at k={}",
                k
            );
            prop_assert_eq!(
                &resumed.completed_nodes,
                &full.completed_nodes,
                "completed_nodes mismatch at k={} (pre-snapshot node lost?)",
                k
            );
            prop_assert_eq!(
                &resumed.active_nodes,
                &full.active_nodes,
                "active_nodes mismatch at k={}",
                k
            );
        }
    }
}

// ── Group 2: Fresh-process SQLite resume ─────────────────────────────────────
//
// Writes 3 successive per-turn snapshots via commit_turn into a temp SQLite
// file, drops the backend, reopens the file, and asserts that materialize()
// produces the same result as a from-origin apply_events fold.
//
// Also verifies that __budget placed in an early turn's state_patch survives
// in the reopened snapshot's state (the budget key is just a regular JSON
// merge-patch key living in current_state).
#[tokio::test]
async fn fresh_process_sqlite_resume() {
    let db_path = std::path::PathBuf::from(format!("/tmp/jamjet-test-{}.sqlite3", Uuid::new_v4()));
    let url = format!("sqlite://{}", db_path.display());

    let db = SqliteBackend::open(&url)
        .await
        .expect("failed to open temp SQLite file");

    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let initial = json!({});

    // Materialize-and-advance helper: returns the new MaterializedState after
    // applying `kind` on top of `prior` (using sequence 0; commit_turn will
    // overwrite at_sequence / last_sequence to the real assigned sequence).
    fn advance(
        prior: MaterializedState,
        exec_id: &ExecutionId,
        kind: &EventKind,
    ) -> MaterializedState {
        let dummy = Event::new(exec_id.clone(), 0, kind.clone());
        apply_events_seeded(prior, &[dummy])
    }

    // ── Turn 1: n1 with a __budget key in state_patch ────────────────────────
    let t1_kind = EventKind::NodeCompleted {
        node_id: "n1".into(),
        output: json!("r1"),
        state_patch: json!({"step": "n1", "__budget": {"remaining": 100, "spent": 10}}),
        duration_ms: 5,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
    };

    db.enqueue_work_item(WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
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
    })
    .await
    .unwrap();

    let item1 = db.claim_work_item("w", &["model"]).await.unwrap().unwrap();
    let prior0 = MaterializedState {
        current_state: initial.clone(),
        status: WorkflowStatus::Pending,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 0,
    };
    let state1 = advance(prior0, &exec_id, &t1_kind);
    let snap1 = Snapshot::from_materialized(exec_id.clone(), &state1);
    db.commit_turn(
        item1.id,
        item1.lease_fence,
        Event::new(exec_id.clone(), 0, t1_kind),
        Some(snap1),
    )
    .await
    .unwrap();

    // ── Turn 2: n2 ───────────────────────────────────────────────────────────
    let t2_kind = EventKind::NodeCompleted {
        node_id: "n2".into(),
        output: json!("r2"),
        state_patch: json!({"step": "n2"}),
        duration_ms: 5,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
    };

    db.enqueue_work_item(WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
        node_id: "n2".into(),
        queue_type: "model".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "default".into(),
        lease_fence: 0,
    })
    .await
    .unwrap();

    let item2 = db.claim_work_item("w", &["model"]).await.unwrap().unwrap();
    let state2 = advance(state1.clone(), &exec_id, &t2_kind);
    let snap2 = Snapshot::from_materialized(exec_id.clone(), &state2);
    db.commit_turn(
        item2.id,
        item2.lease_fence,
        Event::new(exec_id.clone(), 0, t2_kind),
        Some(snap2),
    )
    .await
    .unwrap();

    // ── Turn 3: n3 ───────────────────────────────────────────────────────────
    let t3_kind = EventKind::NodeCompleted {
        node_id: "n3".into(),
        output: json!("r3"),
        state_patch: json!({"step": "n3"}),
        duration_ms: 5,
        gen_ai_system: None,
        gen_ai_model: None,
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        cost_usd: None,
        provenance: None,
    };

    db.enqueue_work_item(WorkItem {
        id: Uuid::new_v4(),
        execution_id: exec_id.clone(),
        node_id: "n3".into(),
        queue_type: "model".into(),
        payload: json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        tenant_id: "default".into(),
        lease_fence: 0,
    })
    .await
    .unwrap();

    let item3 = db.claim_work_item("w", &["model"]).await.unwrap().unwrap();
    let state3 = advance(state2.clone(), &exec_id, &t3_kind);
    let snap3 = Snapshot::from_materialized(exec_id.clone(), &state3);
    db.commit_turn(
        item3.id,
        item3.lease_fence,
        Event::new(exec_id.clone(), 0, t3_kind),
        Some(snap3),
    )
    .await
    .unwrap();

    // Drop the backend so all connections are closed.
    drop(db);

    // ── Fresh process: reopen the file ───────────────────────────────────────
    let db2 = SqliteBackend::open(&url)
        .await
        .expect("failed to reopen temp SQLite file");

    let all_events = db2.get_events(&exec_id).await.unwrap();
    assert_eq!(all_events.len(), 3, "expected exactly 3 events in the log");

    // From-origin reference fold.
    let full = apply_events(initial.clone(), &all_events, &WorkflowStatus::Running);

    // Resume via materialize() (loads latest snapshot + applies tail events).
    let resumed = materialize(&db2, &exec_id).await.unwrap();

    assert_eq!(
        resumed.current_state, full.current_state,
        "current_state must be byte-identical after fresh-process resume"
    );
    assert_eq!(resumed.status, full.status, "status must match");
    assert_eq!(
        resumed.completed_nodes, full.completed_nodes,
        "completed_nodes must contain all three nodes"
    );
    assert_eq!(
        resumed.active_nodes, full.active_nodes,
        "active_nodes must match"
    );
    assert_eq!(
        resumed.last_sequence, full.last_sequence,
        "last_sequence must match"
    );

    // Budget key placed in turn 1's state_patch must survive in the snapshot state.
    assert!(
        resumed.current_state.get("__budget").is_some(),
        "__budget must survive in the resumed current_state"
    );
    assert_eq!(
        resumed.current_state["__budget"]["remaining"],
        json!(100),
        "__budget.remaining must be 100 after fresh-process resume"
    );

    // Clean up.
    drop(db2);
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

// ── Group 3: Cross-backend resume ────────────────────────────────────────────
//
// Verifies the "write snapshot at mid-point + materialize() == full fold"
// invariant against both InMemoryBackend and TenantScopedSqliteBackend.
//
// Uses append_event + write_snapshot directly (no work-item setup needed)
// because this test is about the seeding/fold correctness, not the fenced
// commit atomicity (that is Task 3's territory).

async fn assert_resume_equivalence_for_backend(backend: &dyn StateBackend) {
    let exec_id = ExecutionId::new();
    backend
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let initial = json!({});

    // Five events: WorkflowStarted, NodeScheduled n1, NodeCompleted n1,
    //              NodeScheduled n2, NodeCompleted n2.
    let kinds: Vec<EventKind> = vec![
        EventKind::WorkflowStarted {
            workflow_id: "xb-wf".into(),
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
            duration_ms: 5,
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
            duration_ms: 5,
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
        backend
            .append_event(Event::new(exec_id.clone(), 0, kind.clone()))
            .await
            .unwrap();
    }

    let all_events = backend.get_events(&exec_id).await.unwrap();
    assert_eq!(all_events.len(), 5);

    // Full from-origin reference.
    let full = apply_events(initial.clone(), &all_events, &WorkflowStatus::Running);

    // Take a snapshot at K=3 (after NodeCompleted n1).
    let state_at_k3 = apply_events(initial.clone(), &all_events[0..3], &WorkflowStatus::Running);
    assert!(
        state_at_k3.completed_nodes.contains_key("n1"),
        "n1 must be in completed_nodes at K=3"
    );

    let snap = Snapshot::from_materialized(exec_id.clone(), &state_at_k3);
    backend.write_snapshot(snap).await.unwrap();

    // materialize() must seed from that snapshot and fold events 4-5.
    let resumed = materialize(backend, &exec_id).await.unwrap();

    assert_eq!(
        resumed.current_state, full.current_state,
        "current_state mismatch"
    );
    assert_eq!(resumed.status, full.status, "status mismatch");
    assert_eq!(
        resumed.completed_nodes, full.completed_nodes,
        "completed_nodes mismatch: n1 must survive the snapshot cut"
    );
    assert_eq!(
        resumed.active_nodes, full.active_nodes,
        "active_nodes mismatch"
    );
    assert_eq!(
        resumed.last_sequence, full.last_sequence,
        "last_sequence mismatch"
    );
}

#[tokio::test]
async fn cross_backend_in_memory() {
    let backend = InMemoryBackend::new();
    assert_resume_equivalence_for_backend(&backend).await;
}

#[tokio::test]
async fn cross_backend_tenant_scoped_sqlite() {
    let db = SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite");
    let scoped = db.for_tenant(TenantId::default());
    assert_resume_equivalence_for_backend(&scoped).await;
}
