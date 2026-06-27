//! TDD tests for segment lineage columns and `SegmentBoundary` event.
//!
//! Tests written RED before the model fields existed; turned GREEN after:
//! - `WorkflowExecution::parent_execution_id` and `::segment_number`
//! - `EventKind::SegmentBoundary { segment_number, next_execution_id }`
//! - migration 0007_segment_links.sql
//! - `create_execution` / `get_execution` wired to the two new DB columns
//! - `apply_events_seeded` explicit no-op arm for `SegmentBoundary`

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{
    backend::{StateBackend, WorkItem},
    content_hash, materialize, segment_chain, start_next_segment, Event, EventKind,
    InMemoryBackend, MaterializedState, Snapshot, SqliteBackend,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

// ── Shared helpers ────────────────────────────────────────────────────────────

async fn open_sqlite() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite for segment tests")
}

fn root_execution(id: &ExecutionId) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "seg-wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "counter": 0 }),
        current_state: json!({ "counter": 0 }),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: None,
        segment_number: 0,
    }
}

fn child_execution(
    id: &ExecutionId,
    parent: &ExecutionId,
    segment_number: u32,
) -> WorkflowExecution {
    let now = Utc::now();
    WorkflowExecution {
        execution_id: id.clone(),
        workflow_id: "seg-wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: json!({ "counter": 42, "carried": true }),
        current_state: json!({ "counter": 42, "carried": true }),
        started_at: now,
        updated_at: now,
        completed_at: None,
        session_type: None,
        parent_execution_id: Some(parent.clone()),
        segment_number,
    }
}

// ── SQLite backend ────────────────────────────────────────────────────────────

/// A child execution (segment 1) carries `parent_execution_id` and
/// `segment_number=1` round-trip through SQLite.
#[tokio::test]
async fn sqlite_linked_execution_round_trips() {
    let db = open_sqlite().await;

    let parent_id = ExecutionId::new();
    db.create_execution(root_execution(&parent_id))
        .await
        .expect("create root");

    let child_id = ExecutionId::new();
    db.create_execution(child_execution(&child_id, &parent_id, 1))
        .await
        .expect("create child");

    let fetched = db
        .get_execution(&child_id)
        .await
        .expect("get_execution")
        .expect("child must exist");

    assert_eq!(fetched.segment_number, 1, "segment_number must be 1");
    assert_eq!(
        fetched.parent_execution_id.as_ref(),
        Some(&parent_id),
        "parent_execution_id must match"
    );
    assert_eq!(
        fetched.current_state,
        json!({ "counter": 42, "carried": true })
    );
}

/// A root execution (no parent) reads back `segment_number=0` and
/// `parent_execution_id=None` from SQLite.
#[tokio::test]
async fn sqlite_root_execution_has_defaults() {
    let db = open_sqlite().await;

    let root_id = ExecutionId::new();
    db.create_execution(root_execution(&root_id))
        .await
        .expect("create root");

    let fetched = db
        .get_execution(&root_id)
        .await
        .expect("get_execution")
        .expect("root must exist");

    assert_eq!(fetched.segment_number, 0, "root segment_number must be 0");
    assert!(
        fetched.parent_execution_id.is_none(),
        "root parent_execution_id must be None"
    );
}

/// Appending a `SegmentBoundary` event then materializing the OLD execution
/// does NOT change `current_state` — it is a pure audit record.
#[tokio::test]
async fn sqlite_segment_boundary_event_is_materialize_noop() {
    let db = open_sqlite().await;

    let exec_id = ExecutionId::new();
    let next_id = ExecutionId::new();
    db.create_execution(root_execution(&exec_id))
        .await
        .expect("create root");

    // Append a WorkflowStarted so the execution is in Running state.
    db.append_event(Event::new(
        exec_id.clone(),
        1,
        EventKind::WorkflowStarted {
            workflow_id: "seg-wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({ "counter": 0 }),
        },
    ))
    .await
    .expect("append WorkflowStarted");

    // Materialize before SegmentBoundary.
    let before = materialize(&db, &exec_id)
        .await
        .expect("materialize before");
    let state_before = before.current_state.clone();

    // Append the SegmentBoundary audit event.
    db.append_event(Event::new(
        exec_id.clone(),
        2,
        EventKind::SegmentBoundary {
            segment_number: 1,
            next_execution_id: next_id.to_string(),
        },
    ))
    .await
    .expect("append SegmentBoundary");

    // Materialize after SegmentBoundary — state must be unchanged.
    let after = materialize(&db, &exec_id).await.expect("materialize after");
    assert_eq!(
        after.current_state, state_before,
        "SegmentBoundary must not mutate current_state"
    );
    assert_eq!(after.last_sequence, 2, "last_sequence must advance to 2");
}

// ── In-memory backend ─────────────────────────────────────────────────────────

/// Same linked-execution round-trip, but on the in-memory backend.
#[tokio::test]
async fn memory_linked_execution_round_trips() {
    let backend = InMemoryBackend::new();

    let parent_id = ExecutionId::new();
    backend
        .create_execution(root_execution(&parent_id))
        .await
        .expect("create root");

    let child_id = ExecutionId::new();
    backend
        .create_execution(child_execution(&child_id, &parent_id, 1))
        .await
        .expect("create child");

    let fetched = backend
        .get_execution(&child_id)
        .await
        .expect("get_execution")
        .expect("child must exist");

    assert_eq!(fetched.segment_number, 1);
    assert_eq!(fetched.parent_execution_id.as_ref(), Some(&parent_id));
}

/// Root execution defaults on the in-memory backend.
#[tokio::test]
async fn memory_root_execution_has_defaults() {
    let backend = InMemoryBackend::new();

    let root_id = ExecutionId::new();
    backend
        .create_execution(root_execution(&root_id))
        .await
        .expect("create root");

    let fetched = backend
        .get_execution(&root_id)
        .await
        .expect("get_execution")
        .expect("root must exist");

    assert_eq!(fetched.segment_number, 0);
    assert!(fetched.parent_execution_id.is_none());
}

/// `SegmentBoundary` is a materialize no-op on the in-memory backend.
#[tokio::test]
async fn memory_segment_boundary_event_is_materialize_noop() {
    let backend = InMemoryBackend::new();

    let exec_id = ExecutionId::new();
    let next_id = ExecutionId::new();
    backend
        .create_execution(root_execution(&exec_id))
        .await
        .expect("create root");

    backend
        .append_event(Event::new(
            exec_id.clone(),
            1,
            EventKind::WorkflowStarted {
                workflow_id: "seg-wf".into(),
                workflow_version: "1.0.0".into(),
                initial_input: json!({ "counter": 0 }),
            },
        ))
        .await
        .expect("append WorkflowStarted");

    let before = materialize(&backend, &exec_id)
        .await
        .expect("materialize before");
    let state_before = before.current_state.clone();

    backend
        .append_event(Event::new(
            exec_id.clone(),
            2,
            EventKind::SegmentBoundary {
                segment_number: 1,
                next_execution_id: next_id.to_string(),
            },
        ))
        .await
        .expect("append SegmentBoundary");

    let after = materialize(&backend, &exec_id)
        .await
        .expect("materialize after");
    assert_eq!(
        after.current_state, state_before,
        "SegmentBoundary must not mutate current_state"
    );
}

// ── Tenant-scoped backend ─────────────────────────────────────────────────────

// ── start_next_segment helpers ────────────────────────────────────────────────

/// Builds a `MaterializedState` representing the terminal state of a segment.
/// Carries a non-trivial nested JSON value so byte-identity is meaningful.
fn carried_materialized_state() -> MaterializedState {
    MaterializedState {
        current_state: json!({
            "counter": 42,
            "nested": { "a": [1, 2, 3] },
            "carried": true
        }),
        status: WorkflowStatus::Running,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 0,
    }
}

/// Run the full `start_next_segment` assertion suite against any `StateBackend`.
async fn assert_start_next_segment(backend: &dyn StateBackend) {
    let parent_id = ExecutionId::new();
    // Parent execution must exist (FK-safe even though column is nullable TEXT).
    backend
        .create_execution(root_execution(&parent_id))
        .await
        .expect("create parent");

    let carried = carried_materialized_state();

    let new_id = start_next_segment(
        backend, &parent_id, &carried, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("start_next_segment must succeed");

    // ── New execution exists with a DIFFERENT id ──────────────────────────
    assert_ne!(new_id, parent_id, "new_id must differ from parent_id");

    let new_exec = backend
        .get_execution(&new_id)
        .await
        .expect("get_execution")
        .expect("new execution must exist");

    // ── Lineage fields ────────────────────────────────────────────────────
    assert_eq!(
        new_exec.parent_execution_id.as_ref(),
        Some(&parent_id),
        "parent_execution_id must link back to parent"
    );
    assert_eq!(
        new_exec.segment_number, 1,
        "segment_number must be next_segment_number"
    );

    // ── Carried state ─────────────────────────────────────────────────────
    assert_eq!(
        new_exec.initial_input, carried.current_state,
        "initial_input must equal the carried current_state"
    );
    assert_eq!(
        new_exec.current_state, carried.current_state,
        "current_state must equal the carried current_state"
    );

    // ── Byte-identity ─────────────────────────────────────────────────────
    assert_eq!(
        content_hash(&new_exec.current_state),
        content_hash(&carried.current_state),
        "content_hash of new current_state must match the carried state"
    );

    // ── Seed snapshot ─────────────────────────────────────────────────────
    let snap = backend
        .latest_snapshot(&new_id)
        .await
        .expect("latest_snapshot")
        .expect("seed snapshot must exist");
    assert_eq!(
        snap.state, carried.current_state,
        "seed snapshot state must equal the carried current_state"
    );

    // ── materialize returns carried state ─────────────────────────────────
    let mat = materialize(backend, &new_id)
        .await
        .expect("materialize must succeed");
    assert_eq!(
        mat.current_state, carried.current_state,
        "materialize(new_id).current_state must equal the carried state"
    );

    // ── Events: WorkflowStarted (seq 1) then NodeScheduled (seq 2) ────────
    let events = backend.get_events(&new_id).await.expect("get_events");
    assert_eq!(events.len(), 2, "must have exactly 2 events");
    assert!(
        matches!(events[0].kind, EventKind::WorkflowStarted { .. }),
        "first event must be WorkflowStarted"
    );
    assert_eq!(events[0].sequence, 1, "WorkflowStarted must be seq 1");
    match &events[1].kind {
        EventKind::NodeScheduled {
            node_id,
            queue_type,
        } => {
            assert_eq!(node_id, "start", "NodeScheduled node_id must be 'start'");
            assert_eq!(
                queue_type, "general",
                "NodeScheduled queue_type must be 'general'"
            );
        }
        other => panic!("expected NodeScheduled, got {other:?}"),
    }
    assert_eq!(events[1].sequence, 2, "NodeScheduled must be seq 2");

    // ── Work item is enqueued and claimable ───────────────────────────────
    let item = backend
        .claim_work_item("test-worker-1", &["general"])
        .await
        .expect("claim_work_item")
        .expect("a work item must be available");
    assert_eq!(
        item.execution_id, new_id,
        "work item must belong to the new segment"
    );
    assert_eq!(
        item.node_id, "start",
        "work item must target the start node"
    );
}

// ── start_next_segment tests — SQLite ─────────────────────────────────────────

/// `start_next_segment` creates the linked, state-seeded continuation on SQLite.
#[tokio::test]
async fn sqlite_start_next_segment() {
    let db = open_sqlite().await;
    assert_start_next_segment(&db).await;
}

// ── start_next_segment tests — in-memory ──────────────────────────────────────

/// `start_next_segment` creates the linked, state-seeded continuation in memory.
#[tokio::test]
async fn memory_start_next_segment() {
    let backend = InMemoryBackend::new();
    assert_start_next_segment(&backend).await;
}

// ── Same linked-execution round-trip on the tenant-scoped SQLite backend.
#[tokio::test]
async fn tenant_scoped_linked_execution_round_trips() {
    use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};

    let db = open_sqlite().await;

    // Register a non-default tenant.
    let tenant_id = TenantId::from("acme");
    let scoped_admin = db.for_tenant(TenantId::default());
    let now = Utc::now();
    scoped_admin
        .create_tenant(Tenant {
            id: tenant_id.clone(),
            name: "Acme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("register tenant");

    let scoped = db.for_tenant(tenant_id);

    let parent_id = ExecutionId::new();
    scoped
        .create_execution(root_execution(&parent_id))
        .await
        .expect("create root");

    let child_id = ExecutionId::new();
    scoped
        .create_execution(child_execution(&child_id, &parent_id, 1))
        .await
        .expect("create child");

    let fetched = scoped
        .get_execution(&child_id)
        .await
        .expect("get_execution")
        .expect("child must exist");

    assert_eq!(fetched.segment_number, 1);
    assert_eq!(fetched.parent_execution_id.as_ref(), Some(&parent_id));
}

// ── segment_chain helper ──────────────────────────────────────────────────────

/// Appends `count` minimal events to `exec_id` starting at sequence `start_seq`.
/// Uses bare `NodeCompleted` events with a null state_patch (no state mutation).
async fn append_many_events(
    backend: &dyn StateBackend,
    exec_id: &ExecutionId,
    start_seq: i64,
    count: usize,
) {
    for i in 0..count {
        let seq = start_seq + i as i64;
        backend
            .append_event(Event::new(
                exec_id.clone(),
                seq,
                EventKind::NodeCompleted {
                    node_id: "loop_body".into(),
                    output: json!({}),
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
                },
            ))
            .await
            .expect("append_many_events: append failed");
    }
}

/// Generic lineage test: root -> seg1 -> seg2.
/// `segment_chain(backend, &seg2_id)` must return [root_id, seg1_id, seg2_id],
/// each with the correct segment_number (0, 1, 2).
async fn assert_segment_chain_lineage(backend: &dyn StateBackend) {
    let root_id = ExecutionId::new();
    backend
        .create_execution(root_execution(&root_id))
        .await
        .expect("create root");

    let seg1_carried = MaterializedState {
        current_state: json!({ "counter": 1 }),
        status: WorkflowStatus::Running,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 0,
    };

    let seg1_id = start_next_segment(
        backend,
        &root_id,
        &seg1_carried,
        "seg-wf",
        "1.0.0",
        1,
        "start",
        "general",
        "default",
    )
    .await
    .expect("start_next_segment seg1");

    // Drain the work item so a second claim does not interfere.
    backend
        .claim_work_item("cleanup-chain-1", &["general"])
        .await
        .ok();

    let seg2_carried = MaterializedState {
        current_state: json!({ "counter": 2 }),
        status: WorkflowStatus::Running,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 0,
    };

    let seg2_id = start_next_segment(
        backend,
        &seg1_id,
        &seg2_carried,
        "seg-wf",
        "1.0.0",
        2,
        "start",
        "general",
        "default",
    )
    .await
    .expect("start_next_segment seg2");

    // Drain so the queue does not leak into other tests.
    backend
        .claim_work_item("cleanup-chain-2", &["general"])
        .await
        .ok();

    // Walk the chain from seg2 back to root.
    let chain = segment_chain(backend, &seg2_id)
        .await
        .expect("segment_chain must succeed");

    assert_eq!(chain.len(), 3, "chain must have 3 entries");
    assert_eq!(chain[0], root_id, "chain[0] must be root");
    assert_eq!(chain[1], seg1_id, "chain[1] must be seg1");
    assert_eq!(chain[2], seg2_id, "chain[2] must be seg2");

    // Verify segment_number is monotonically 0, 1, 2.
    for (expected_seg_num, exec_id) in [(0u32, &chain[0]), (1u32, &chain[1]), (2u32, &chain[2])] {
        let exec = backend
            .get_execution(exec_id)
            .await
            .expect("get_execution")
            .expect("execution must exist");
        assert_eq!(
            exec.segment_number, expected_seg_num,
            "segment_number mismatch at position {expected_seg_num}"
        );
    }
}

// ── 2g-4 Task 1: segment_chain — SQLite, in-memory, tenant-scoped ─────────────

#[tokio::test]
async fn sqlite_segment_chain_lineage() {
    let db = open_sqlite().await;
    assert_segment_chain_lineage(&db).await;
}

#[tokio::test]
async fn memory_segment_chain_lineage() {
    let backend = InMemoryBackend::new();
    assert_segment_chain_lineage(&backend).await;
}

#[tokio::test]
async fn tenant_scoped_segment_chain_lineage() {
    use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};

    let db = open_sqlite().await;
    let tenant_id = TenantId::from("chain-acme");
    let scoped_admin = db.for_tenant(TenantId::default());
    let now = Utc::now();
    scoped_admin
        .create_tenant(Tenant {
            id: tenant_id.clone(),
            name: "ChainAcme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("register tenant");

    let scoped = db.for_tenant(tenant_id);
    assert_segment_chain_lineage(&scoped).await;
}

// ── 2g-4 Task 2: bounded-replay proof (SQLite; event count independence) ──────

/// THE proof of 2g: adding 50 events to the root execution must NOT inflate
/// the new segment's event count or replay cost.
///
/// Root gets 50 events (seq 1..=50).
/// `start_next_segment` seeds seg1 with root's materialized state.
/// `get_events(seg1).len()` must equal 2 (WorkflowStarted + NodeScheduled).
/// `materialize(seg1).current_state` must equal the carried state.
#[tokio::test]
async fn sqlite_bounded_replay_independent_of_root_event_count() {
    let db = open_sqlite().await;

    let root_id = ExecutionId::new();
    db.create_execution(root_execution(&root_id))
        .await
        .expect("create root");

    // Give the root a large event log (50 events, seq 1..=50).
    append_many_events(&db, &root_id, 1, 50).await;

    let root_events = db.get_events(&root_id).await.expect("get_events root");
    assert_eq!(root_events.len(), 50, "root must have 50 events");

    // The root's terminal materialized state (after all 50 events).
    let root_mat = materialize(&db, &root_id).await.expect("materialize root");

    let seg1_id = start_next_segment(
        &db, &root_id, &root_mat, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("start_next_segment");

    // Drain the work item (not needed for this test's logic).
    db.claim_work_item("cleanup-bounded", &["general"])
        .await
        .ok();

    // Seg1's event count is SMALL and independent of root's 50 events.
    let seg1_events = db.get_events(&seg1_id).await.expect("get_events seg1");
    assert_eq!(
        seg1_events.len(),
        2,
        "seg1 must have exactly 2 events (WorkflowStarted + NodeScheduled), got {}",
        seg1_events.len()
    );
    assert!(
        matches!(seg1_events[0].kind, EventKind::WorkflowStarted { .. }),
        "seg1 event[0] must be WorkflowStarted"
    );
    assert!(
        matches!(seg1_events[1].kind, EventKind::NodeScheduled { .. }),
        "seg1 event[1] must be NodeScheduled"
    );

    // materialize(seg1) returns the carried state WITHOUT reading root's 50 events.
    let seg1_mat = materialize(&db, &seg1_id).await.expect("materialize seg1");
    assert_eq!(
        seg1_mat.current_state, root_mat.current_state,
        "seg1.current_state must equal the carried root state"
    );

    // Seed snapshot for seg1 exists and reflects the carried state.
    let snap = db
        .latest_snapshot(&seg1_id)
        .await
        .expect("latest_snapshot")
        .expect("seed snapshot must exist");
    assert_eq!(
        snap.state, root_mat.current_state,
        "seed snapshot must carry the root's terminal state"
    );
}

// ── 2g-4 Task 3: inert old segment — late commit does not leak into new seg ───

/// A late event appended to the OLD (terminal) execution after rollover must NOT
/// change `materialize(seg1).current_state`. Proves the inert-old-segment
/// invariant stated in the Global Constraints.
#[tokio::test]
async fn sqlite_inert_old_segment_does_not_leak() {
    let db = open_sqlite().await;

    let root_id = ExecutionId::new();
    db.create_execution(root_execution(&root_id))
        .await
        .expect("create root");

    // Append one event to root so it is non-empty, then roll over.
    db.append_event(Event::new(
        root_id.clone(),
        1,
        EventKind::WorkflowStarted {
            workflow_id: "seg-wf".into(),
            workflow_version: "1.0.0".into(),
            initial_input: json!({ "counter": 0 }),
        },
    ))
    .await
    .expect("append WorkflowStarted to root");

    let root_mat = materialize(&db, &root_id).await.expect("materialize root");

    let seg1_id = start_next_segment(
        &db, &root_id, &root_mat, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("start_next_segment");

    db.claim_work_item("cleanup-inert", &["general"]).await.ok();

    // Capture seg1's state BEFORE the late commit.
    let seg1_before = materialize(&db, &seg1_id)
        .await
        .expect("materialize seg1 before late commit");

    // Simulate a late / parked NodeCompleted arriving on the OLD execution
    // with a conflicting state_patch.
    db.append_event(Event::new(
        root_id.clone(),
        2,
        EventKind::NodeCompleted {
            node_id: "late_node".into(),
            output: json!({ "result": "LATE" }),
            state_patch: json!({ "counter": 999, "injected": true }),
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
    ))
    .await
    .expect("late append on old execution must succeed (appends to closed log)");

    // seg1's materialized state is UNCHANGED.
    let seg1_after = materialize(&db, &seg1_id)
        .await
        .expect("materialize seg1 after late commit");

    assert_eq!(
        seg1_after.current_state, seg1_before.current_state,
        "inert-old-segment: a late commit on the old execution must not change the new segment's state"
    );
    assert_eq!(
        content_hash(&seg1_after.current_state),
        content_hash(&seg1_before.current_state),
        "content_hash must be unchanged after late commit on old execution"
    );
}

// ── 2g-4/2g-5 Task 4: 2-rollover byte-identity across backends ───────────────

/// Carry a non-trivial nested JSON state through TWO rollovers (root -> seg1 ->
/// seg2). The state must arrive byte-identical at seg2 (`content_hash` equal
/// end-to-end). Tested on SQLite, in-memory, and tenant-scoped backends.
async fn assert_two_rollover_byte_identity(backend: &dyn StateBackend) {
    let nested_state = json!({
        "counter": 7,
        "items": [{"k": "v"}, {"k": "w"}],
        "deep": {"a": {"b": [1, 2, 3]}}
    });

    let root_id = ExecutionId::new();
    backend
        .create_execution(WorkflowExecution {
            execution_id: root_id.clone(),
            workflow_id: "seg-wf".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: nested_state.clone(),
            current_state: nested_state.clone(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .expect("create root");

    let root_mat = MaterializedState {
        current_state: nested_state.clone(),
        status: WorkflowStatus::Running,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 0,
    };

    // root -> seg1
    let seg1_id = start_next_segment(
        backend, &root_id, &root_mat, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("start_next_segment seg1");

    backend
        .claim_work_item("cleanup-2ro-1", &["general"])
        .await
        .ok();

    // Verify seg1 carries the state byte-identically.
    let seg1_exec = backend
        .get_execution(&seg1_id)
        .await
        .expect("get seg1")
        .expect("seg1 must exist");
    assert_eq!(
        content_hash(&seg1_exec.current_state),
        content_hash(&nested_state),
        "seg1.current_state hash must match original"
    );

    // seg1 -> seg2 (second rollover), seeded from materialize(seg1).
    let seg1_mat = materialize(backend, &seg1_id)
        .await
        .expect("materialize seg1");

    let seg2_id = start_next_segment(
        backend, &seg1_id, &seg1_mat, "seg-wf", "1.0.0", 2, "start", "general", "default",
    )
    .await
    .expect("start_next_segment seg2");

    backend
        .claim_work_item("cleanup-2ro-2", &["general"])
        .await
        .ok();

    let seg2_exec = backend
        .get_execution(&seg2_id)
        .await
        .expect("get seg2")
        .expect("seg2 must exist");

    // Byte-identity end-to-end: seg2.current_state hash == original nested_state hash.
    assert_eq!(
        content_hash(&seg2_exec.current_state),
        content_hash(&nested_state),
        "seg2.current_state hash must match the original nested state after 2 rollovers"
    );

    // Lineage chain from seg2 gives [root, seg1, seg2].
    let chain = segment_chain(backend, &seg2_id)
        .await
        .expect("segment_chain from seg2");
    assert_eq!(
        chain,
        vec![root_id.clone(), seg1_id.clone(), seg2_id.clone()]
    );
}

#[tokio::test]
async fn sqlite_two_rollover_byte_identity() {
    let db = open_sqlite().await;
    assert_two_rollover_byte_identity(&db).await;
}

#[tokio::test]
async fn memory_two_rollover_byte_identity() {
    let backend = InMemoryBackend::new();
    assert_two_rollover_byte_identity(&backend).await;
}

#[tokio::test]
async fn tenant_scoped_two_rollover_byte_identity() {
    use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};

    let db = open_sqlite().await;
    let tenant_id = TenantId::from("bytecheck-acme");
    let scoped_admin = db.for_tenant(TenantId::default());
    let now = Utc::now();
    scoped_admin
        .create_tenant(Tenant {
            id: tenant_id.clone(),
            name: "ByteCheckAcme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("register tenant");

    let scoped = db.for_tenant(tenant_id);
    assert_two_rollover_byte_identity(&scoped).await;
}

// ── C2 regression: SQLite child forward-progress after rollover ───────────────

/// Regression test for the "frozen child state" bug on SQLite.
///
/// Root with `last_sequence = 50` rolls over to seg1.  A `NodeCompleted`
/// event is then appended to seg1 at sequence 3 with `state_patch = {"counter": 100}`.
///
/// BUG (before fix): `Snapshot::from_materialized` seeded the child snapshot at
/// `at_sequence = 50` (the parent's last_sequence).  SQLite's
/// `get_events_since(seg1, 50)` returns only events with `sequence > 50`, so
/// the child's seq-1/2/3 events were ALL silently dropped.
/// `materialize(seg1)` returned `{"counter": 0}` — frozen at the carried value.
///
/// FIX: `Snapshot::seed_for_segment` anchors the seed at `at_sequence = 0`.
/// `get_events_since(seg1, 0)` returns all child events; the patch is applied;
/// `materialize(seg1).current_state["counter"]` must equal 100.
#[tokio::test]
async fn sqlite_child_forward_progress_after_rollover() {
    let db = open_sqlite().await;

    let parent_id = ExecutionId::new();
    db.create_execution(root_execution(&parent_id))
        .await
        .expect("create parent");

    // Give the parent 50 events so its event log is large.
    append_many_events(&db, &parent_id, 1, 50).await;

    // Build carried state with last_sequence = 50 — this is exactly the
    // value that Snapshot::from_materialized would have copied into at_sequence,
    // causing child seq 1/2/3 to be silently dropped.
    let carried = MaterializedState {
        current_state: json!({ "counter": 0 }),
        status: WorkflowStatus::Running,
        completed_nodes: HashMap::new(),
        active_nodes: HashSet::new(),
        last_sequence: 50,
    };

    let seg1_id = start_next_segment(
        &db, &parent_id, &carried, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("start_next_segment");

    // Drain the start-node work item.
    db.claim_work_item("cleanup-fwd", &["general"]).await.ok();

    // Drive a state mutation on the child at seq 3 (after WorkflowStarted=1,
    // NodeScheduled=2).  state_patch sets counter to 100.
    db.append_event(Event::new(
        seg1_id.clone(),
        3,
        EventKind::NodeCompleted {
            node_id: "step_1".into(),
            output: json!({ "result": "ok" }),
            state_patch: json!({ "counter": 100 }),
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
    ))
    .await
    .expect("append NodeCompleted on child");

    // Before fix: returns {"counter": 0} (frozen — at_sequence=50 dropped seq 1/2/3).
    // After fix:  returns {"counter": 100} (at_sequence=0 folds all child events).
    let mat = materialize(&db, &seg1_id).await.expect("materialize child");

    assert_eq!(
        mat.current_state["counter"], 100,
        "child state must reflect the post-rollover NodeCompleted commit on SQLite; \
         got {:?} — value 0 means the frozen-state bug is still present",
        mat.current_state
    );
}

// ── C1 regression: pre-boundary crash cannot fork child segment ───────────────

/// Regression test for the "crash double-roll" / "state fork" bug.
///
/// BUG (before fix): `new_id = ExecutionId::new()` was random on each call.
/// A crash between `start_next_segment` and the `SegmentBoundary` append caused
/// the worker to reclaim the parent item, pass the double-roll guard (no
/// SegmentBoundary yet), and call `start_next_segment` again — creating a SECOND
/// child with a different random id.  Two independent children each held a copy
/// of the carried state, forking the segment chain.
///
/// FIX: the child id is now deterministic (UUID v5 of parent+segment_number) and
/// `start_next_segment` is idempotent: a second call with the same arguments
/// detects the existing child and returns early without re-seeding or re-enqueuing.
///
/// This test simulates the crash window by calling `start_next_segment` twice
/// with identical arguments and asserting:
/// - Both calls return the SAME child `ExecutionId`.
/// - Exactly ONE work item is enqueued (the second call was a no-op).
#[tokio::test]
async fn sqlite_idempotent_start_next_segment_prevents_double_roll() {
    let db = open_sqlite().await;

    let parent_id = ExecutionId::new();
    db.create_execution(root_execution(&parent_id))
        .await
        .expect("create parent");

    let carried = carried_materialized_state();

    // First invocation: creates the child execution + seed + 2 events + work item.
    let id1 = start_next_segment(
        &db, &parent_id, &carried, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("first start_next_segment");

    // Second invocation: simulates re-run after a crash before SegmentBoundary.
    // Must return the SAME id and must NOT enqueue a duplicate work item.
    let id2 = start_next_segment(
        &db, &parent_id, &carried, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("second start_next_segment (idempotent re-run)");

    assert_eq!(
        id1, id2,
        "second call must return the same child ExecutionId as the first (deterministic id)"
    );

    // Claim the one expected work item.
    let item = db
        .claim_work_item("dedup-check-1", &["general"])
        .await
        .expect("claim_work_item")
        .expect("exactly one work item must exist");
    assert_eq!(item.execution_id, id1, "work item must belong to the child");

    // The queue must now be empty — no second work item was enqueued.
    let second = db
        .claim_work_item("dedup-check-2", &["general"])
        .await
        .expect("claim second");
    assert!(
        second.is_none(),
        "queue must be empty after draining the single work item; \
         a second item means the idempotency guard did not prevent re-enqueue"
    );
}

// ── 2g partial-creation gap fix: all-artifacts regression guard ───────────────

/// Generic helper: after `start_next_segment`, ALL 5 artifacts (execution, snapshot,
/// WorkflowStarted seq1, NodeScheduled seq2, claimable work item) must exist.
/// This is the regression guard that proves the partial-creation gap is closed.
async fn assert_all_artifacts_present(backend: &dyn StateBackend) {
    let parent_id = ExecutionId::new();
    backend
        .create_execution(root_execution(&parent_id))
        .await
        .expect("create parent");
    let carried = carried_materialized_state();

    let new_id = start_next_segment(
        backend, &parent_id, &carried, "seg-wf", "1.0.0", 1, "start", "general", "default",
    )
    .await
    .expect("start_next_segment");

    // 1. Execution row exists.
    assert!(
        backend
            .get_execution(&new_id)
            .await
            .expect("get_execution")
            .is_some(),
        "child execution row must exist"
    );

    // 2. Seed snapshot exists.
    assert!(
        backend
            .latest_snapshot(&new_id)
            .await
            .expect("latest_snapshot")
            .is_some(),
        "seed snapshot must exist"
    );

    // 3. Both seed events exist with correct sequences.
    let events = backend.get_events(&new_id).await.expect("get_events");
    assert_eq!(events.len(), 2, "must have exactly 2 seed events");
    assert!(
        matches!(events[0].kind, EventKind::WorkflowStarted { .. }),
        "event[0] must be WorkflowStarted"
    );
    assert_eq!(events[0].sequence, 1, "WorkflowStarted must be seq 1");
    assert!(
        matches!(events[1].kind, EventKind::NodeScheduled { .. }),
        "event[1] must be NodeScheduled"
    );
    assert_eq!(events[1].sequence, 2, "NodeScheduled must be seq 2");

    // 4. A CLAIMABLE work item exists. This is the key regression guard:
    //    before the fix a partial-created child had no work item, so this
    //    claim would return None and the continuation would be stuck forever.
    let item = backend
        .claim_work_item("regression-guard-worker", &["general"])
        .await
        .expect("claim_work_item")
        .expect("a work item MUST be claimable: partial-creation gap would leave child stuck");
    assert_eq!(
        item.execution_id, new_id,
        "work item must belong to the new segment"
    );
    assert_eq!(
        item.node_id, "start",
        "work item must target the start node"
    );
}

/// Regression guard — SQLite: child always has a claimable work item (partial-creation
/// gap is closed).
#[tokio::test]
async fn sqlite_all_artifacts_present_after_start_next_segment() {
    let db = open_sqlite().await;
    assert_all_artifacts_present(&db).await;
}

/// Regression guard — in-memory: child always has a claimable work item.
#[tokio::test]
async fn memory_all_artifacts_present_after_start_next_segment() {
    let backend = InMemoryBackend::new();
    assert_all_artifacts_present(&backend).await;
}

/// Regression guard — tenant-scoped: child always has a claimable work item.
#[tokio::test]
async fn tenant_scoped_all_artifacts_present_after_start_next_segment() {
    use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};

    let db = open_sqlite().await;
    let tenant_id = TenantId::from("artifacts-acme");
    let scoped_admin = db.for_tenant(TenantId::default());
    let now = Utc::now();
    scoped_admin
        .create_tenant(Tenant {
            id: tenant_id.clone(),
            name: "ArtifactsAcme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("register tenant");

    let scoped = db.for_tenant(tenant_id);
    assert_all_artifacts_present(&scoped).await;
}

/// Atomic rollback: if the work item INSERT fails (duplicate id conflict), the
/// execution row (and snapshot + events) must be rolled back — no partial child.
///
/// Calls `create_segment_atomic` directly so we can supply a known work_item id
/// that we pre-insert to trigger the constraint violation.
///
/// The in-memory backend is NOT tested for rollback here: its DashMap inserts
/// have no ACID rollback, so a failure mid-way leaves the execution row inserted
/// (by design — the in-memory backend has no crash-durability guarantee). The
/// all-artifacts tests + SQLite transaction code review cover atomicity for the
/// durable path.
#[tokio::test]
async fn sqlite_segment_atomic_rollback_on_work_item_conflict() {
    let db = open_sqlite().await;
    let parent_id = ExecutionId::new();
    db.create_execution(root_execution(&parent_id))
        .await
        .expect("create parent");

    // Pre-insert a work item with a known id to force a UNIQUE(id) conflict.
    let conflicting_id = Uuid::new_v4();
    db.enqueue_work_item(WorkItem {
        id: conflicting_id,
        execution_id: parent_id.clone(),
        node_id: "placeholder".into(),
        queue_type: "general".into(),
        payload: serde_json::json!({}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    })
    .await
    .expect("pre-insert conflicting work item");

    // Build the child segment artifacts — use the same conflicting_id for the work item.
    let child_id = ExecutionId::new();
    let carried = carried_materialized_state();

    let exec = WorkflowExecution {
        execution_id: child_id.clone(),
        workflow_id: "seg-wf".into(),
        workflow_version: "1.0.0".into(),
        status: WorkflowStatus::Running,
        initial_input: carried.current_state.clone(),
        current_state: carried.current_state.clone(),
        started_at: Utc::now(),
        updated_at: Utc::now(),
        completed_at: None,
        session_type: None,
        parent_execution_id: Some(parent_id.clone()),
        segment_number: 1,
    };
    let seed = Snapshot::seed_for_segment(child_id.clone(), &carried.current_state);
    let started = EventKind::WorkflowStarted {
        workflow_id: "seg-wf".into(),
        workflow_version: "1.0.0".into(),
        initial_input: carried.current_state.clone(),
    };
    let scheduled = EventKind::NodeScheduled {
        node_id: "start".into(),
        queue_type: "general".into(),
    };
    let wi = WorkItem {
        id: conflicting_id, // DUPLICATE — triggers UNIQUE(id) conflict at step 5
        execution_id: child_id.clone(),
        node_id: "start".into(),
        queue_type: "general".into(),
        payload: serde_json::json!({"workflow_id": "seg-wf", "workflow_version": "1.0.0"}),
        attempt: 0,
        max_attempts: 3,
        created_at: Utc::now(),
        lease_expires_at: None,
        worker_id: None,
        lease_fence: 0,
        tenant_id: "default".into(),
    };

    // create_segment_atomic must fail at the work item INSERT.
    let result = db
        .create_segment_atomic(exec, seed, started, scheduled, wi)
        .await;
    assert!(
        result.is_err(),
        "create_segment_atomic must fail on duplicate work_item id"
    );

    // KEY ASSERTION: execution row must NOT exist — the transaction rolled back.
    let fetched = db.get_execution(&child_id).await.expect("get_execution");
    assert!(
        fetched.is_none(),
        "execution row must be rolled back on atomic failure — partial child not left behind"
    );
}
