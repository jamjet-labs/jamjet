//! Tests for the `tool_effects` idempotency cache.
//!
//! `commit_turn` records a `tool_effects` row atomically when the terminal
//! event is `NodeCompleted` with `idempotency_key = Some(_)`.
//! `get_tool_effect(key)` returns the recorded result JSON or `None`.
//!
//! TDD: test was written RED (before the field and trait method existed),
//! turned GREEN after adding `idempotency_key` to `NodeCompleted` and
//! implementing `get_tool_effect` + the commit_turn recording.

use chrono::Utc;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::tenant::{Tenant, TenantStatus};
use jamjet_state::{
    backend::{StateBackend, WorkItem},
    Event, EventKind, InMemoryBackend, SqliteBackend, TenantId,
};
use serde_json::json;
use uuid::Uuid;

// ── Shared fixtures ──────────────────────────────────────────────────────────

async fn open_test_db() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite")
}

/// Register a tenant so FK constraints in workflow_executions are satisfied.
async fn register_tenant(db: &SqliteBackend, id: &str) {
    let now = Utc::now();
    // The "default" tenant is pre-seeded by migrations; only register non-default ones.
    if id == "default" {
        return;
    }
    db.for_tenant(TenantId::default())
        .create_tenant(Tenant {
            id: TenantId::from(id),
            name: id.to_string(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("failed to register tenant");
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

fn sample_item(execution_id: &ExecutionId, node_id: &str) -> WorkItem {
    WorkItem {
        id: Uuid::new_v4(),
        execution_id: execution_id.clone(),
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
    }
}

// ── SQLite backend tests ─────────────────────────────────────────────────────

/// `commit_turn` with `NodeCompleted { idempotency_key: Some("k1") }` records
/// a tool_effect; `get_tool_effect("k1")` returns the result JSON with the
/// expected output and state_patch; `get_tool_effect("nope")` returns None.
#[tokio::test]
async fn sqlite_tool_effect_round_trip() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    db.enqueue_work_item(sample_item(&exec_id, "n1"))
        .await
        .unwrap();

    let item = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimed");

    let event = Event::new(
        exec_id.clone(),
        0, // sequence assigned inside commit_turn
        EventKind::NodeCompleted {
            node_id: "n1".into(),
            output: json!({"result": "hello"}),
            state_patch: json!({"key": "val"}),
            duration_ms: 42,
            gen_ai_system: Some("anthropic".into()),
            gen_ai_model: Some("claude-3".into()),
            input_tokens: Some(10),
            output_tokens: Some(20),
            finish_reason: Some("stop".into()),
            cost_usd: None,
            provenance: None,
            idempotency_key: Some("k1".into()),
        },
    );

    db.commit_turn(item.id, item.lease_fence, event, false)
        .await
        .expect("commit_turn must succeed");

    // k1 must be recorded with the correct result JSON fields.
    let recorded = db
        .get_tool_effect("k1")
        .await
        .expect("get_tool_effect must not error");
    assert!(recorded.is_some(), "expected Some for key k1");
    let val = recorded.unwrap();
    assert_eq!(val["output"], json!({"result": "hello"}), "output mismatch");
    assert_eq!(
        val["state_patch"],
        json!({"key": "val"}),
        "state_patch mismatch"
    );
    assert_eq!(val["duration_ms"], json!(42u64), "duration_ms mismatch");
    assert_eq!(
        val["gen_ai_system"],
        json!("anthropic"),
        "gen_ai_system mismatch"
    );
    assert_eq!(
        val["gen_ai_model"],
        json!("claude-3"),
        "gen_ai_model mismatch"
    );
    assert_eq!(val["input_tokens"], json!(10u64), "input_tokens mismatch");
    assert_eq!(val["output_tokens"], json!(20u64), "output_tokens mismatch");
    assert_eq!(
        val["finish_reason"],
        json!("stop"),
        "finish_reason mismatch"
    );

    // Unknown key must return None.
    let none_val = db
        .get_tool_effect("nope")
        .await
        .expect("get_tool_effect must not error for unknown key");
    assert!(none_val.is_none(), "expected None for unknown key");
}

/// A NodeCompleted WITHOUT an idempotency_key records NO tool_effect row.
#[tokio::test]
async fn sqlite_no_key_no_effect_recorded() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    db.enqueue_work_item(sample_item(&exec_id, "n1"))
        .await
        .unwrap();

    let item = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimed");

    let event = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "n1".into(),
            output: json!("out"),
            state_patch: json!({}),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: None, // no key
        },
    );

    db.commit_turn(item.id, item.lease_fence, event, false)
        .await
        .expect("commit_turn must succeed without idempotency key");

    // Nothing recorded for any key.
    let none_val = db.get_tool_effect("anything").await.unwrap();
    assert!(
        none_val.is_none(),
        "no tool_effect should be recorded when idempotency_key is None"
    );
}

/// Concurrent winner: INSERT OR IGNORE means a second commit with the SAME key
/// keeps the first result intact (the second insert is silently dropped).
#[tokio::test]
async fn sqlite_concurrent_winner_insert_or_ignore() {
    let db = open_test_db().await;
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();

    // Enqueue two items for the same key.
    for node in ["n1", "n2"] {
        db.enqueue_work_item(sample_item(&exec_id, node))
            .await
            .unwrap();
    }

    let item1 = db
        .claim_work_item("w1", &["model"])
        .await
        .unwrap()
        .expect("item1 must be claimed");
    let item2 = db
        .claim_work_item("w2", &["model"])
        .await
        .unwrap()
        .expect("item2 must be claimed");

    // First commit records output "first".
    db.commit_turn(
        item1.id,
        item1.lease_fence,
        Event::new(
            exec_id.clone(),
            0,
            EventKind::NodeCompleted {
                node_id: item1.node_id.clone(),
                output: json!("first"),
                state_patch: json!({}),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: Some("shared-key".into()),
            },
        ),
        false,
    )
    .await
    .expect("first commit must succeed");

    // Second commit with same key: INSERT OR IGNORE keeps "first".
    db.commit_turn(
        item2.id,
        item2.lease_fence,
        Event::new(
            exec_id.clone(),
            0,
            EventKind::NodeCompleted {
                node_id: item2.node_id.clone(),
                output: json!("second"),
                state_patch: json!({}),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: Some("shared-key".into()),
            },
        ),
        false,
    )
    .await
    .expect("second commit with duplicate key must not error (INSERT OR IGNORE)");

    // The first result must be the one that survives.
    let val = db.get_tool_effect("shared-key").await.unwrap().unwrap();
    assert_eq!(
        val["output"],
        json!("first"),
        "INSERT OR IGNORE must keep the first result"
    );
}

// ── In-memory backend tests ──────────────────────────────────────────────────

/// Same round-trip test against the InMemoryBackend.
#[tokio::test]
async fn memory_tool_effect_round_trip() {
    let db = InMemoryBackend::new();
    let exec_id = ExecutionId::new();
    db.create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    db.enqueue_work_item(sample_item(&exec_id, "n1"))
        .await
        .unwrap();

    let item = db
        .claim_work_item("worker-1", &["model"])
        .await
        .unwrap()
        .expect("item must be claimed");

    let event = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "n1".into(),
            output: json!({"result": "mem-hello"}),
            state_patch: json!({"mk": "mv"}),
            duration_ms: 7,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: Some("mem-k1".into()),
        },
    );

    db.commit_turn(item.id, item.lease_fence, event, false)
        .await
        .expect("commit_turn must succeed");

    let recorded = db.get_tool_effect("mem-k1").await.unwrap();
    assert!(recorded.is_some(), "expected Some for key mem-k1");
    let val = recorded.unwrap();
    assert_eq!(val["output"], json!({"result": "mem-hello"}));
    assert_eq!(val["duration_ms"], json!(7u64));

    let none_val = db.get_tool_effect("no-such-key").await.unwrap();
    assert!(
        none_val.is_none(),
        "expected None for unknown key in InMemoryBackend"
    );
}

// ── Tenant-scoped SQLite tests ───────────────────────────────────────────────

/// TenantScopedSqliteBackend: tool_effect is scoped by tenant_id.
/// A lookup from a different tenant must return None.
#[tokio::test]
async fn tenant_scoped_tool_effect_round_trip() {
    let db = SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite");

    // Register both tenants so FK constraints in workflow_executions are satisfied.
    register_tenant(&db, "tenant-a").await;
    register_tenant(&db, "tenant-b").await;

    let tenant_a = TenantId("tenant-a".into());
    let tenant_b = TenantId("tenant-b".into());

    let backend_a = db.for_tenant(tenant_a.clone());
    let backend_b = db.for_tenant(tenant_b);

    let exec_id = ExecutionId::new();
    // Create execution as tenant-a (uses shared executions table with tenant_id).
    backend_a
        .create_execution(sample_execution(&exec_id))
        .await
        .unwrap();
    backend_a
        .enqueue_work_item(sample_item(&exec_id, "n1"))
        .await
        .unwrap();

    let item = backend_a
        .claim_work_item("w", &["model"])
        .await
        .unwrap()
        .expect("item must be claimed by tenant-a");

    let event = Event::new(
        exec_id.clone(),
        0,
        EventKind::NodeCompleted {
            node_id: "n1".into(),
            output: json!("tenant-out"),
            state_patch: json!({}),
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: Some("tenant-key".into()),
        },
    );

    backend_a
        .commit_turn(item.id, item.lease_fence, event, false)
        .await
        .expect("commit_turn must succeed for tenant-a");

    // tenant-a can read it.
    let recorded = backend_a.get_tool_effect("tenant-key").await.unwrap();
    assert!(recorded.is_some(), "tenant-a must see its own tool_effect");
    assert_eq!(recorded.unwrap()["output"], json!("tenant-out"));

    // tenant-b must NOT see tenant-a's tool_effect.
    let other = backend_b.get_tool_effect("tenant-key").await.unwrap();
    assert!(
        other.is_none(),
        "tenant-b must not see tenant-a's tool_effect"
    );
}
