//! `commit_turn`: the fenced commit that also writes a per-turn snapshot
//! in the same transaction, and resuming from a snapshot.
//!
//! The pure fold checks (`Snapshot::new`'s defaults, the replay-equivalence
//! property) stay in `jamjet-state`'s `tests/commit_turn.rs`.

use crate::conformance::cases::support::not_on;
use crate::conformance::{Fixture, StoreKind};
use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    apply_events,
    backend::{StateBackend, StateBackendError, WorkItem},
    event::{Event, EventKind},
    materialize,
    snapshot::Snapshot,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

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
        parent_execution_id: None,
        segment_number: 0,
    }
}

/// Full materialized state round-trips through write_snapshot → latest_snapshot.
///
/// Verifies: status, completed_nodes, active_nodes, last_sequence are all
/// persisted and reloaded faithfully by the backend.
pub async fn test_snapshot_full_materialized_state_round_trips(fx: &Fixture) {
    let db = fx.backend.as_ref();
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
pub async fn test_materialize_equals_full_apply_after_snapshot(fx: &Fixture) {
    let db = fx.backend.as_ref();
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
            initiated_by: None,
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
            idempotency_key: None,
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
            idempotency_key: None,
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
    let mat = materialize(db, &exec_id).await.unwrap();

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
        idempotency_key: None,
    }
}

// ── Task 3 tests ─────────────────────────────────────────────────────────────

/// `commit_turn` with `write_snapshot = true`: settles the work item, appends
/// exactly one terminal event, AND writes a snapshot (computed in-tx from the
/// committed event log) in the same transaction. `latest_snapshot` returns it.
///
/// The snapshot is computed from the event log rather than a pre-tx value, so
/// `completed_nodes` reflects the actual committed NodeCompleted event.
pub async fn commit_turn_with_snapshot_writes_event_and_snapshot(fx: &Fixture) {
    let db = fx.backend.as_ref();
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

    let seq = db
        .commit_turn(item.id, item.lease_fence, event, true)
        .await
        .expect("commit_turn with write_snapshot must succeed");

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
    // Snapshot is computed from the event log: NodeCompleted n1 marks n1 as done.
    assert!(
        loaded_snap.completed_nodes.contains_key("n1"),
        "completed_nodes must contain n1 (from the committed NodeCompleted event)"
    );
    // No NodeScheduled event was emitted for any other node, so active_nodes is empty.
    assert!(
        loaded_snap.active_nodes.is_empty(),
        "active_nodes must be empty when no NodeScheduled events preceded the commit"
    );
}

/// Stale-fence path: `commit_turn` with a snapshot and a stale fence must
/// return `FenceLost` and write NEITHER an event NOR a snapshot.
pub async fn commit_turn_stale_fence_writes_nothing(fx: &Fixture) {
    let db = fx.backend.as_ref();
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
    fx.hooks.force_lease_expired(zombie.id).await;

    let _b = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();

    // Zombie tries to commit with stale fence and write_snapshot = true.
    let before = db.get_events(&exec_id).await.unwrap().len();
    let event = Event::new(exec_id.clone(), 0, node_completed_kind("n1"));

    let err = db
        .commit_turn(zombie.id, zombie.lease_fence, event, true)
        .await
        .expect_err("stale-fence commit_turn must fail closed");

    assert!(
        matches!(err, StateBackendError::FenceLost(_)),
        "expected FenceLost, got {err:?}"
    );

    // Zero events written.
    assert_eq!(
        db.get_events(&exec_id).await.unwrap().len(),
        before,
        "stale-fence commit_turn must emit zero events"
    );

    // No snapshot written.
    assert!(
        db.latest_snapshot(&exec_id).await.unwrap().is_none(),
        "stale-fence commit_turn must write no snapshot"
    );
}

// ── Group 2: Fresh-process resume ────────────────────────────────────────────
//
// Writes 3 successive per-turn snapshots via commit_turn, opens the store
// again through a new pool, and asserts that materialize()
// produces the same result as a from-origin apply_events fold.
//
// Also verifies that __budget placed in an early turn's state_patch survives
// in the reopened snapshot's state (the budget key is just a regular JSON
// merge-patch key living in current_state).
pub async fn fresh_process_resume(fx: &Fixture) {
    if not_on(fx, &[StoreKind::Memory], "memory cannot be reopened") {
        return;
    }
    let db = fx.backend.as_ref();

    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    let initial = json!({});

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
        idempotency_key: None,
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
    db.commit_turn(
        item1.id,
        item1.lease_fence,
        Event::new(exec_id.clone(), 0, t1_kind),
        true,
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
        idempotency_key: None,
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
    db.commit_turn(
        item2.id,
        item2.lease_fence,
        Event::new(exec_id.clone(), 0, t2_kind),
        true,
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
        idempotency_key: None,
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
    db.commit_turn(
        item3.id,
        item3.lease_fence,
        Event::new(exec_id.clone(), 0, t3_kind),
        true,
    )
    .await
    .unwrap();

    // ── Fresh process: a new pool on the same store ──────────────────────────
    let db2 = fx
        .hooks
        .reopen()
        .await
        .expect("a store that can be reopened");

    let all_events = db2.get_events(&exec_id).await.unwrap();
    assert_eq!(all_events.len(), 3, "expected exactly 3 events in the log");

    // From-origin reference fold.
    let full = apply_events(initial.clone(), &all_events, &WorkflowStatus::Running);

    // Resume via materialize() (loads latest snapshot + applies tail events).
    let resumed = materialize(&*db2, &exec_id).await.unwrap();

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
}

// ── Group 3: Cross-backend resume ────────────────────────────────────────────
//
// Verifies the "write snapshot at mid-point + materialize() == full fold"
// invariant on every backend.
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
            initiated_by: None,
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
            idempotency_key: None,
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
            idempotency_key: None,
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

pub async fn cross_backend_resume(fx: &Fixture) {
    assert_resume_equivalence_for_backend(fx.backend.as_ref()).await;
}

// ── Regression: concurrent sibling write-skew ────────────────────────────────

/// Concurrent sibling commits must not produce write-skew.
///
/// Two sibling nodes (n_a and n_b) each hold their own work item and commit
/// sequentially in the same test (simulating the losing-race scenario where B
/// commits after A). Because the snapshot is computed IN-TX after the terminal
/// event is inserted, B's snapshot sees A's committed event and must carry
/// BOTH patches. A pre-tx snapshot computation would make B's snapshot miss
/// A's patch (write-skew).
pub async fn concurrent_sibling_commits_no_write_skew(fx: &Fixture) {
    let db = fx.backend.as_ref();
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    // Enqueue two sibling nodes.
    for node_id in ["n_a", "n_b"] {
        db.enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: exec_id.clone(),
            node_id: node_id.into(),
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
    }

    // Two concurrent workers each claim one item.
    let first = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let second = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();

    // Determine which claimed item belongs to which node.
    let (item_a, item_b) = if first.node_id == "n_a" {
        (first, second)
    } else {
        (second, first)
    };

    // Worker A commits first with its patch.
    let ev_a = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "n_a".into(),
            output: json!("out_a"),
            state_patch: json!({"key_a": "value_a"}),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: None,
        },
    );
    db.commit_turn(item_a.id, item_a.lease_fence, ev_a, true)
        .await
        .expect("A commit must succeed");

    // Worker B commits second with its own patch.
    let ev_b = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "n_b".into(),
            output: json!("out_b"),
            state_patch: json!({"key_b": "value_b"}),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: None,
        },
    );
    db.commit_turn(item_b.id, item_b.lease_fence, ev_b, true)
        .await
        .expect("B commit must succeed");

    // The final snapshot must contain BOTH patches: no write-skew.
    let snap = db
        .latest_snapshot(&exec_id)
        .await
        .unwrap()
        .expect("snapshot must exist after both commits");

    assert_eq!(
        snap.state.get("key_a"),
        Some(&json!("value_a")),
        "patch from n_a must be present in final snapshot (write-skew regression)"
    );
    assert_eq!(
        snap.state.get("key_b"),
        Some(&json!("value_b")),
        "patch from n_b must be present in final snapshot (write-skew regression)"
    );
    assert!(
        snap.completed_nodes.contains_key("n_a"),
        "n_a must be in completed_nodes"
    );
    assert!(
        snap.completed_nodes.contains_key("n_b"),
        "n_b must be in completed_nodes"
    );
}

// ── Regression: genuinely concurrent sibling write-skew (two tasks) ──

/// Two tokio tasks race to commit sibling nodes to the SAME execution on
/// one handle of the store. A `Barrier` forces both tasks to reach
/// the commit point before either proceeds, maximising BEGIN IMMEDIATE
/// contention. Both commits must succeed (busy_timeout serialises them), and
/// the final snapshot must contain BOTH patches, proving the in-tx snapshot
/// computation closes the write-skew even under real concurrent writers.
pub async fn concurrent_sibling_commits_race(fx: &Fixture) {
    let db = fx.backend.clone();

    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    // Enqueue two sibling work items for the same execution.
    for node_id in ["n_a", "n_b"] {
        db.enqueue_work_item(WorkItem {
            id: Uuid::new_v4(),
            execution_id: exec_id.clone(),
            node_id: node_id.into(),
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
    }

    // Claim both items before spawning the tasks (both get distinct lease fences).
    let first = db
        .claim_work_item("worker-A", &["model"])
        .await
        .unwrap()
        .unwrap();
    let second = db
        .claim_work_item("worker-B", &["model"])
        .await
        .unwrap()
        .unwrap();

    let (item_a, item_b) = if first.node_id == "n_a" {
        (first, second)
    } else {
        (second, first)
    };

    // Barrier ensures both tasks reach the commit call before either proceeds.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    // ── Task A ────────────────────────────────────────────────────────────────
    let db_a = Arc::clone(&db);
    let barrier_a = Arc::clone(&barrier);
    let exec_id_a = exec_id.clone();
    let handle_a = tokio::spawn(async move {
        let ev = Event::new(
            exec_id_a,
            0,
            EventKind::NodeCompleted {
                node_id: "n_a".into(),
                output: json!("out_a"),
                state_patch: json!({"a": 1}),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: None,
            },
        );
        barrier_a.wait().await;
        db_a.commit_turn(item_a.id, item_a.lease_fence, ev, true)
            .await
    });

    // ── Task B ────────────────────────────────────────────────────────────────
    let db_b = Arc::clone(&db);
    let barrier_b = Arc::clone(&barrier);
    let exec_id_b = exec_id.clone();
    let handle_b = tokio::spawn(async move {
        let ev = Event::new(
            exec_id_b,
            0,
            EventKind::NodeCompleted {
                node_id: "n_b".into(),
                output: json!("out_b"),
                state_patch: json!({"b": 2}),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: None,
            },
        );
        barrier_b.wait().await;
        db_b.commit_turn(item_b.id, item_b.lease_fence, ev, true)
            .await
    });

    // Both commits must succeed.
    handle_a
        .await
        .expect("task A panicked")
        .expect("task A commit_turn failed");
    handle_b
        .await
        .expect("task B panicked")
        .expect("task B commit_turn failed");

    // Final state must carry BOTH patches.
    let all_events = db.get_events(&exec_id).await.unwrap();
    assert_eq!(all_events.len(), 2, "expected exactly 2 events");

    let full = apply_events(json!({}), &all_events, &WorkflowStatus::Running);
    assert!(
        full.current_state.get("a").is_some(),
        "patch a must be in current_state"
    );
    assert!(
        full.current_state.get("b").is_some(),
        "patch b must be in current_state"
    );
    assert!(
        full.completed_nodes.contains_key("n_a"),
        "n_a must be in completed_nodes"
    );
    assert!(
        full.completed_nodes.contains_key("n_b"),
        "n_b must be in completed_nodes"
    );

    // Latest snapshot (written by whichever task committed second) must also
    // contain both patches. This is the write-skew invariant.
    let snap = db
        .latest_snapshot(&exec_id)
        .await
        .unwrap()
        .expect("snapshot must exist after both concurrent commits");
    assert!(
        snap.state.get("a").is_some() && snap.state.get("b").is_some(),
        "snapshot state must contain both patches (a={:?}, b={:?})",
        snap.state.get("a"),
        snap.state.get("b"),
    );
    assert!(
        snap.completed_nodes.contains_key("n_a") && snap.completed_nodes.contains_key("n_b"),
        "snapshot completed_nodes must contain both n_a and n_b"
    );
}
