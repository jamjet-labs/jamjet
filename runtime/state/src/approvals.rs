//! Approval-state derivation.
//!
//! Folds an execution's event log into per-node approval status. This is the
//! single source of truth consumed by the worker (skip re-holding an approved
//! node), the API (validate decisions, list pending approvals), and the
//! scheduler tests. Events are the only persistent state: a node is pending
//! iff its latest `ToolApprovalRequired` has no later `ApprovalReceived`.

use crate::event::{ApprovalDecision, EventKind};
use crate::hashing::content_hash;
use crate::{Event, EventSequence};
use jamjet_core::workflow::ExecutionId;
use serde::Serialize;

/// A `ToolApprovalRequired` that has not been decided yet.
#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
    pub node_id: String,
    pub tool_name: String,
    pub approver: String,
    pub context: serde_json::Value,
    pub sequence: EventSequence,
}

impl PendingApproval {
    /// The `action_digest` the request was made for, if it carries one.
    pub fn action_digest(&self) -> Option<&str> {
        self.context.get(ACTION_DIGEST_KEY).and_then(|d| d.as_str())
    }
}

/// Approval status of a single node, derived from the event log.
#[derive(Debug, Clone)]
pub enum NodeApprovalStatus {
    NotRequested,
    Pending(PendingApproval),
    Approved {
        user_id: String,
        sequence: EventSequence,
        /// The action digest the approval authorises; `None` for a legacy
        /// approval or one answering a request with no digest.
        approved_digest: Option<String>,
    },
    Rejected {
        user_id: String,
        comment: Option<String>,
        sequence: EventSequence,
    },
}

/// Fold events (must be in sequence order) into the status of `node_id`.
pub fn node_approval_status(events: &[Event], node_id: &str) -> NodeApprovalStatus {
    let mut status = NodeApprovalStatus::NotRequested;
    for event in events {
        match &event.kind {
            EventKind::ToolApprovalRequired {
                node_id: n,
                tool_name,
                approver,
                context,
            } if n == node_id => {
                status = NodeApprovalStatus::Pending(PendingApproval {
                    node_id: n.clone(),
                    tool_name: tool_name.clone(),
                    approver: approver.clone(),
                    context: context.clone(),
                    sequence: event.sequence,
                });
            }
            EventKind::ApprovalReceived {
                node_id: n,
                user_id,
                decision,
                comment,
                approved_digest,
                ..
            } if n == node_id => {
                // A decision only resolves an outstanding request.
                if matches!(status, NodeApprovalStatus::Pending(_)) {
                    status = match decision {
                        ApprovalDecision::Approved => NodeApprovalStatus::Approved {
                            user_id: user_id.clone(),
                            sequence: event.sequence,
                            approved_digest: approved_digest.clone(),
                        },
                        ApprovalDecision::Rejected => NodeApprovalStatus::Rejected {
                            user_id: user_id.clone(),
                            comment: comment.clone(),
                            sequence: event.sequence,
                        },
                    };
                }
            }
            _ => {}
        }
    }
    status
}

/// Context key under which a hold records the digest of the action it parks.
pub const ACTION_DIGEST_KEY: &str = "action_digest";

/// Context key under which a hold records which occurrence of the node it is
/// for: the node's completion count when the request was made.
pub const ACTION_STEP_KEY: &str = "action_step";

/// The request the node's current decision answered: its latest
/// `ToolApprovalRequired`, if the node has a decision. `None` while the latest
/// request is still pending or there has never been one.
pub fn answered_request(events: &[Event], node_id: &str) -> Option<PendingApproval> {
    match node_approval_status(events, node_id) {
        NodeApprovalStatus::Approved { .. } | NodeApprovalStatus::Rejected { .. } => {}
        _ => return None,
    }
    events.iter().rev().find_map(|e| match &e.kind {
        EventKind::ToolApprovalRequired {
            node_id: n,
            tool_name,
            approver,
            context,
        } if n == node_id => Some(PendingApproval {
            node_id: n.clone(),
            tool_name: tool_name.clone(),
            approver: approver.clone(),
            context: context.clone(),
            sequence: e.sequence,
        }),
        _ => None,
    })
}

/// The digest an approval of a plain node is bound to.
///
/// Covers the run, the node, which occurrence of the node this is (`step`, the
/// number of times it has already completed), the node's DEFINITION (see
/// [`definition_hash`]) and the full materialized state the node would fire
/// on. So an approval stops covering the action when the state changes after
/// the request (a sibling commits, an approver's patch lands somewhere else),
/// when the node is visited again (a loop or any re-enqueue of a completed
/// node): `step` differs, so the old approval cannot be replayed, and when the
/// node's definition is not the one that was reviewed (a different module,
/// function, tool or policy under the same node id).
///
/// Domain-separated from the idempotency key (`hashing::idempotency_key`):
/// that key is persisted with a fixed shape and hashes the denormalised
/// `current_state` column, which neither in-process completions nor approval
/// patches update. This digest hashes the MATERIALIZED state (event fold), the
/// state an approval's `state_patch` actually changes.
pub fn action_digest(
    run: &str,
    step: u64,
    node: &str,
    definition: &str,
    state: &serde_json::Value,
) -> String {
    content_hash(&serde_json::json!({
        "domain": "jamjet.approval.action.v2",
        "run": run,
        "step": step,
        "node": node,
        "definition": definition,
        "state": content_hash(state),
    }))
}

/// Context key under which a hold records the [`definition_hash`] of the node
/// it parks, so a reviewer (and the approve route) can see what was bound.
pub const ACTION_DEFINITION_KEY: &str = "action_definition";

/// The hash of one node's definition as registered (the IR's `NodeDef`
/// serialized to JSON, its own policy included), in canonical JSON. Bound
/// into [`action_digest`] so an approval never covers a different node body.
pub fn definition_hash(node_def: &serde_json::Value) -> String {
    content_hash(&serde_json::json!({
        "domain": "jamjet.approval.node.v1",
        "node": node_def,
    }))
}

/// What a digest computation needs from the backend, read once, plus the
/// definition hash the caller supplies.
#[derive(Debug, Clone)]
pub struct ActionBasis {
    pub run: String,
    pub step: u64,
    pub definition: String,
    pub state: serde_json::Value,
}

impl ActionBasis {
    /// The digest of firing `node` on this basis.
    pub fn digest(&self, node: &str) -> String {
        action_digest(&self.run, self.step, node, &self.definition, &self.state)
    }
}

/// Read the [`ActionBasis`] for the next occurrence of `node_id`: the node's
/// completion count and the materialized state. `definition` is the node's
/// [`definition_hash`]. Errors propagate: a caller on the enforcement path
/// must treat them as "cannot prove this is approved".
pub async fn action_basis(
    backend: &dyn crate::backend::StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    definition: &str,
) -> crate::backend::BackendResult<ActionBasis> {
    let step = backend
        .count_node_completions(execution_id, node_id)
        .await?;
    let state = crate::materializer::materialize(backend, execution_id)
        .await?
        .current_state;
    Ok(ActionBasis {
        run: execution_id.to_string(),
        step,
        definition: definition.to_string(),
        state,
    })
}

/// [`action_digest`] for the next occurrence of `node_id`, read from the backend.
pub async fn derive_action_digest(
    backend: &dyn crate::backend::StateBackend,
    execution_id: &ExecutionId,
    node_id: &str,
    definition: &str,
) -> crate::backend::BackendResult<String> {
    Ok(action_basis(backend, execution_id, node_id, definition)
        .await?
        .digest(node_id))
}

/// Whether an approval status authorises firing an action whose digest is
/// `current`. Only an `Approved` whose `approved_digest` equals `current`
/// does; a missing digest (legacy approval) never does.
pub fn approval_covers(status: &NodeApprovalStatus, current: &str) -> bool {
    matches!(
        status,
        NodeApprovalStatus::Approved { approved_digest: Some(d), .. } if d == current
    )
}

/// The principal that started this run: the FIRST `WorkflowStarted`'s
/// `initiated_by`. `None` for a dev-mode run, a run logged before initiators
/// were recorded, or a log with no `WorkflowStarted`.
pub fn run_initiator(events: &[Event]) -> Option<String> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::WorkflowStarted { initiated_by, .. } => Some(initiated_by.clone()),
        _ => None,
    })?
}

/// All nodes whose latest `ToolApprovalRequired` is undecided.
pub fn pending_approvals(events: &[Event]) -> Vec<PendingApproval> {
    use std::collections::BTreeMap;
    let mut by_node: BTreeMap<String, Option<PendingApproval>> = BTreeMap::new();
    for event in events {
        match &event.kind {
            EventKind::ToolApprovalRequired {
                node_id,
                tool_name,
                approver,
                context,
            } => {
                by_node.insert(
                    node_id.clone(),
                    Some(PendingApproval {
                        node_id: node_id.clone(),
                        tool_name: tool_name.clone(),
                        approver: approver.clone(),
                        context: context.clone(),
                        sequence: event.sequence,
                    }),
                );
            }
            EventKind::ApprovalReceived { node_id, .. } => {
                // Intentional asymmetry: node_approval_status returns NotRequested for a
                // decision with no prior request; here we simply leave the event with no
                // effect. Both paths ignore orphan decisions by design.
                if let Some(slot) = by_node.get_mut(node_id) {
                    *slot = None;
                }
            }
            _ => {}
        }
    }
    by_node.into_values().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ApprovalDecision, EventKind};
    use crate::Event;
    use jamjet_core::workflow::ExecutionId;

    fn ev(seq: u64, kind: EventKind) -> Event {
        Event::new(ExecutionId::new(), seq as EventSequence, kind)
    }
    fn required(seq: u64, node: &str) -> Event {
        ev(
            seq,
            EventKind::ToolApprovalRequired {
                node_id: node.into(),
                tool_name: format!("tool_{node}"),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
        )
    }
    fn received(seq: u64, node: &str, decision: ApprovalDecision) -> Event {
        ev(
            seq,
            EventKind::ApprovalReceived {
                node_id: node.into(),
                user_id: "tester".into(),
                decision,
                comment: Some("note".into()),
                state_patch: None,
                principal: None,
                approved_digest: None,
                request_sequence: None,
            },
        )
    }

    #[test]
    fn no_events_means_not_requested() {
        assert!(matches!(
            node_approval_status(&[], "a"),
            NodeApprovalStatus::NotRequested
        ));
        assert!(pending_approvals(&[]).is_empty());
    }

    #[test]
    fn required_without_decision_is_pending() {
        let events = vec![required(1, "a")];
        match node_approval_status(&events, "a") {
            NodeApprovalStatus::Pending(p) => {
                assert_eq!(p.node_id, "a");
                assert_eq!(p.tool_name, "tool_a");
                assert_eq!(p.sequence, 1);
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        assert_eq!(pending_approvals(&events).len(), 1);
    }

    #[test]
    fn approved_resolves_pending() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Approved),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Approved { .. }
        ));
        assert!(pending_approvals(&events).is_empty());
    }

    #[test]
    fn rejected_resolves_pending_with_comment() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Rejected),
        ];
        match node_approval_status(&events, "a") {
            NodeApprovalStatus::Rejected {
                user_id, comment, ..
            } => {
                assert_eq!(user_id, "tester");
                assert_eq!(comment.as_deref(), Some("note"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn new_request_after_approval_is_pending_again() {
        let events = vec![
            required(1, "a"),
            received(2, "a", ApprovalDecision::Approved),
            required(3, "a"),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Pending(_)
        ));
    }

    #[test]
    fn decision_without_request_is_ignored() {
        let events = vec![received(1, "a", ApprovalDecision::Approved)];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::NotRequested
        ));
    }

    #[test]
    fn pending_approvals_order_is_deterministic() {
        // Insert "b" before "a" to verify BTreeMap lexicographic ordering, not insertion order.
        let events = vec![required(1, "b"), required(2, "a")];
        let pending = pending_approvals(&events);
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].node_id, "a");
        assert_eq!(pending[1].node_id, "b");
    }

    #[test]
    fn per_node_isolation() {
        let events = vec![
            required(1, "a"),
            required(2, "b"),
            received(3, "a", ApprovalDecision::Approved),
        ];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Approved { .. }
        ));
        assert!(matches!(
            node_approval_status(&events, "b"),
            NodeApprovalStatus::Pending(_)
        ));
        let pending = pending_approvals(&events);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].node_id, "b");
    }
}
