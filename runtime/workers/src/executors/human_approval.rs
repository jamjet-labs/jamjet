//! Executor for `human_approval` workflow nodes.
//!
//! The node never reaches this executor unapproved. Every work item passes
//! `node_guard::guard_item` first, and the guard treats a `human_approval`
//! node as gated whatever the policy chain says: the first time it is
//! claimed the guard records a `ToolApprovalRequired` (bound to the action
//! digest, with the node's `timeout_secs` as a deadline next to any the
//! policy chain sets) and settles the item as held. The run waits there.
//!
//! - Approved: the scheduler dispatches the node again, the guard finds the
//!   approval covering it, and this executor completes the node with the
//!   decision as its output.
//! - Rejected: the scheduler fails the node (and so the run), as for any
//!   rejected approval.
//! - Expired: with a `fallback_node`, the scheduler completes the node routed
//!   to the fallback; without one, the node fails like a rejection by
//!   `system:deadline`.
//!
//! This executor reads the decision from the run's log and fails closed if
//! it finds none: it never completes a node nobody approved.

use crate::executor::{ExecutionResult, ExecutorError, NodeExecutor};
use async_trait::async_trait;
use jamjet_state::approvals::{answered_request, node_approval_status, NodeApprovalStatus};
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use serde_json::json;
use std::sync::Arc;

/// Executor for `human_approval` nodes. Reads the run's log for the decision.
pub struct HumanApprovalExecutor {
    backend: Arc<dyn StateBackend>,
}

impl HumanApprovalExecutor {
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl NodeExecutor for HumanApprovalExecutor {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        let start = std::time::Instant::now();
        let events = self
            .backend
            .get_events(&item.execution_id)
            .await
            .map_err(|e| {
                ExecutorError::Fatal(format!(
                    "human_approval: the run's log is unreadable, so the decision cannot be \
                     read: {e}"
                ))
            })?;
        let NodeApprovalStatus::Approved {
            user_id, sequence, ..
        } = node_approval_status(&events, &item.node_id)
        else {
            return Err(ExecutorError::Fatal(format!(
                "human_approval node '{}' has no approval on record; refusing to complete it",
                item.node_id
            )));
        };
        let (principal, comment) = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ApprovalReceived {
                    principal, comment, ..
                } if e.sequence == sequence => Some((principal.clone(), comment.clone())),
                _ => None,
            })
            .unwrap_or((None, None));
        let request_sequence = answered_request(&events, &item.node_id).map(|r| r.sequence);
        Ok(ExecutionResult {
            output: json!({
                "decision": "approved",
                "approved_by": user_id,
                "principal": principal,
                "comment": comment,
                "request_sequence": request_sequence,
                "decision_sequence": sequence,
            }),
            state_patch: json!({}),
            duration_ms: start.elapsed().as_millis() as u64,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
        })
    }
}

/// Executor for `limit_exceeded` nodes, which the strategy compilers emit as
/// the target of a limit guard. Reaching one means a strategy limit was hit:
/// the node fails, so the run stops and reports failure. It never completes,
/// which would report a run cut short by its limit as a success.
#[derive(Debug, Default)]
pub struct LimitExceededExecutor;

#[async_trait]
impl NodeExecutor for LimitExceededExecutor {
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        Err(ExecutorError::Fatal(format!(
            "strategy limit reached at node '{}'; the run stops here",
            item.node_id
        )))
    }
}
