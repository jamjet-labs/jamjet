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
    Event, EventKind, InMemoryBackend, ReserveOutcome, SqliteBackend, TenantId,
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
        parent_execution_id: None,
        segment_number: 0,
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

// ── Key-stability unit tests ─────────────────────────────────────────────────

/// The key formula is deterministic: the same (execution_id, node_id, step,
/// current_state) inputs produce the same key regardless of when or how many
/// times it is computed.
#[test]
fn key_stability_same_inputs_yields_same_key() {
    let exec_id = "exec-stable-abc";
    let node_id = "n1";
    let step: u64 = 0;
    let state = json!({});

    let input_hash = jamjet_state::content_hash(&state);
    let key_a = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": step,
        "node": node_id,
        "input": input_hash,
    }));
    let key_b = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": step,
        "node": node_id,
        "input": input_hash,
    }));

    assert_eq!(key_a, key_b, "same inputs must produce the same key");
    assert_eq!(key_a.len(), 64, "key must be a 64-char hex sha256");
    assert!(
        key_a.chars().all(|c| c.is_ascii_hexdigit()),
        "key must be lowercase hex"
    );
}

/// A second loop occurrence (step advances once per NodeCompleted) produces a
/// different key from the first occurrence. This ensures the cache cannot
/// collide across loop passes.
#[test]
fn key_differs_when_step_advances() {
    let exec_id = "exec-stable-abc";
    let node_id = "n1";
    let state = json!({});
    let input_hash = jamjet_state::content_hash(&state);

    let key_step0 = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": 0u64,
        "node": node_id,
        "input": input_hash,
    }));
    let key_step1 = jamjet_state::content_hash(&json!({
        "run": exec_id,
        "segment": 0,
        "step": 1u64,
        "node": node_id,
        "input": input_hash,
    }));

    assert_ne!(
        key_step0, key_step1,
        "step=0 and step=1 must yield different keys (second loop occurrence)"
    );
}

// ── Restart-survival test ─────────────────────────────────────────────────────

/// A tool_effect written to a file-backed SQLite database is readable after
/// the connection is closed and reopened — simulating a process restart.
#[tokio::test]
async fn sqlite_tool_effect_survives_reopen() {
    let mut path = std::env::temp_dir();
    let unique = uuid::Uuid::new_v4().to_string().replace('-', "");
    path.push(format!("jamjet_idem_test_{unique}.db"));
    let url = format!("sqlite://{}", path.display());

    // First "process": commit a NodeCompleted with an idempotency key.
    {
        let db = SqliteBackend::open(&url)
            .await
            .expect("failed to open db for write");

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
                output: json!({"answer": 99}),
                state_patch: json!({}),
                duration_ms: 1,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: Some("restart-key".into()),
            },
        );

        db.commit_turn(item.id, item.lease_fence, event, false)
            .await
            .expect("commit_turn must succeed");
        // db dropped here — connection closed.
    }

    // Second "process": reopen the same file and verify the effect survived.
    {
        let db2 = SqliteBackend::open(&url)
            .await
            .expect("failed to reopen db");

        let recorded = db2
            .get_tool_effect("restart-key")
            .await
            .expect("get_tool_effect must not error after reopen");
        assert!(
            recorded.is_some(),
            "tool_effect must survive process restart (db reopen)"
        );
        let val = recorded.unwrap();
        assert_eq!(
            val["output"],
            json!({"answer": 99}),
            "output must survive db reopen"
        );
    }

    let _ = std::fs::remove_file(&path);
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

// ── Reserve-before-fire (spec Move 4b) ───────────────────────────────────────

/// Two workers racing for one idempotency key: exactly one may fire.
///
/// `get_tool_effect` only answers "did this run to COMPLETION", so before the
/// reservation existed both workers read `None` and both ran the effect. That is
/// check-then-fire, and duplicate live items for one node are producible today
/// (retry crash window, approval-hold resurrection, public POST /work-items).
#[tokio::test]
async fn only_one_worker_can_claim_an_idempotency_key() {
    let db = open_test_db().await;
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    let first = db
        .reserve_tool_effect("k1", &eid, "n1", "worker-A", 1, ttl)
        .await
        .unwrap();
    assert_eq!(first, ReserveOutcome::Acquired);

    let second = db
        .reserve_tool_effect("k1", &eid, "n1", "worker-B", 2, ttl)
        .await
        .unwrap();
    match second {
        ReserveOutcome::Held { owner, .. } => assert_eq!(owner, "worker-A"),
        ReserveOutcome::Acquired => {
            panic!("two workers acquired the same key — the effect will fire twice")
        }
    }
}

/// The reservation is REENTRANT for its own holder.
///
/// It exists to exclude a SECOND worker, not to lock a worker out of its own
/// node. A retry is the same logical attempt: the item is re-claimed by the same
/// worker and runs again, and a node that fails WITHOUT recording a tool effect
/// leaves its reservation standing. Blocking the holder would stall every such
/// retry for the whole TTL.
///
/// This was originally written to assert the opposite, and the engine's own
/// continue-as-new retry tests caught it — they deadlocked against a reservation
/// their own worker held.
#[tokio::test]
async fn a_reservation_is_reentrant_for_its_holder() {
    let db = open_test_db().await;
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    assert_eq!(
        db.reserve_tool_effect("k2", &eid, "n1", "worker-A", 1, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired
    );
    assert_eq!(
        db.reserve_tool_effect("k2", &eid, "n1", "worker-A", 2, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired,
        "the holder must be able to retry its own node"
    );
    assert!(matches!(
        db.reserve_tool_effect("k2", &eid, "n1", "worker-B", 3, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Held { .. }
    ));
}

/// An EXPIRED reservation must be takeable, or a worker that dies mid-tool
/// leaves the key unrunnable forever — a worse failure than the double-fire the
/// reservation replaced.
#[tokio::test]
async fn an_expired_reservation_can_be_taken_over() {
    let db = open_test_db().await;
    let eid = ExecutionId::new();

    // A TTL of zero is already lapsed by the time the next call reads it.
    assert_eq!(
        db.reserve_tool_effect(
            "k3",
            &eid,
            "n1",
            "worker-dead",
            1,
            std::time::Duration::ZERO
        )
        .await
        .unwrap(),
        ReserveOutcome::Acquired
    );

    let taken = db
        .reserve_tool_effect(
            "k3",
            &eid,
            "n1",
            "worker-B",
            2,
            std::time::Duration::from_secs(300),
        )
        .await
        .unwrap();
    assert_eq!(
        taken,
        ReserveOutcome::Acquired,
        "a lapsed reservation must be reclaimable, or a dead worker wedges the key"
    );

    // And having taken it, B now holds it against everyone else.
    assert!(matches!(
        db.reserve_tool_effect(
            "k3",
            &eid,
            "n1",
            "worker-C",
            3,
            std::time::Duration::from_secs(300)
        )
        .await
        .unwrap(),
        ReserveOutcome::Held { .. }
    ));
}

/// Different keys never contend — the reservation is per idempotency key, not a
/// global lock on the node or the execution.
#[tokio::test]
async fn distinct_keys_do_not_contend() {
    let db = open_test_db().await;
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    for key in ["a", "b", "c"] {
        assert_eq!(
            db.reserve_tool_effect(key, &eid, "n1", "worker-A", 1, ttl)
                .await
                .unwrap(),
            ReserveOutcome::Acquired,
            "key {key} should not have contended"
        );
    }
}

/// The in-memory backend must reserve identically.
///
/// Memory-only coverage is how the SQLite-vs-memory divergence class hides (the
/// claim-side lease expiry existed only in SQLite for exactly that reason), and
/// the inverse is just as bad: a dev/test backend that hands the same key to two
/// workers makes every double-fire test pass while proving nothing.
#[tokio::test]
async fn the_in_memory_backend_reserves_identically() {
    let db = InMemoryBackend::new();
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    assert_eq!(
        db.reserve_tool_effect("k", &eid, "n1", "worker-A", 1, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired
    );
    match db
        .reserve_tool_effect("k", &eid, "n1", "worker-B", 2, ttl)
        .await
        .unwrap()
    {
        ReserveOutcome::Held { owner, .. } => assert_eq!(owner, "worker-A"),
        ReserveOutcome::Acquired => panic!("in-memory handed the same key to two workers"),
    }

    // Expiry behaves the same way.
    assert_eq!(
        db.reserve_tool_effect("gone", &eid, "n1", "dead", 1, std::time::Duration::ZERO)
            .await
            .unwrap(),
        ReserveOutcome::Acquired
    );
    assert_eq!(
        db.reserve_tool_effect("gone", &eid, "n1", "worker-B", 2, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired,
        "a lapsed in-memory reservation must also be reclaimable"
    );
}

/// Concurrency, not just sequence: 32 workers race for one key and exactly one
/// may win. A sequential test cannot distinguish a real atomic claim from a
/// get-then-insert that simply has not been raced yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn exactly_one_of_many_concurrent_claimants_wins() {
    let db = std::sync::Arc::new(open_test_db().await);
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    let mut handles = Vec::new();
    for i in 0..32 {
        let db = db.clone();
        let eid = eid.clone();
        handles.push(tokio::spawn(async move {
            matches!(
                db.reserve_tool_effect("hot", &eid, "n1", &format!("worker-{i}"), i, ttl)
                    .await
                    .unwrap(),
                ReserveOutcome::Acquired
            )
        }));
    }

    let mut winners = 0;
    for h in handles {
        if h.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one worker may fire the effect; {winners} were told the key was theirs"
    );
}

/// A released reservation is immediately available to another worker.
///
/// Parking or failing ends an attempt WITHOUT recording an effect, so the
/// reservation must not outlive it. It is reentrant for its holder and lapses on
/// its TTL either way, but until then a DIFFERENT worker picking the node up
/// would wait out the remaining TTL for a key nobody is working on.
#[tokio::test]
async fn releasing_a_reservation_frees_it_immediately() {
    let db = open_test_db().await;
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    assert_eq!(
        db.reserve_tool_effect("rel", &eid, "n1", "worker-A", 1, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired
    );
    assert!(matches!(
        db.reserve_tool_effect("rel", &eid, "n1", "worker-B", 2, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Held { .. }
    ));

    db.release_tool_reservation("rel", "worker-A", 1)
        .await
        .unwrap();

    assert_eq!(
        db.reserve_tool_effect("rel", &eid, "n1", "worker-B", 2, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired,
        "a released key must be takeable at once, not after the TTL"
    );
}

/// Only the holder may release. A worker whose lease was stolen must not be able
/// to free the key for the worker that took over — that would hand a live
/// reservation to a third party.
#[tokio::test]
async fn a_non_holder_cannot_release_someone_elses_reservation() {
    let db = open_test_db().await;
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    db.reserve_tool_effect("guarded", &eid, "n1", "worker-A", 1, ttl)
        .await
        .unwrap();

    // A stale worker tries to free it.
    db.release_tool_reservation("guarded", "worker-stale", 1)
        .await
        .unwrap();

    assert!(
        matches!(
            db.reserve_tool_effect("guarded", &eid, "n1", "worker-C", 3, ttl)
                .await
                .unwrap(),
            ReserveOutcome::Held { .. }
        ),
        "a non-holder's release must be a no-op — A still owns this key"
    );
}

/// The in-memory backend must release identically, or every worker test runs
/// against different semantics from production.
#[tokio::test]
async fn the_in_memory_backend_releases_identically() {
    let db = InMemoryBackend::new();
    let eid = ExecutionId::new();
    let ttl = std::time::Duration::from_secs(300);

    db.reserve_tool_effect("m", &eid, "n1", "worker-A", 1, ttl)
        .await
        .unwrap();
    db.release_tool_reservation("m", "worker-stale", 1)
        .await
        .unwrap();
    assert!(
        matches!(
            db.reserve_tool_effect("m", &eid, "n1", "worker-B", 2, ttl)
                .await
                .unwrap(),
            ReserveOutcome::Held { .. }
        ),
        "a non-holder's release must be a no-op in memory too"
    );

    db.release_tool_reservation("m", "worker-A", 1)
        .await
        .unwrap();
    assert_eq!(
        db.reserve_tool_effect("m", &eid, "n1", "worker-B", 2, ttl)
            .await
            .unwrap(),
        ReserveOutcome::Acquired
    );
}
