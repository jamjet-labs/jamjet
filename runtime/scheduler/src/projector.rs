//! Async approval projector — consumes the event log and maintains a durable
//! `proj_approvals` read-model keyed by `(execution_id, node_id)`.
//!
//! The projector runs as a background task alongside the scheduler.  Each tick
//! it: paginates over all running executions (avoiding the 100-execution cap);
//! for each, reads events since its per-execution checkpoint; folds approval
//! events into the projection using first-decision-wins semantics (matching the
//! runtime's canonical fold in `approvals.rs`); seeds prior-tick state from the
//! existing projection so a cross-tick late decision cannot overwrite a decided
//! node; and advances the checkpoint atomically with ALL changed rows in a
//! single transaction (via `apply_approval_projection_batch`).
//!
//! # Invariants
//!
//! - **Atomicity**: the checkpoint advances IFF every changed row in the batch
//!   committed.  A crash before `commit()` leaves both rows and checkpoint
//!   unchanged; the next tick re-reads the same events and re-applies.
//!
//! - **First-decision-wins**: an `ApprovalReceived` event only transitions a
//!   node that is currently `"pending"`.  A node that is already `"approved"`
//!   or `"rejected"` cannot be overwritten by a later decision.  Orphan
//!   `ApprovalReceived` events (no prior `ToolApprovalRequired`) are ignored.
//!
//! - **Full coverage**: `tick()` paginates over all Running executions so the
//!   100-row default limit cannot silently drop executions beyond the first page.

use jamjet_core::workflow::{ExecutionId, WorkflowStatus};
use jamjet_state::{
    backend::{ApprovalProjectionRow, BackendResult, StateBackend},
    event::{ApprovalDecision, EventKind},
};
use std::collections::{HashMap, HashSet};
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
    /// Paginates over Running executions so the 100-row page limit does not
    /// silently cap coverage.  Returns the total number of approval rows
    /// written (for tests and monitoring).  Non-approval events are consumed
    /// and the checkpoint is advanced past them, but they do not contribute to
    /// the returned count.  Per-execution errors are logged and skipped so a
    /// single bad execution does not abort the whole tick.
    pub async fn tick(&self) -> BackendResult<usize> {
        let page_size: u32 = 100;
        let mut offset: u32 = 0;
        let mut total_applied = 0usize;

        loop {
            let page = self
                .backend
                .list_executions(Some(WorkflowStatus::Running), page_size, offset)
                .await?;
            let page_len = page.len();

            for exec in page {
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

            if (page_len as u32) < page_size {
                break;
            }
            offset += page_len as u32;
        }

        Ok(total_applied)
    }

    /// Project a single execution: fetch the event delta, fold approval events,
    /// and advance the checkpoint to the batch maximum atomically with all
    /// changed rows.
    ///
    /// ### Fold semantics (first-decision-wins, matching `approvals.rs`)
    ///
    /// 1. Seed the working node-state from the existing projection
    ///    (`get_approval_projection`).  This means prior-tick decisions are
    ///    visible during the fold and cannot be overwritten.
    /// 2. For each new event in sequence order:
    ///    - `ToolApprovalRequired` → set node to `"pending"` with
    ///      tool/approver/context metadata.
    ///    - `ApprovalReceived` → only if the node is currently `"pending"`,
    ///      set to `"approved"` / `"rejected"`.  If the node is already
    ///      decided (or never requested — orphan), ignore the event.
    /// 3. Emit only the rows for nodes actually modified in this batch.
    ///
    /// ### Atomicity
    ///
    /// `apply_approval_projection_batch` writes all changed rows AND the
    /// checkpoint in one `BEGIN IMMEDIATE` transaction.  If the process crashes
    /// mid-batch, the next tick re-reads from the old checkpoint and re-applies.
    async fn project_execution(&self, execution_id: &ExecutionId) -> BackendResult<usize> {
        let cp = self
            .backend
            .get_projector_checkpoint("approvals", execution_id)
            .await?;

        let mut events = self.backend.get_events_since(execution_id, cp).await?;
        // Defensive sort: the trait contract requires ascending sequence order, but
        // sort here so the fold is correct regardless of backend implementation.
        // An ApprovalReceived processed before its ToolApprovalRequired (reversed
        // delivery) would be treated as an orphan, leaving the node stuck pending.
        events.sort_by_key(|e| e.sequence);

        if events.is_empty() {
            return Ok(0);
        }

        // Safety: events is non-empty so max() is Some.
        let batch_max = events.iter().map(|e| e.sequence).max().unwrap();

        // Seed working state from the existing projection so prior-tick
        // decisions are visible during the fold (first-decision-wins across
        // ticks).
        let existing = self.backend.get_approval_projection(execution_id).await?;
        let mut node_map: HashMap<String, ApprovalProjectionRow> = existing
            .into_iter()
            .map(|r| (r.node_id.clone(), r))
            .collect();

        // Track nodes modified in this batch (these are the rows to write).
        let mut touched: HashSet<String> = HashSet::new();

        for event in &events {
            match &event.kind {
                EventKind::ToolApprovalRequired {
                    node_id,
                    tool_name,
                    approver,
                    context,
                } => {
                    touched.insert(node_id.clone());
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
                    // First-decision-wins: only transition a node that is
                    // currently pending.  A decided node (approved / rejected)
                    // and an orphan (never requested) are both ignored.
                    let currently_pending = node_map
                        .get(node_id)
                        .map(|r| r.status == "pending")
                        .unwrap_or(false);

                    if currently_pending {
                        touched.insert(node_id.clone());
                        let status = match decision {
                            ApprovalDecision::Approved => "approved",
                            ApprovalDecision::Rejected => "rejected",
                        };
                        // Pending metadata (tool_name/approver/context) is
                        // cleared on resolution — the node is no longer
                        // waiting for a decision.
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
                    // else: orphan or already-decided → ignore.
                }
                _ => {
                    // Non-approval event: ignored for content, but the cursor
                    // still advances to batch_max below so it is not re-scanned.
                }
            }
        }

        // Collect only the rows that were modified in this batch.
        let rows_to_write: Vec<ApprovalProjectionRow> = touched
            .iter()
            .filter_map(|node_id| node_map.remove(node_id))
            .collect();

        if !rows_to_write.is_empty() {
            let count = rows_to_write.len();
            // Atomic: all rows + checkpoint in one transaction.
            self.backend
                .apply_approval_projection_batch(
                    rows_to_write,
                    "approvals",
                    execution_id,
                    batch_max,
                )
                .await?;
            Ok(count)
        } else {
            // Batch had events but none produced approval changes (all
            // non-approval, all orphan decisions, or all late post-decision
            // events).  Advance the checkpoint so this tail is not re-scanned.
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
    /// same node in the same tick.  First-decision-wins within a batch: approved
    /// wins; checkpoint advances to max sequence; second tick is a no-op.
    #[tokio::test]
    async fn approved_beats_prior_pending_and_checkpoint_idempotent() {
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
            "approved overrides pending within the same batch"
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

    /// Test 2 (first-decision-wins, cross-tick): nodeA is approved in tick 1.
    /// A late Rejected event arriving in tick 2 must be ignored — the earlier
    /// approved decision stands.  The checkpoint still advances.
    #[tokio::test]
    async fn late_decision_ignored_after_first_approval() {
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
        projector.tick().await.unwrap(); // Tick 1: nodeA approved.

        // Late rejection arrives after the first decision.
        let seq_rejected = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "bob",
                ApprovalDecision::Rejected,
            ))
            .await
            .unwrap();

        // Tick 2: the late rejection is ignored because nodeA is already decided.
        let applied = projector.tick().await.unwrap();
        assert_eq!(
            applied, 0,
            "late rejection ignored: nodeA already approved (first-decision-wins)"
        );

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 1, "still exactly one row (upsert)");
        assert_eq!(
            rows[0].status, "approved",
            "approved must not be overwritten"
        );
        assert_eq!(
            rows[0].user_id.as_deref(),
            Some("alice"),
            "alice's approval stands"
        );

        // Checkpoint still advances past the ignored event.
        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp, seq_rejected, "checkpoint advances past the late event");
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
        // batch_max = seq_completed; apply_approval_projection_batch advances checkpoint there.
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

        // nodeA was NOT re-read from events: still approved (seq 1..N excluded by
        // checkpoint).  The seeded state from get_approval_projection confirmed its
        // "approved" status and the late-rejection path correctly ignored it.
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
    /// Crash-mid-apply safety note: `apply_approval_projection_batch` and the
    /// checkpoint advance share one SQLite transaction (proven by the 2h-1
    /// atomic tests in state/tests/projector.rs).  A checkpoint can therefore
    /// never be observed as advanced past an unapplied approval row — it is
    /// structurally impossible to land in a state where the checkpoint says
    /// "seq N processed" but the row for that seq is absent from proj_approvals.
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

    // ── C1: batch atomicity ───────────────────────────────────────────────────

    /// C1: A batch with 3 Required events (nodes A, B, C) in one tick must write
    /// all 3 rows and advance the checkpoint to batch_max in one atomic apply.
    ///
    /// The single-transaction guarantee means a crash mid-batch leaves NO rows
    /// committed and the checkpoint un-advanced; the next tick re-reads the same
    /// events and re-applies correctly.  (The happy-path here proves the all-3
    /// case; the rollback guarantee is structural — one `BEGIN IMMEDIATE` in
    /// `apply_approval_projection_batch`.)
    #[tokio::test]
    async fn batch_atomic_three_nodes_all_committed() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        let _seq_a = backend
            .append_event(ev_tool_approval_required(&exec, "nodeA"))
            .await
            .unwrap();
        let _seq_b = backend
            .append_event(ev_tool_approval_required(&exec, "nodeB"))
            .await
            .unwrap();
        let seq_c = backend
            .append_event(ev_tool_approval_required(&exec, "nodeC"))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.expect("tick must succeed");
        assert_eq!(applied, 3, "all 3 nodes written in one atomic batch apply");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 3, "all 3 rows present after batch apply");

        let node_ids: Vec<&str> = rows.iter().map(|r| r.node_id.as_str()).collect();
        assert!(node_ids.contains(&"nodeA"), "nodeA must be in projection");
        assert!(node_ids.contains(&"nodeB"), "nodeB must be in projection");
        assert!(node_ids.contains(&"nodeC"), "nodeC must be in projection");

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp, seq_c,
            "checkpoint == batch_max: all rows + checkpoint committed atomically"
        );
    }

    // ── I2: pagination ────────────────────────────────────────────────────────

    /// I2: Create 105 Running executions (> one 100-row page) each with a pending
    /// approval.  A single tick() must project ALL 105; none dropped due to the
    /// old hard-coded limit=100,offset=0 query.
    #[tokio::test]
    async fn pagination_covers_more_than_100_running_executions() {
        let backend = Arc::new(InMemoryBackend::new());
        let count = 105usize;
        let mut exec_ids = Vec::with_capacity(count);

        for _ in 0..count {
            let exec = ExecutionId::new();
            backend
                .create_execution(running_execution(&exec))
                .await
                .unwrap();
            backend
                .append_event(ev_tool_approval_required(&exec, "nodeA"))
                .await
                .unwrap();
            exec_ids.push(exec);
        }

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.expect("tick must succeed");
        assert_eq!(
            applied, count,
            "all {count} executions must be projected (pagination works)"
        );

        // Verify every execution individually has its row.
        let mut missing = 0usize;
        for exec in &exec_ids {
            let rows = backend.get_approval_projection(exec).await.unwrap();
            if rows.is_empty() {
                missing += 1;
            }
        }
        assert_eq!(
            missing, 0,
            "every execution must have a projected row — {missing} were missing"
        );
    }

    // ── I3: first-decision-wins ───────────────────────────────────────────────

    /// I3a: Required(s1) + Approved-alice(s2) + Rejected-bob(s3) in one tick →
    /// projection shows approved/alice (first decision wins within batch).
    #[tokio::test]
    async fn first_decision_wins_approved_beats_same_tick_rejection() {
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
        let seq_last = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "bob",
                ApprovalDecision::Rejected,
            ))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.expect("tick must succeed");
        assert_eq!(applied, 1, "exactly one row written");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "approved",
            "first decision wins: approved/alice beats rejected/bob"
        );
        assert_eq!(rows[0].user_id.as_deref(), Some("alice"));

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(
            cp, seq_last,
            "checkpoint advances to batch_max even though last event was ignored"
        );
    }

    /// I3b: Orphan ApprovalReceived (no prior ToolApprovalRequired) must not
    /// create a projection row.  The checkpoint still advances.
    #[tokio::test]
    async fn orphan_approval_received_creates_no_row() {
        let backend = Arc::new(InMemoryBackend::new());
        let exec = ExecutionId::new();
        backend
            .create_execution(running_execution(&exec))
            .await
            .unwrap();

        let seq = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "alice",
                ApprovalDecision::Approved,
            ))
            .await
            .unwrap();

        let projector = Projector::new(backend.clone());
        let applied = projector.tick().await.expect("tick must succeed");
        assert_eq!(applied, 0, "orphan ApprovalReceived must not create a row");

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert!(rows.is_empty(), "no projection rows for orphan decision");

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp, seq, "checkpoint still advances past orphan event");
    }

    /// I3c: Decision split across two ticks — Required+Approved in tick 1, a late
    /// Rejected in tick 2.  Projection stays approved (seed-from-existing-projection
    /// path: nodeA is "approved" at tick-2 seed time, so the Rejected is ignored).
    #[tokio::test]
    async fn cross_tick_first_decision_wins_stays_approved() {
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
        projector.tick().await.unwrap(); // Tick 1: nodeA approved.

        // Late rejection arrives in a separate tick.
        let seq_late = backend
            .append_event(ev_approval_received(
                &exec,
                "nodeA",
                "bob",
                ApprovalDecision::Rejected,
            ))
            .await
            .unwrap();

        let applied = projector.tick().await.expect("tick 2 must succeed");
        assert_eq!(
            applied, 0,
            "late rejection ignored: nodeA already decided in prior tick"
        );

        let rows = backend.get_approval_projection(&exec).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "approved",
            "nodeA stays approved after cross-tick late rejection"
        );
        assert_eq!(rows[0].user_id.as_deref(), Some("alice"));

        let cp = backend
            .get_projector_checkpoint("approvals", &exec)
            .await
            .unwrap();
        assert_eq!(cp, seq_late, "checkpoint advances past the ignored event");
    }

    /// Finding C — the fold must process events in sequence order.
    /// Without the sort, an ApprovalReceived delivered before its
    /// ToolApprovalRequired (reversed delivery) would be treated as an orphan
    /// and the node would be left pending even after approval.
    ///
    /// The InMemoryBackend always returns events in ascending sequence order so
    /// the sort is a no-op here; this test documents what the sort guards against
    /// by verifying the sort places Required (seq 1) before Received (seq 2)
    /// when the input vec is constructed in reverse delivery order.
    #[test]
    fn sort_by_sequence_places_required_before_received() {
        use jamjet_state::event::{ApprovalDecision, Event as StateEvent, EventKind};
        let exec = ExecutionId::new();
        let received_first = StateEvent::new(
            exec.clone(),
            2,
            EventKind::ApprovalReceived {
                node_id: "nodeA".into(),
                user_id: "alice".into(),
                decision: ApprovalDecision::Approved,
                comment: None,
                state_patch: None,
            },
        );
        let required_second = StateEvent::new(
            exec.clone(),
            1,
            EventKind::ToolApprovalRequired {
                node_id: "nodeA".into(),
                tool_name: "tool_nodeA".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
        );
        // Simulate reversed delivery: Received (seq 2) arrives before Required (seq 1).
        let mut events = vec![received_first, required_second];
        events.sort_by_key(|e| e.sequence);
        // After sort: seq 1 (Required) must be first.
        assert_eq!(
            events[0].sequence, 1,
            "Required (seq 1) must come first after sort"
        );
        assert_eq!(
            events[1].sequence, 2,
            "Received (seq 2) must come second after sort"
        );
        assert!(
            matches!(&events[0].kind, EventKind::ToolApprovalRequired { node_id, .. } if node_id == "nodeA"),
            "first event after sort must be ToolApprovalRequired"
        );
        assert!(
            matches!(&events[1].kind, EventKind::ApprovalReceived { node_id, .. } if node_id == "nodeA"),
            "second event after sort must be ApprovalReceived"
        );
    }
}
