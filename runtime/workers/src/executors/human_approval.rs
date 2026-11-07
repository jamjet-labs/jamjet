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
use jamjet_state::approvals::{
    answered_request, node_approval_status, NodeApprovalStatus, ACTION_STEP_KEY,
};
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
        // The approval must answer a request for THIS occurrence of the node
        // (the guard before this checked the same; this keeps the executor
        // closed on its own). A request records the node's completion count
        // when it was raised.
        let request = answered_request(&events, &item.node_id).ok_or_else(|| {
            ExecutorError::Fatal(format!(
                "human_approval node '{}' has an approval but no request it answers; \
                 refusing to complete it",
                item.node_id
            ))
        })?;
        let step = self
            .backend
            .count_node_completions(&item.execution_id, &item.node_id)
            .await
            .map_err(|e| {
                ExecutorError::Fatal(format!(
                    "human_approval: the node's completion count is unreadable: {e}"
                ))
            })?;
        if request
            .context
            .get(ACTION_STEP_KEY)
            .and_then(|v| v.as_u64())
            != Some(step)
        {
            return Err(ExecutorError::Fatal(format!(
                "human_approval node '{}': the approval on record is for another visit of \
                 the node; refusing to complete it",
                item.node_id
            )));
        }
        let request_sequence = Some(request.sequence);
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

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_core::workflow::ExecutionId;
    use jamjet_state::event::ApprovalDecision;
    use jamjet_state::{Event, InMemoryBackend};

    fn item(eid: &ExecutionId) -> WorkItem {
        WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: eid.clone(),
            node_id: "review".into(),
            queue_type: "general".into(),
            payload: json!({}),
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "default".into(),
        }
    }

    async fn append(backend: &InMemoryBackend, eid: &ExecutionId, kind: EventKind) {
        let seq = backend.latest_sequence(eid).await.unwrap() + 1;
        backend
            .append_event(Event::new(eid.clone(), seq, kind))
            .await
            .unwrap();
    }

    /// A log where `review` was asked at occurrence 0 and bob approved.
    async fn approved_log() -> (Arc<InMemoryBackend>, ExecutionId) {
        let backend = Arc::new(InMemoryBackend::new());
        let eid = ExecutionId::new();
        append(
            &backend,
            &eid,
            EventKind::ToolApprovalRequired {
                node_id: "review".into(),
                tool_name: "review".into(),
                approver: "human".into(),
                context: json!({ "node_id": "review", "action_step": 0 }),
            },
        )
        .await;
        append(
            &backend,
            &eid,
            EventKind::ApprovalReceived {
                node_id: "review".into(),
                user_id: "bob".into(),
                decision: ApprovalDecision::Approved,
                comment: Some("ok".into()),
                state_patch: None,
                principal: Some("token:b".into()),
                approved_digest: None,
                request_sequence: Some(1),
            },
        )
        .await;
        (backend, eid)
    }

    #[tokio::test]
    async fn an_approval_of_this_occurrence_completes_with_the_decision() {
        let (backend, eid) = approved_log().await;
        let exec = HumanApprovalExecutor::new(backend.clone());
        let out = exec.execute(&item(&eid)).await.unwrap().output;
        assert_eq!(out["decision"], "approved");
        assert_eq!(out["approved_by"], "bob");
        assert_eq!(out["principal"], "token:b");
        assert_eq!(out["comment"], "ok");
    }

    /// The approval on record answered a request for an earlier visit of the
    /// node (it has completed since): the executor refuses on its own, not
    /// only because the guard before it would.
    #[tokio::test]
    async fn an_approval_of_an_earlier_visit_does_not_complete_the_node() {
        let (backend, eid) = approved_log().await;
        append(
            &backend,
            &eid,
            EventKind::NodeCompleted {
                node_id: "review".into(),
                output: json!({}),
                state_patch: json!({}),
                duration_ms: 0,
                gen_ai_system: None,
                gen_ai_model: None,
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
                cost_usd: None,
                provenance: None,
                idempotency_key: None,
            },
        )
        .await;
        let exec = HumanApprovalExecutor::new(backend.clone());
        match exec.execute(&item(&eid)).await {
            Err(ExecutorError::Fatal(msg)) => assert!(msg.contains("another visit"), "{msg}"),
            other => panic!("expected Fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_approval_on_record_does_not_complete_the_node() {
        let backend = Arc::new(InMemoryBackend::new());
        let eid = ExecutionId::new();
        let exec = HumanApprovalExecutor::new(backend.clone());
        assert!(matches!(
            exec.execute(&item(&eid)).await,
            Err(ExecutorError::Fatal(_))
        ));
    }
}
