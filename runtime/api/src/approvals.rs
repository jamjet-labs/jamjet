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
    /// Always refused when non-null: see [`SubmitError::EditsUnsupported`].
    /// Carried so both transports refuse it in one place rather than one of
    /// them dropping it silently.
    pub state_patch: Option<serde_json::Value>,
    /// The `action_digest` the decider was shown, echoed back. When present it
    /// must equal the pending request's digest, or the decision is refused as
    /// stale (the request it was made against has been superseded).
    pub action_digest: Option<String>,
    /// The transport runs without authentication (dev mode). Outside it, an
    /// `Approved` decision must echo the request's binding.
    pub dev_mode: bool,
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
    /// Outside dev mode, the run has no recorded initiator, so separation of
    /// duties cannot be proven for an approval.
    InitiatorUnknown,
    /// The echoed `action_digest` is not the pending request's digest.
    StaleDigest,
    /// Outside dev mode, an `Approved` decision on a request that binds an
    /// action did not echo the binding it was made against.
    DigestRequired,
    /// Outside dev mode, the decision carries no authenticated principal. Both
    /// transports always supply one there, so this is a misconfigured caller:
    /// an unattributable decision is never recorded.
    PrincipalMissing,
    /// A decision carried a `state_patch`. Approval edits are not supported:
    /// the executor builds node payloads from the scheduler's own state, which
    /// never applies an approval's patch, so recording "approved as edited"
    /// would be a false record of what ran.
    EditsUnsupported,
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
            Self::InitiatorUnknown => write!(
                f,
                "separation of duties: this run has no recorded initiator (it predates \
                 initiator recording or was started in dev mode), so an approval cannot be \
                 shown to come from someone other than whoever started it; reject it, re-run \
                 it, or set JAMJET_ALLOW_SELF_APPROVAL=1 to allow approvals without that check"
            ),
            Self::StaleDigest => write!(
                f,
                "action_digest does not match the pending request; re-read the pending approval and decide again"
            ),
            Self::DigestRequired => write!(
                f,
                "action_digest is required to approve: echo the action_digest (or, for an \
                 agent tool dispatch, the calls_hash) from the context of the pending \
                 approval you reviewed, as listed by GET /executions/{{id}}/approvals"
            ),
            Self::EditsUnsupported => write!(
                f,
                "approval edits are not supported: a decision cannot carry a state_patch, \
                 because the action would not run on the edited state; approve or reject \
                 the action as requested"
            ),
            Self::PrincipalMissing => write!(
                f,
                "the decision has no authenticated principal; outside dev mode every \
                 approval decision must come from an API token"
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
/// Outside dev mode, a run with no recorded initiator (dev-mode runs, runs
/// logged before initiators were recorded) is refused with
/// [`SubmitError::InitiatorUnknown`], since SoD cannot be proven for it. In
/// dev mode there is no principal and the check does not apply.
///
/// SoD distinguishes TOKENS, not people: see the note in the body.
pub async fn submit_approval(
    backend: &Arc<dyn StateBackend>,
    execution_id: &ExecutionId,
    submission: ApprovalSubmission,
    policy: ApprovalPolicy,
) -> Result<(String, Event), SubmitError> {
    // No edits, on either decision, before anything is read or written.
    if submission
        .state_patch
        .as_ref()
        .is_some_and(|p| !p.is_null())
    {
        return Err(SubmitError::EditsUnsupported);
    }

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

    // IDENTITY. Outside dev mode every decision is attributable. With no
    // principal, separation of duties has nothing to compare and would fall
    // through to "allow"; refuse instead, whatever the self-approval policy.
    if !submission.dev_mode && submission.principal.is_none() {
        return Err(SubmitError::PrincipalMissing);
    }

    // SEPARATION OF DUTIES is per TOKEN, not per person. Operators mint
    // tokens (there is no REST route for it), so one human holding two tokens,
    // or a token re-minted after revocation, can approve a run they started.
    // That is a deployment control: issue one token per human.
    if matches!(submission.decision, ApprovalDecision::Approved) && !policy.allow_self_approval {
        match (run_initiator(&events), submission.principal.as_deref()) {
            (Some(initiator), Some(principal)) if initiator == principal => {
                return Err(SubmitError::SelfApproval);
            }
            // No recorded initiator: the run predates initiator recording or
            // was started in dev mode. Outside dev mode SoD cannot be proven,
            // so refuse rather than treat the run as approvable by anyone,
            // including whoever started it.
            (None, _) if !submission.dev_mode => {
                return Err(SubmitError::InitiatorUnknown);
            }
            _ => {}
        }
    }

    // BINDING. The decision answers one specific request: the latest pending
    // `ToolApprovalRequired` for this node. `node_id` was validated against
    // `pending` above, so the lookup cannot miss; it is still handled, closed.
    let Some(request) = pending.iter().find(|p| p.node_id == node_id) else {
        return Err(SubmitError::NodeNotPending(node_id));
    };
    let request_digest = request.action_digest().map(str::to_owned);
    // What the request binds: its action digest, or for an agent tool dispatch
    // its `calls_hash` if it predates dispatch digests. A legacy request binds
    // nothing.
    let binding = request.binding();

    // Outside dev mode an approval must say which request it answers, so it
    // can never land on a request that replaced the one the reviewer saw.
    // Rejections need no echo: refusing to act is never the risk. A legacy
    // request binds nothing, so there is nothing to echo; approving it
    // authorises nothing by itself (the worker asks again with a digest).
    if !submission.dev_mode
        && matches!(submission.decision, ApprovalDecision::Approved)
        && binding.is_some()
        && submission.action_digest.is_none()
    {
        return Err(SubmitError::DigestRequired);
    }

    // A decider that echoes the binding it was shown is refused if the request
    // has since been superseded. A request without a binding can never match.
    if let Some(echoed) = submission.action_digest.as_deref() {
        if binding != Some(echoed) {
            return Err(SubmitError::StaleDigest);
        }
    }

    // What an approval authorises: exactly the requested action. If the state
    // has moved since the request, the worker finds a different digest at
    // resume and requests afresh. Requests with no digest (agent tool
    // dispatch requests from before dispatch digests, bound by `calls_hash`
    // only, and other legacy requests) record none, and authorise nothing by
    // themselves: the guard asks again for the bound action.
    let approved_digest = match (&submission.decision, request_digest) {
        (ApprovalDecision::Approved, Some(requested)) => Some(requested),
        _ => None,
    };
    let request_sequence = Some(request.sequence);

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
            // Refused above when non-null; never recorded.
            state_patch: None,
            principal: submission.principal,
            approved_digest,
            request_sequence,
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
            NodeApprovalStatus::Approved {
                user_id,
                sequence,
                approved_digest,
            } => serde_json::json!({
                "node_id": n,
                "status": "approved",
                "user_id": user_id,
                "sequence": sequence,
                "approved_digest": approved_digest,
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
