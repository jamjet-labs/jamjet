//! Async approval projector — consumes the event log and maintains a durable
//! `proj_approvals` read-model keyed by `(execution_id, node_id)`.
//!
//! The projector runs as a background task alongside the scheduler.  Each tick
//! it: loads all running executions; for each, reads events since its
//! per-execution checkpoint; folds approval events into the projection (last-
//! write-wins per node); and advances the checkpoint to the maximum event
//! sequence seen in the batch regardless of whether any approval rows changed.
//! This ensures non-approval events do not stall the cursor.
//!
//! Crash-safe: the UPSERT and checkpoint advance happen in one transaction
//! inside `apply_approval_projection`.  A restart re-reads only the delta
//! since the durable checkpoint.

use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_state::{
    backend::{ApprovalProjectionRow, BackendResult, StateBackend},
    event::{ApprovalDecision, EventKind},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Async read-model projector.
///
/// Maintains the `proj_approvals` projection by tailing each running
/// execution's event log and folding approval events into durable rows.
pub struct Projector {
    backend: Arc<dyn StateBackend>,
}

impl Projector {
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        Self { backend }
    }

    /// One projection pass over all running executions.
    ///
    /// Returns the total number of approval rows written (for tests and
    /// monitoring).  Non-approval events are consumed and the checkpoint is
    /// advanced past them, but they do not contribute to the returned count.
    pub async fn tick(&self) -> BackendResult<usize> {
        let executions = self
            .backend
            .list_executions(Some(WorkflowStatus::Running), 100, 0)
            .await?;

        let mut total_applied = 0usize;

        for exec in executions {
            match self.project_execution(&exec.execution_id).await {
                Ok(n) => total_applied += n,
                Err(e) => {
                    warn!(
                        execution_id = %exec.execution_id,
                        "Projector: error projecting execution: {e}"
                    );
                }
            }
        }

        Ok(total_applied)
    }

    /// Project a single execution: fetch the event delta, fold approval events,
    /// and advance the checkpoint to the batch maximum.
    ///
    /// The checkpoint always reaches `batch_max` whether or not approval rows
    /// changed — preventing both re-scan (stale cursor) and skip (approval
    /// after a non-approval tail).
    async fn project_execution(&self, execution_id: &ExecutionId) -> BackendResult<usize> {
        let cp = self
            .backend
            .get_projector_checkpoint("approvals", execution_id)
            .await?;

        let events = self.backend.get_events_since(execution_id, cp).await?;

        if events.is_empty() {
            return Ok(0);
        }

        // Safety: events is non-empty so max() is Some.
        let batch_max = events.iter().map(|e| e.sequence).max().unwrap();

        // Fold approval events (last-write-wins per node_id, ordered by seq).
        let mut node_map: HashMap<String, ApprovalProjectionRow> = HashMap::new();

        for event in &events {
            match &event.kind {
                EventKind::ToolApprovalRequired { node_id, .. } => {
                    node_map.insert(
                        node_id.clone(),
                        ApprovalProjectionRow {
                            execution_id: execution_id.clone(),
                            node_id: node_id.clone(),
                            status: "pending".into(),
                            user_id: None,
                            comment: None,
                            last_sequence: event.sequence,
                        },
                    );
                }
                EventKind::ApprovalReceived {
                    node_id,
                    user_id,
                    decision,
                    comment,
                    ..
                } => {
                    let status = match decision {
                        ApprovalDecision::Approved => "approved",
                        ApprovalDecision::Rejected => "rejected",
                    };
                    node_map.insert(
                        node_id.clone(),
                        ApprovalProjectionRow {
                            execution_id: execution_id.clone(),
                            node_id: node_id.clone(),
                            status: status.into(),
                            user_id: Some(user_id.clone()),
                            comment: comment.clone(),
                            last_sequence: event.sequence,
                        },
                    );
                }
                _ => {
                    // Non-approval event: ignored for content, but the cursor
                    // still advances to batch_max below so it is not re-scanned.
                }
            }
        }

        if !node_map.is_empty() {
            let count = node_map.len();
            for row in node_map.into_values() {
                // apply_approval_projection atomically UPSERTs the row AND sets
                // the checkpoint to batch_max.  For the second and later rows in
                // the same batch the checkpoint write is idempotent (same value).
                self.backend
                    .apply_approval_projection(row, "approvals", batch_max)
                    .await?;
            }
            Ok(count)
        } else {
            // Batch had events but none were approval events.  Advance the
            // checkpoint only so this tail is not re-scanned next tick.
            self.backend
                .set_projector_checkpoint("approvals", execution_id, batch_max)
                .await?;
            Ok(0)
        }
    }

    /// Background loop: tick then sleep 500 ms, forever.
    ///
    /// Mirrors the scheduler's cadence.  There is no graceful-shutdown wiring
    /// today (follow-up: F-2h-shutdown).
    pub async fn run(self) {
        loop {
            if let Err(e) = self.tick().await {
                warn!("Projector tick error: {e}");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
    use jamjet_state::{
        backend::StateBackend,
        event::{ApprovalDecision, Event, EventKind},
        InMemoryBackend,
    };
    use serde_json::json;

    fn running_execution(id: &ExecutionId) -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            execution_id: id.clone(),
            workflow_id: "wf-projector-test".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: json!({}),
            current_state: json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
            parent_execution_id: None,
            segment_number: 0,
        }
    }

    fn ev_tool_approval_required(exec: &ExecutionId, node: &str) -> Event {
        Event::new(
            exec.clone(),
            0, // sequence overwritten by InMemoryBackend::append_event
            EventKind::ToolApprovalRequired {
                node_id: node.into(),
                tool_name: format!("tool_{node}"),
                approver: "human".into(),
                context: json!({}),
            },
        )
    }

    fn ev_approval_received(
        exec: &ExecutionId,
        node: &str,
        user: &str,
        decision: ApprovalDecision,
    ) -> Event {
        Event::new(
            exec.clone(),
            0,
            EventKind::ApprovalReceived {
                node_id: node.into(),
                user_id: user.into(),
                decision,
                comment: None,
                state_patch: None,
            },
        )
    }

    fn ev_node_completed(exec: &ExecutionId, node: &str) -> Event {
        Event::new(
            exec.clone(),
            0,
            EventKind::NodeCompleted {
                node_id: node.into(),
                output: json!({}),
                state_patch: json!({}),
                duration_ms: 1,
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
    }

    /// Test 1: ToolApprovalRequired followed by ApprovalReceived(Approved) for the
    /// same node.  Projector must surface the LAST event (last-write-wins) as
    /// "approved", advance the checkpoint to the max sequence, and be idempotent
    /// on a second tick.
    #[tokio::test]
    async fn last_write_wins_and_checkpoint_idempotent() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        let _seq_req = backend
            .append_event(ev_tool_approval_required(&exec, "nodeA"))
            .await
            .unwrap();
        let seq_grant = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "alice",
                ApprovalDecision::Approved,
            ))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.expect("first tick must succeed");
        assert_eq!(applied, 1, "one approval row written");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 1, "exactly one projected row");
        assert_eq!(rows[0].node_id, "nodeA");
        assert_eq!(
            rows[0].status, "approved",
            "last-write-wins: granted overrides pending"
        );
        assert_eq!(rows[0].user_id.as_deref(), Some("alice"));

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp, seq_grant, "checkpoint at max sequence in batch");

        // Second tick: no new events → 0 applied, checkpoint stable.
        let applied2 = projector.tick().await.expect("second tick must succeed");
        assert_eq!(applied2, 0, "second tick is a no-op");

        let cp2 = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp2, seq_grant, "checkpoint unchanged after idle tick");

        // Row unchanged too.
        let rows2 = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows2, rows, "projection unchanged after idle tick");
    }

    /// Test 2: After an Approved row, appending an ApprovalReceived(Rejected)
    /// updates the same node to "rejected" and advances the checkpoint.
    #[tokio::test]
    async fn denied_overwrites_approved() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        backend
            .append_event(ev_tool_approval_required(&exec, "nodeA"))
            .await
            .unwrap();
        backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "alice",
                ApprovalDecision::Approved,
            ))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        projector.tick().await.unwrap();

        let seq_rejected = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "bob",
                ApprovalDecision::Rejected,
            ))
            .await
            .unwrap();

        let applied = projector.tick().await.unwrap();
        assert_eq!(applied, 1, "one row updated to rejected");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 1, "still exactly one row (upsert)");
        assert_eq!(rows[0].status, "rejected");
        assert_eq!(rows[0].user_id.as_deref(), Some("bob"));

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp, seq_rejected,
            "checkpoint advanced to rejected event seq"
        );
    }

    /// Test 3: A batch where the highest-sequence events are NON-approval events
    /// (NodeCompleted appended after ToolApprovalRequired).  The checkpoint must
    /// advance to the non-approval tail seq (batch_max), and a second tick must
    /// NOT re-scan or re-apply the approval event.
    #[tokio::test]
    async fn non_approval_tail_advances_checkpoint_no_rescan() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        // Approval at seq S1; non-approval at seq S2 > S1.
        let _seq_req = backend
            .append_event(ev_tool_approval_required(&exec, "nodeA"))
            .await
            .unwrap();
        let seq_completed = backend
            .append_event(ev_node_completed(&exec, "nodeA"))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.unwrap();

        // node_map has nodeA (pending from ToolApprovalRequired).
        // batch_max = seq_completed; apply_approval_projection advances checkpoint there.
        assert_eq!(applied, 1, "pending row written");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "pending");

        // KEY: checkpoint is at seq_completed (batch_max), NOT at the approval seq.
        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp, seq_completed,
            "checkpoint must equal batch_max (non-approval tail seq)"
        );

        // Second tick: get_events_since(exec, seq_completed) = [] → no re-scan.
        let applied2 = projector.tick().await.unwrap();
        assert_eq!(applied2, 0, "no re-scan after non-approval tail");

        let cp2 = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp2, seq_completed, "checkpoint stable after idle tick");
    }

    /// Test 4: A batch with ONLY non-approval events.  No approval rows written;
    /// checkpoint advances past them via set_projector_checkpoint.
    #[tokio::test]
    async fn non_approval_only_batch_advances_checkpoint() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        let seq_completed = backend
            .append_event(ev_node_completed(&exec, "nodeA"))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.unwrap();
        assert_eq!(applied, 0, "no approval rows for non-approval-only batch");

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp, seq_completed,
            "checkpoint advances past non-approval events"
        );

        // Second tick: idempotent.
        let applied2 = projector.tick().await.unwrap();
        assert_eq!(applied2, 0, "second tick is a no-op");
    }
}
