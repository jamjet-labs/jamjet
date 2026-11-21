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
//!   decision as its output, and writes the decision, the option chosen and
//!   the note into the run's state.
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
    APPROVALS_STATE_KEY,
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
        // The choice and the note come from the decision on record, never from
        // the work item: the approve route checked the choice against the
        // request's options before it recorded it. The note is stored as the
        // event's `comment`.
        let (principal, note, choice, decided_at) = events
            .iter()
            .find_map(|e| match &e.kind {
                EventKind::ApprovalReceived {
                    principal,
                    comment,
                    choice,
                    ..
                } if e.sequence == sequence => Some((
                    principal.clone(),
                    comment.clone(),
                    choice.clone(),
                    e.created_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                )),
                _ => None,
            })
            .ok_or_else(|| {
                ExecutorError::Fatal(format!(
                    "human_approval node '{}': the decision at sequence {sequence} is not in \
                     the run's log; refusing to complete it",
                    item.node_id
                ))
            })?;
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
        // The decision also goes into the run's state, under the engine's own
        // key (`state.__approvals.<node_id>.approver`), so later steps and
        // prompts can read it, and a condition can route on the option the
        // reviewer chose (`state.__approvals.<node_id>.choice`). Engine-owned:
        // no worker or run input can write that key
        // (`approvals::ENGINE_STATE_KEYS`), so the record cannot be forged,
        // and it never overwrites a user key named like the node. `choice` is
        // null for a node without options; `comment` holds the note too, for
        // readers from before `note`. Only an approval reaches here; a
        // rejection fails the node, so nothing downstream sees
        // `approved: false` except a fallback after an expiry.
        let mut record = serde_json::Map::new();
        record.insert(
            item.node_id.clone(),
            json!({
                "approved": true,
                "decision": "approved",
                "approver": user_id,
                "principal": principal,
                "comment": note,
                "note": note,
                "choice": choice,
                "decided_at": decided_at,
            }),
        );
        Ok(ExecutionResult {
            output: json!({
                "decision": "approved",
                "approved_by": user_id,
                "principal": principal,
                "comment": note,
                "note": note,
                "choice": choice,
                "decided_at": decided_at,
                "request_sequence": request_sequence,
                "decision_sequence": sequence,
            }),
            state_patch: serde_json::Value::Object(
                [(
                    APPROVALS_STATE_KEY.to_string(),
                    serde_json::Value::Object(record),
                )]
                .into_iter()
                .collect(),
            ),
            duration_ms: start.elapsed().as_millis() as u64,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
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
        approved_log_with(Some("ok"), None).await
    }

    /// [`approved_log`] with the decision's note and choice.
    async fn approved_log_with(
        note: Option<&str>,
        choice: Option<&str>,
    ) -> (Arc<InMemoryBackend>, ExecutionId) {
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
                comment: note.map(String::from),
                state_patch: None,
                principal: Some("token:b".into()),
                approved_digest: None,
                request_sequence: Some(1),
                choice: choice.map(String::from),
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

    /// The decision is written into the run's state under the node's id.
    #[tokio::test]
    async fn an_approval_records_the_decision_in_state_under_the_node_id() {
        let (backend, eid) = approved_log().await;
        let exec = HumanApprovalExecutor::new(backend.clone());
        let patch = exec.execute(&item(&eid)).await.unwrap().state_patch;
        let rec = &patch["__approvals"]["review"];
        assert_eq!(rec["approved"], true);
        assert_eq!(rec["approver"], "bob");
        assert_eq!(rec["principal"], "token:b");
        assert!(
            rec["decided_at"].as_str().is_some_and(|s| s.ends_with('Z')),
            "{rec}"
        );
        assert_eq!(patch.as_object().unwrap().len(), 1, "only the engine's key");
        assert_eq!(
            patch["__approvals"].as_object().unwrap().len(),
            1,
            "only this node's record"
        );
    }

    /// The decision's choice and note go into the record and the output, read
    /// from the decision on record. The note is also kept as `comment`, its
    /// old name.
    #[tokio::test]
    async fn an_approval_records_its_choice_and_note() {
        let (backend, eid) =
            approved_log_with(Some("Go with the second one."), Some("topic_2")).await;
        let exec = HumanApprovalExecutor::new(backend.clone());
        let result = exec.execute(&item(&eid)).await.unwrap();
        let rec = &result.state_patch["__approvals"]["review"];
        assert_eq!(rec["choice"], "topic_2", "{rec}");
        assert_eq!(rec["note"], "Go with the second one.", "{rec}");
        assert_eq!(rec["comment"], "Go with the second one.", "{rec}");
        let mut keys: Vec<&str> = rec
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "approved",
                "approver",
                "choice",
                "comment",
                "decided_at",
                "decision",
                "note",
                "principal"
            ]
        );
        assert_eq!(result.output["choice"], "topic_2", "{}", result.output);
        assert_eq!(
            result.output["note"], "Go with the second one.",
            "{}",
            result.output
        );
    }

    /// A decision with no choice and no note records both as null.
    #[tokio::test]
    async fn an_approval_without_a_choice_records_none() {
        let (backend, eid) = approved_log_with(None, None).await;
        let exec = HumanApprovalExecutor::new(backend.clone());
        let result = exec.execute(&item(&eid)).await.unwrap();
        let rec = &result.state_patch["__approvals"]["review"];
        for key in ["choice", "note", "comment"] {
            assert_eq!(rec.get(key), Some(&serde_json::Value::Null), "{key}: {rec}");
            assert_eq!(
                result.output.get(key),
                Some(&serde_json::Value::Null),
                "{key}: {}",
                result.output
            );
        }
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
