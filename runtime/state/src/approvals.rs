//! Approval-state derivation.
//!
//! Folds an execution's event log into per-node approval status. This is the
//! single source of truth consumed by the worker (skip re-holding an approved
//! node), the API (validate decisions, list pending approvals), and the
//! scheduler tests. Events are the only persistent state: a node is pending
//! iff its latest `ToolApprovalRequired` has no later `ApprovalReceived`.

use crate::event::{ApprovalDecision, EventKind};
use crate::{Event, EventSequence};
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

/// Approval status of a single node, derived from the event log.
#[derive(Debug, Clone)]
pub enum NodeApprovalStatus {
    NotRequested,
    Pending(PendingApproval),
    Approved {
        user_id: String,
        sequence: EventSequence,
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
                ..
            } if n == node_id => {
                // A decision only resolves an outstanding request.
                if matches!(status, NodeApprovalStatus::Pending(_)) {
                    status = match decision {
                        ApprovalDecision::Approved => NodeApprovalStatus::Approved {
                            user_id: user_id.clone(),
                            sequence: event.sequence,
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
        let events = vec![required(1, "a"), received(2, "a", ApprovalDecision::Approved)];
        assert!(matches!(
            node_approval_status(&events, "a"),
            NodeApprovalStatus::Approved { .. }
        ));
        assert!(pending_approvals(&events).is_empty());
    }

    #[test]
    fn rejected_resolves_pending_with_comment() {
        let events = vec![required(1, "a"), received(2, "a", ApprovalDecision::Rejected)];
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
