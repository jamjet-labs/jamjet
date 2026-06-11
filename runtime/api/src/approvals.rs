//! Validated approval submission, shared by the REST route and the MCP bridge.
//!
//! All approval decisions pass through `submit_approval` which folds the event log
//! to verify something is actually pending before appending `ApprovalReceived`.
//! The listing helper `approvals_view` builds the pending/decided payload for
//! `GET /executions/:id/approvals`.

use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_state::approvals::{node_approval_status, pending_approvals, NodeApprovalStatus};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::Event;
use std::sync::Arc;

pub struct ApprovalSubmission {
    pub node_id: Option<String>,
    pub user_id: String,
    pub decision: ApprovalDecision,
    pub comment: Option<String>,
    pub state_patch: Option<serde_json::Value>,
}

#[derive(Debug)]
pub enum SubmitError {
    /// Nothing is waiting for approval on this execution.
    NoPending,
    /// `node_id` omitted but several nodes are pending; caller must disambiguate.
    MultiplePending(Vec<String>),
    /// The named node has no outstanding approval request.
    NodeNotPending(String),
    Backend(String),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPending => write!(f, "no pending approval on this execution"),
            Self::MultiplePending(nodes) => write!(
                f,
                "multiple approvals pending ({}); specify node_id",
                nodes.join(", ")
            ),
            Self::NodeNotPending(n) => write!(
                f,
                "node '{n}' has no pending approval (unknown or already decided)"
            ),
            Self::Backend(e) => write!(f, "backend error: {e}"),
        }
    }
}

/// Validate against derived pending state, then append `ApprovalReceived`.
/// Returns the resolved `node_id`.
pub async fn submit_approval(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
    submission: ApprovalSubmission,
) -> Result<String, SubmitError> {
    let events = backend
        .get_events(execution_id)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;
    let pending = pending_approvals(&events);

    let node_id = match submission.node_id.filter(|n| !n.is_empty()) {
        Some(n) => {
            if !pending.iter().any(|p| p.node_id == n) {
                return Err(SubmitError::NodeNotPending(n));
            }
            n
        }
        None => match pending.as_slice() {
            [] => return Err(SubmitError::NoPending),
            [only] => only.node_id.clone(),
            many => {
                return Err(SubmitError::MultiplePending(
                    many.iter().map(|p| p.node_id.clone()).collect(),
                ))
            }
        },
    };

    let seq = backend
        .latest_sequence(execution_id)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?
        + 1;
    backend
        .append_event(Event::new(
            execution_id.clone(),
            seq,
            EventKind::ApprovalReceived {
                node_id: node_id.clone(),
                user_id: submission.user_id,
                decision: submission.decision,
                comment: submission.comment,
                state_patch: submission.state_patch,
            },
        ))
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;

    // Preserve the legacy paused -> running flip.
    if let Ok(Some(exec)) = backend.get_execution(execution_id).await {
        if exec.status == WorkflowStatus::Paused {
            backend
                .update_execution_status(execution_id, WorkflowStatus::Running)
                .await
                .map_err(|e| SubmitError::Backend(e.to_string()))?;
        }
    }

    Ok(node_id)
}

/// Pending + decided approval view for `GET /executions/:id/approvals`.
pub fn approvals_view(events: &[Event]) -> serde_json::Value {
    let pending = pending_approvals(events);

    // Collect every node that ever had a `ToolApprovalRequired`.
    let mut nodes: Vec<String> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolApprovalRequired { node_id, .. } => Some(node_id.clone()),
            _ => None,
        })
        .collect();
    nodes.sort();
    nodes.dedup();

    let decided: Vec<serde_json::Value> = nodes
        .iter()
        .filter(|n| !pending.iter().any(|p| &p.node_id == *n))
        .map(|n| match node_approval_status(events, n) {
            NodeApprovalStatus::Approved { user_id, sequence } => serde_json::json!({
                "node_id": n,
                "status": "approved",
                "user_id": user_id,
                "sequence": sequence,
            }),
            NodeApprovalStatus::Rejected {
                user_id,
                comment,
                sequence,
            } => serde_json::json!({
                "node_id": n,
                "status": "rejected",
                "user_id": user_id,
                "comment": comment,
                "sequence": sequence,
            }),
            _ => serde_json::json!({ "node_id": n, "status": "unknown" }),
        })
        .collect();

    serde_json::json!({ "pending": pending, "decided": decided })
}
