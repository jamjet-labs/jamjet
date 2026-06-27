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
    backend::StateBackend, content_hash, materialize, start_next_segment, Event, EventKind,
    InMemoryBackend, MaterializedState, SqliteBackend,
};
use serde_json::json;
use std::collections::{HashMap, HashSet};

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
        backend, &parent_id, &carried, "seg-wf", "1.0.0", 1, "start", "general",
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
