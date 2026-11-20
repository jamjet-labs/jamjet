//! The async read-model projector's backend side.
//!
//! Covers:
//! (a) apply_approval_projection then read-back: row present, checkpoint advanced.
//! (b) Re-apply for the same (exec, node) with different status: UPSERT leaves one
//!     row, checkpoint advances.
//! (c) get_projector_checkpoint for an unseen execution returns 0.
//!
//! The scoped stores run these in a tenant other than the default. A scoped
//! store projects only its own tenant's runs, so each case starts its run
//! first.

use crate::conformance::cases::support::{in_tenant, tenant_view, without_tenant_views};
use crate::conformance::Fixture;
use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
use jamjet_state::{ApprovalProjectionRow, StateBackend};

/// A running run, created through `backend`.
async fn new_run(backend: &dyn StateBackend) -> ExecutionId {
    let id = ExecutionId::new();
    let now = chrono::Utc::now();
    backend
        .create_execution(WorkflowExecution {
            execution_id: id.clone(),
            workflow_id: "wf-projector".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        })
        .await
        .unwrap();
    id
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

/// (a) apply then get_approval_projection returns the row;
///     get_projector_checkpoint returns the checkpoint.
pub async fn apply_and_read(fx: &Fixture) {
    let backend = &*in_tenant(fx, "proj-acme").await;
    let exec = new_run(backend).await;
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
pub async fn upsert_idempotency(fx: &Fixture) {
    let backend = &*in_tenant(fx, "proj-acme").await;
    let exec = new_run(backend).await;

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
pub async fn unknown_checkpoint_is_zero(fx: &Fixture) {
    let backend = &*in_tenant(fx, "proj-acme").await;
    let unseen = ExecutionId::new();
    let cp = backend
        .get_projector_checkpoint("approvals", &unseen)
        .await
        .expect("get_projector_checkpoint must not error for unseen execution");
    assert_eq!(cp, 0, "unseen execution must yield checkpoint 0");
}

/// Two nodes in the same execution each get their own row.
pub async fn two_nodes_same_execution(fx: &Fixture) {
    let backend = &*in_tenant(fx, "proj-acme").await;
    let exec = new_run(backend).await;

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

pub async fn tenant_isolation(fx: &Fixture) {
    // Two tenants with the same execution_id must not see each other's rows.
    let (Some(alpha), Some(beta)) = (
        tenant_view(fx, "alpha").await,
        tenant_view(fx, "beta").await,
    ) else {
        return without_tenant_views(fx);
    };

    // Alpha's run, read through beta by the same execution_id.
    let shared_exec = new_run(&*alpha).await;

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
