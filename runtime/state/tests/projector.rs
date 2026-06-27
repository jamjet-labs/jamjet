//! TDD tests for the async read-model projector (Task 2h-1).
//!
//! Covers:
//! (a) apply_approval_projection then read-back: row present, checkpoint advanced.
//! (b) Re-apply for the same (exec, node) with different status: UPSERT leaves one
//!     row, checkpoint advances.
//! (c) get_projector_checkpoint for an unseen execution returns 0.
//!
//! Run on SQLite, in-memory, and tenant-scoped backends.
//!
//! Written RED (trait stubs only, no impls) before the implementations existed;
//! turned GREEN after adding the three backend methods.

use jamjet_core::workflow::ExecutionId;
use jamjet_state::{backend::StateBackend, ApprovalProjectionRow, InMemoryBackend, SqliteBackend};

// ── Shared helpers ────────────────────────────────────────────────────────────

async fn open_sqlite() -> SqliteBackend {
    SqliteBackend::open("sqlite::memory:")
        .await
        .expect("failed to open in-memory SQLite for projector tests")
}

fn granted_row(execution_id: ExecutionId) -> ApprovalProjectionRow {
    ApprovalProjectionRow {
        execution_id,
        node_id: "nodeA".into(),
        status: "granted".into(),
        user_id: Some("alice".into()),
        comment: Some("looks good".into()),
        last_sequence: 5,
        tool_name: None,
        approver: None,
        context: None,
    }
}

// ── Generic test body (runs against any StateBackend) ────────────────────────

/// (a) apply then get_approval_projection returns the row;
///     get_projector_checkpoint returns the checkpoint.
async fn assert_apply_and_read(backend: &dyn StateBackend) {
    let exec = ExecutionId::new();
    let row = granted_row(exec.clone());

    backend
        .apply_approval_projection(row.clone(), "approvals", 5)
        .await
        .expect("apply_approval_projection must succeed");

    let rows = backend
        .get_approval_projection(&exec)
        .await
        .expect("get_approval_projection must succeed");
    assert_eq!(rows.len(), 1, "must have exactly one projected row");
    let r = &rows[0];
    assert_eq!(r.node_id, "nodeA");
    assert_eq!(r.status, "granted");
    assert_eq!(r.user_id.as_deref(), Some("alice"));
    assert_eq!(r.comment.as_deref(), Some("looks good"));
    assert_eq!(r.last_sequence, 5);

    let cp = backend
        .get_projector_checkpoint("approvals", &exec)
        .await
        .expect("get_projector_checkpoint must succeed");
    assert_eq!(cp, 5, "checkpoint must be 5 after first apply");
}

/// (b) Re-apply for same (exec, nodeA) with updated status/checkpoint:
///     still ONE row (UPSERT idempotency), status updated, checkpoint advanced.
async fn assert_upsert_idempotency(backend: &dyn StateBackend) {
    let exec = ExecutionId::new();

    // First apply: granted at seq 5.
    backend
        .apply_approval_projection(granted_row(exec.clone()), "approvals", 5)
        .await
        .expect("first apply");

    // Re-apply same (exec, nodeA) with status="denied" at checkpoint 9.
    let updated = ApprovalProjectionRow {
        execution_id: exec.clone(),
        node_id: "nodeA".into(),
        status: "denied".into(),
        user_id: Some("bob".into()),
        comment: None,
        last_sequence: 9,
        tool_name: None,
        approver: None,
        context: None,
    };
    backend
        .apply_approval_projection(updated, "approvals", 9)
        .await
        .expect("second apply (upsert)");

    let rows = backend
        .get_approval_projection(&exec)
        .await
        .expect("get_approval_projection");
    assert_eq!(rows.len(), 1, "UPSERT must leave exactly one row");
    let r = &rows[0];
    assert_eq!(r.status, "denied", "status must be updated to denied");
    assert_eq!(r.user_id.as_deref(), Some("bob"));
    assert!(r.comment.is_none(), "comment must be cleared");
    assert_eq!(r.last_sequence, 9, "last_sequence must be updated to 9");

    let cp = backend
        .get_projector_checkpoint("approvals", &exec)
        .await
        .expect("get_projector_checkpoint after second apply");
    assert_eq!(cp, 9, "checkpoint must advance to 9");
}

/// (c) get_projector_checkpoint for an unseen execution returns 0 (not an error).
async fn assert_unknown_checkpoint_is_zero(backend: &dyn StateBackend) {
    let unseen = ExecutionId::new();
    let cp = backend
        .get_projector_checkpoint("approvals", &unseen)
        .await
        .expect("get_projector_checkpoint must not error for unseen execution");
    assert_eq!(cp, 0, "unseen execution must yield checkpoint 0");
}

/// Two nodes in the same execution each get their own row.
async fn assert_two_nodes_same_execution(backend: &dyn StateBackend) {
    let exec = ExecutionId::new();

    let row_a = ApprovalProjectionRow {
        execution_id: exec.clone(),
        node_id: "nodeA".into(),
        status: "granted".into(),
        user_id: None,
        comment: None,
        last_sequence: 3,
        tool_name: None,
        approver: None,
        context: None,
    };
    let row_b = ApprovalProjectionRow {
        execution_id: exec.clone(),
        node_id: "nodeB".into(),
        status: "pending".into(),
        user_id: None,
        comment: None,
        last_sequence: 7,
        tool_name: None,
        approver: None,
        context: None,
    };

    backend
        .apply_approval_projection(row_a, "approvals", 3)
        .await
        .expect("apply nodeA");
    backend
        .apply_approval_projection(row_b, "approvals", 7)
        .await
        .expect("apply nodeB");

    let rows = backend
        .get_approval_projection(&exec)
        .await
        .expect("get_approval_projection");
    // Rows are ordered by node_id ascending.
    assert_eq!(rows.len(), 2, "must have two rows for two nodes");
    assert_eq!(rows[0].node_id, "nodeA");
    assert_eq!(rows[1].node_id, "nodeB");

    // Checkpoint reflects the last apply (nodeB at 7).
    let cp = backend
        .get_projector_checkpoint("approvals", &exec)
        .await
        .expect("checkpoint");
    assert_eq!(cp, 7);
}

// ── SQLite backend ────────────────────────────────────────────────────────────

#[tokio::test]
async fn sqlite_apply_and_read() {
    let db = open_sqlite().await;
    assert_apply_and_read(&db).await;
}

#[tokio::test]
async fn sqlite_upsert_idempotency() {
    let db = open_sqlite().await;
    assert_upsert_idempotency(&db).await;
}

#[tokio::test]
async fn sqlite_unknown_checkpoint_is_zero() {
    let db = open_sqlite().await;
    assert_unknown_checkpoint_is_zero(&db).await;
}

#[tokio::test]
async fn sqlite_two_nodes_same_execution() {
    let db = open_sqlite().await;
    assert_two_nodes_same_execution(&db).await;
}

// ── In-memory backend ─────────────────────────────────────────────────────────

#[tokio::test]
async fn memory_apply_and_read() {
    let backend = InMemoryBackend::new();
    assert_apply_and_read(&backend).await;
}

#[tokio::test]
async fn memory_upsert_idempotency() {
    let backend = InMemoryBackend::new();
    assert_upsert_idempotency(&backend).await;
}

#[tokio::test]
async fn memory_unknown_checkpoint_is_zero() {
    let backend = InMemoryBackend::new();
    assert_unknown_checkpoint_is_zero(&backend).await;
}

#[tokio::test]
async fn memory_two_nodes_same_execution() {
    let backend = InMemoryBackend::new();
    assert_two_nodes_same_execution(&backend).await;
}

// ── Tenant-scoped backend ─────────────────────────────────────────────────────

async fn open_tenant_scoped() -> jamjet_state::TenantScopedSqliteBackend {
    use chrono::Utc;
    use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};

    let db = open_sqlite().await;
    let tenant_id = TenantId::from("proj-acme");
    let scoped_admin = db.for_tenant(TenantId::default());
    let now = Utc::now();
    scoped_admin
        .create_tenant(Tenant {
            id: tenant_id.clone(),
            name: "ProjAcme".into(),
            status: TenantStatus::Active,
            policy: None,
            limits: None,
            created_at: now,
            updated_at: now,
        })
        .await
        .expect("register tenant");

    db.for_tenant(tenant_id)
}

#[tokio::test]
async fn tenant_apply_and_read() {
    let scoped = open_tenant_scoped().await;
    assert_apply_and_read(&scoped).await;
}

#[tokio::test]
async fn tenant_upsert_idempotency() {
    let scoped = open_tenant_scoped().await;
    assert_upsert_idempotency(&scoped).await;
}

#[tokio::test]
async fn tenant_unknown_checkpoint_is_zero() {
    let scoped = open_tenant_scoped().await;
    assert_unknown_checkpoint_is_zero(&scoped).await;
}

#[tokio::test]
async fn tenant_isolation() {
    // Two tenants with the same execution_id must not see each other's rows.
    use chrono::Utc;
    use jamjet_state::tenant::{Tenant, TenantId, TenantStatus};

    let db = open_sqlite().await;
    let admin = db.for_tenant(TenantId::default());
    let now = Utc::now();

    for name in ["alpha", "beta"] {
        admin
            .create_tenant(Tenant {
                id: TenantId::from(name),
                name: name.into(),
                status: TenantStatus::Active,
                policy: None,
                limits: None,
                created_at: now,
                updated_at: now,
            })
            .await
            .expect("register tenant");
    }

    let alpha = db.for_tenant(TenantId::from("alpha"));
    let beta = db.for_tenant(TenantId::from("beta"));

    // Use the SAME execution_id string in both tenants.
    let shared_exec = ExecutionId::new();

    alpha
        .apply_approval_projection(
            ApprovalProjectionRow {
                execution_id: shared_exec.clone(),
                node_id: "nodeX".into(),
                status: "granted".into(),
                user_id: None,
                comment: None,
                last_sequence: 1,
                tool_name: None,
                approver: None,
                context: None,
            },
            "approvals",
            1,
        )
        .await
        .expect("alpha apply");

    // Beta must see no rows for this execution.
    let beta_rows = beta
        .get_approval_projection(&shared_exec)
        .await
        .expect("beta get");
    assert!(
        beta_rows.is_empty(),
        "tenant beta must not see tenant alpha's projection rows"
    );

    // Beta checkpoint must be 0 (unseen).
    let beta_cp = beta
        .get_projector_checkpoint("approvals", &shared_exec)
        .await
        .expect("beta checkpoint");
    assert_eq!(beta_cp, 0, "beta must not inherit alpha's checkpoint");
}
