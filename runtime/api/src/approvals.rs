//! Validated approval submission, shared by the REST route and the MCP bridge.
//!
//! All approval decisions pass through `submit_approval` which folds the event log
//! to verify something is actually pending before appending `ApprovalReceived`.
//! The listing helper `approvals_view` builds the pending/decided payload for
//! `GET /executions/:id/approvals`.

use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_state::approvals::{
    node_approval_status, pending_approvals, run_initiator, NodeApprovalStatus,
};
use jamjet_state::backend::StateBackend;
use jamjet_state::event::{ApprovalDecision, EventKind};
use jamjet_state::Event;
use std::sync::Arc;

pub struct ApprovalSubmission {
    pub node_id: Option<String>,
    /// Display name recorded as the approver.
    pub user_id: String,
    /// Authenticated identity (`"token:<id>"`), or `None` in dev mode. Callers
    /// derive it from the transport's auth, never from request content.
    pub principal: Option<String>,
    pub decision: ApprovalDecision,
    pub comment: Option<String>,
    pub state_patch: Option<serde_json::Value>,
}

/// Install-wide approval rules, read once at startup into `AppState`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalPolicy {
    /// Let the principal that started a run approve its actions. Off by
    /// default; `JAMJET_ALLOW_SELF_APPROVAL=1` turns it on for single-operator
    /// installs where there is nobody else to approve.
    pub allow_self_approval: bool,
}

impl ApprovalPolicy {
    /// Read `JAMJET_ALLOW_SELF_APPROVAL` (`1` or `true`). Anything else,
    /// including unset or unparseable, enforces separation of duties.
    pub fn from_env() -> Self {
        Self {
            allow_self_approval: std::env::var("JAMJET_ALLOW_SELF_APPROVAL")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }
}

#[derive(Debug)]
pub enum SubmitError {
    /// Nothing is waiting for approval on this execution.
    NoPending,
    /// `node_id` omitted but several nodes are pending; caller must disambiguate.
    MultiplePending(Vec<String>),
    /// The named node has no outstanding approval request.
    NodeNotPending(String),
    /// The execution is already terminal; a decision can no longer change it.
    ExecutionTerminal(String),
    /// The approving principal started this run (separation of duties).
    SelfApproval,
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
            Self::ExecutionTerminal(status) => write!(
                f,
                "execution is {status}; approvals can no longer be decided"
            ),
            Self::SelfApproval => write!(
                f,
                "separation of duties: the principal that started this run cannot approve it"
            ),
            Self::Backend(e) => write!(f, "backend error: {e}"),
        }
    }
}

/// Validate against derived pending state, then append `ApprovalReceived`.
///
/// Returns the resolved `node_id` and the `ApprovalReceived` event that was
/// appended, so callers can seal it into the signed, hash-chained audit log
/// (via `AuditEnricher::enrich_and_append`) after the durable event write.
///
/// Separation of duties: an `Approved` decision whose principal is the run's
/// initiator (`WorkflowStarted.initiated_by`) is refused with
/// [`SubmitError::SelfApproval`] and nothing is appended, unless
/// `policy.allow_self_approval`. Rejections are never refused on this ground.
/// Without a principal (dev mode) or without a recorded initiator (dev-mode
/// runs, runs logged before initiators were recorded) there is nothing to
/// compare and the check does not apply.
pub async fn submit_approval(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
    submission: ApprovalSubmission,
    policy: ApprovalPolicy,
) -> Result<(String, Event), SubmitError> {
    // Refuse decisions on terminal executions up front. The event-derived
    // pending list can still show an undecided request after a cancellation
    // (the fold intentionally ignores decisions for unheld nodes), so without
    // this check an approve on a cancelled run would 200 while changing nothing.
    let execution = backend
        .get_execution(execution_id)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;
    let was_paused = match &execution {
        Some(exec) => {
            if matches!(
                exec.status,
                WorkflowStatus::Completed
                    | WorkflowStatus::Failed
                    | WorkflowStatus::Cancelled
                    | WorkflowStatus::LimitExceeded
            ) {
                return Err(SubmitError::ExecutionTerminal(format!("{:?}", exec.status)));
            }
            exec.status == WorkflowStatus::Paused
        }
        None => false,
    };

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

    if matches!(submission.decision, ApprovalDecision::Approved) && !policy.allow_self_approval {
        if let Some(principal) = submission.principal.as_deref() {
            if run_initiator(&events).as_deref() == Some(principal) {
                return Err(SubmitError::SelfApproval);
            }
        }
    }

    let seq = backend
        .latest_sequence(execution_id)
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?
        + 1;
    // TOCTOU: a concurrent approve may have already appended a decision; the
    // scheduler fold's held.remove gate makes the second event a no-op.
    let event = Event::new(
        execution_id.clone(),
        seq,
        EventKind::ApprovalReceived {
            node_id: node_id.clone(),
            user_id: submission.user_id,
            decision: submission.decision,
            comment: submission.comment,
            state_patch: submission.state_patch,
            principal: submission.principal,
        },
    );
    backend
        .append_event(event.clone())
        .await
        .map_err(|e| SubmitError::Backend(e.to_string()))?;

    // Preserve the legacy paused -> running flip. Uses the pre-append status
    // read; errors propagate rather than silently skipping the flip.
    if was_paused {
        backend
            .update_execution_status(execution_id, WorkflowStatus::Running)
            .await
            .map_err(|e| SubmitError::Backend(e.to_string()))?;
    }

    Ok((node_id, event))
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
            // Unreachable in practice: a node filtered out of `pending` has a
            // decision by construction. Kept total so the view never panics.
            _ => serde_json::json!({ "node_id": n, "status": "unknown" }),
        })
        .collect();

    serde_json::json!({ "pending": pending, "decided": decided })
}
