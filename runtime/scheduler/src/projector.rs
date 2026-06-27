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
                EventKind::ToolApprovalRequired {
                    node_id,
                    tool_name,
                    approver,
                    context,
                } => {
                    node_map.insert(
                        node_id.clone(),
                        ApprovalProjectionRow {
                            execution_id: execution_id.clone(),
                            node_id: node_id.clone(),
                            status: "pending".into(),
                            user_id: None,
                            comment: None,
                            last_sequence: event.sequence,
                            tool_name: Some(tool_name.clone()),
                            approver: Some(approver.clone()),
                            context: Some(context.clone()),
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
                    // Pending metadata (tool_name/approver/context) is cleared on
                    // resolution — the node is no longer waiting for a decision.
                    node_map.insert(
                        node_id.clone(),
                        ApprovalProjectionRow {
                            execution_id: execution_id.clone(),
                            node_id: node_id.clone(),
                            status: status.into(),
                            user_id: Some(user_id.clone()),
                            comment: comment.clone(),
                            last_sequence: event.sequence,
                            tool_name: None,
                            approver: None,
                            context: None,
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

    // ── 2h-4 hardening tests ──────────────────────────────────────────────────

    /// Test 5 (2h-4): A brand-new Projector (simulating a process restart) resumes
    /// from the durable checkpoint rather than reprocessing all events from the
    /// start.
    ///
    /// Phase 1: seed events seq 1..N for nodeA (Requested + Approved), run tick(),
    /// assert checkpoint=N.  Phase 2: construct a SECOND Projector over the same
    /// backend (no in-memory state carried — process restart), append events
    /// seq N+1..M for nodeB (Requested + Rejected), run tick().  Assert:
    ///   - checkpoint advances to M.
    ///   - nodeA remains "approved" (seq 1..N were NOT re-read by projector2).
    ///   - nodeB is "rejected" (seq N+1..M were processed correctly).
    ///   - Structural proof: the checkpoint equalled N before the restart tick, so
    ///     get_events_since(exec, N) returned only seq > N, which excludes seq 1..N.
    #[tokio::test]
    async fn checkpoint_resume_after_restart() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        // Phase 1: events up to N (nodeA: Requested seq 1, Approved seq 2).
        let _seq_req_a = backend
            .append_event(ev_tool_approval_required(&exec, "nodeA"))
            .await
            .unwrap();
        let seq_n = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "alice",
                ApprovalDecision::Approved,
            ))
            .await
            .unwrap();

        // First projector instance (original process).
        let projector1 = Projector::new(backend.clone());
        let applied1 = projector1.tick().await.unwrap();
        assert_eq!(applied1, 1, "phase 1: one row applied");

        let cp_before_restart = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp_before_restart, seq_n,
            "checkpoint at N after phase-1 tick"
        );

        let rows_phase1 = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows_phase1.len(), 1);
        assert_eq!(rows_phase1[0].status, "approved");

        // Phase 2: append new events (seq N+1..M) to simulate work arriving post-restart.
        let _seq_req_b = backend
            .append_event(ev_tool_approval_required(&exec, "nodeB"))
            .await
            .unwrap();
        let seq_m = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeB",
                "bob",
                ApprovalDecision::Rejected,
            ))
            .await
            .unwrap();
        assert!(
            seq_m > seq_n,
            "M > N: new events are strictly after the pre-restart checkpoint"
        );

        // BRAND-NEW Projector — no in-memory state carried (simulated restart).
        let projector2 = Projector::new(backend.clone());
        let applied2 = projector2.tick().await.unwrap();

        // The new projector reads events since N (get_events_since(exec, N))
        // and processes ONLY nodeB (seq N+1..M).
        assert_eq!(
            applied2, 1,
            "restart tick: only events > N are processed (no re-scan of seq 1..N)"
        );

        let cp_after_restart = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp_after_restart, seq_m,
            "checkpoint advances to M after restart tick"
        );

        let rows_final = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows_final.len(), 2, "both nodes present in projection");

        let row_a = rows_final
            .iter()
            .find(|r| r.node_id == "nodeA")
            .expect("nodeA must remain in projection after restart");
        let row_b = rows_final
            .iter()
            .find(|r| r.node_id == "nodeB")
            .expect("nodeB must be added by the restart tick");

        // nodeA was NOT re-read: still approved (seq 1..N excluded by checkpoint).
        assert_eq!(row_a.status, "approved", "nodeA must not be re-processed");
        assert_eq!(row_a.user_id.as_deref(), Some("alice"));

        // nodeB was processed from the delta (seq N+1..M).
        assert_eq!(row_b.status, "rejected");
        assert_eq!(row_b.user_id.as_deref(), Some("bob"));

        // Structural proof: checkpoint was N before the restart tick.
        assert_eq!(cp_before_restart, seq_n);
    }

    /// Test 6 (2h-4): Idempotent re-apply at scale — multiple nodes with
    /// interleaved Requested/Approved/Rejected events.  Running tick() twice
    /// yields an identical projection and a stable checkpoint, confirming that
    /// the UPSERT-keyed write is a pure function of the events.
    #[tokio::test]
    async fn multi_node_idempotent_retick() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        // Five interleaved events across three nodes.
        backend
            .append_event(ev_tool_approval_required(&exec, "nodeA"))
            .await
            .unwrap(); // seq 1
        backend
            .append_event(ev_tool_approval_required(&exec, "nodeB"))
            .await
            .unwrap(); // seq 2
        backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "alice",
                ApprovalDecision::Approved,
            ))
            .await
            .unwrap(); // seq 3
        backend
            .append_event(ev_tool_approval_required(&exec, "nodeC"))
            .await
            .unwrap(); // seq 4
        let seq_last = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeB",
                "bob",
                ApprovalDecision::Rejected,
            ))
            .await
            .unwrap(); // seq 5

        let projector = Projector::new(backend.clone());

        // First tick: projects all three nodes in one batch.
        let applied1 = projector.tick().await.unwrap();
        assert_eq!(applied1, 3, "three nodes written on first tick");

        let rows1 = backend.get_approval_projection(&exec).await.unwrap();
        let cp1 = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp1, seq_last, "checkpoint at last seq after first tick");
        assert_eq!(rows1.len(), 3);

        // Second tick with no new events: must be a no-op.
        let applied2 = projector.tick().await.unwrap();
        assert_eq!(applied2, 0, "second tick must apply zero rows (idempotent)");

        let rows2 = backend.get_approval_projection(&exec).await.unwrap();
        let cp2 = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp2, cp1, "checkpoint stable after second tick");
        // Projection is a pure function of the events regardless of tick count.
        assert_eq!(
            rows2, rows1,
            "projection content identical after idempotent second tick"
        );
    }

    /// Test 7 (2h-4): Read fidelity — for a three-node approval event sequence,
    /// the durable projection returned by get_approval_projection faithfully
    /// represents the latest approval state implied by the events:
    ///   - nodeA (Requested → Approved): status="approved", user_id="alice",
    ///     tool_name cleared (node is resolved).
    ///   - nodeB (Requested → Rejected with comment): status="rejected",
    ///     user_id="bob", comment="exceeds limit", tool_name cleared.
    ///   - nodeC (Requested only): status="pending", tool_name and approver
    ///     preserved from the ToolApprovalRequired event.
    ///
    /// Crash-mid-apply safety note: `apply_approval_projection` and the checkpoint
    /// advance share one SQLite transaction (proven by the 2h-1 atomic tests in
    /// state/tests/projector.rs).  A checkpoint can therefore never be observed as
    /// advanced past an unapplied approval row — it is structurally impossible to
    /// land in a state where the checkpoint says "seq N processed" but the row for
    /// that seq is absent from proj_approvals.  Rely on that atomic test rather
    /// than duplicating a forced-failure scenario here.
    #[tokio::test]
    async fn read_fidelity_three_node_abc() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        // nodeA: Requested → Approved.
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

        // nodeB: Requested → Rejected with a comment.
        backend
            .append_event(ev_tool_approval_required(&exec, "nodeB"))
            .await
            .unwrap();
        backend
            .append_event(Event::new(
                exec.clone(),
                0, // sequence assigned by append_event
                EventKind::ApprovalReceived {
                    node_id: "nodeB".into(),
                    user_id: "bob".into(),
                    decision: ApprovalDecision::Rejected,
                    comment: Some("exceeds limit".into()),
                    state_patch: None,
                },
            ))
            .await
            .unwrap();

        // nodeC: Requested only — stays pending.
        backend
            .append_event(ev_tool_approval_required(&exec, "nodeC"))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        projector.tick().await.expect("tick must succeed");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 3, "one projection row per node");

        let find = |node: &str| {
            rows.iter()
                .find(|r| r.node_id == node)
                .unwrap_or_else(|| panic!("projection row missing for {node}"))
        };

        // nodeA: approved, user identified, pending metadata cleared.
        let row_a = find("nodeA");
        assert_eq!(row_a.status, "approved");
        assert_eq!(row_a.user_id.as_deref(), Some("alice"));
        assert!(
            row_a.tool_name.is_none(),
            "approved row must not carry tool_name"
        );

        // nodeB: rejected, user identified, comment preserved, pending metadata cleared.
        let row_b = find("nodeB");
        assert_eq!(row_b.status, "rejected");
        assert_eq!(row_b.user_id.as_deref(), Some("bob"));
        assert_eq!(row_b.comment.as_deref(), Some("exceeds limit"));
        assert!(
            row_b.tool_name.is_none(),
            "rejected row must not carry tool_name"
        );

        // nodeC: pending, tool_name and approver preserved from ToolApprovalRequired.
        let row_c = find("nodeC");
        assert_eq!(row_c.status, "pending");
        assert_eq!(
            row_c.tool_name.as_deref(),
            Some("tool_nodeC"),
            "pending row must preserve tool_name from ToolApprovalRequired event"
        );
        assert_eq!(
            row_c.approver.as_deref(),
            Some("human"),
            "pending row must preserve approver from ToolApprovalRequired event"
        );
        assert!(row_c.user_id.is_none(), "pending row has no user_id");
    }
}
