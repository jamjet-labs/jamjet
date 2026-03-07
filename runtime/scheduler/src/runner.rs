use jamjet_core::node::NodeId;
use jamjet_core::workflow::ExecutionId;
use jamjet_ir::WorkflowIr;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

/// The JamJet scheduler drives workflow execution.
///
/// It runs as a Tokio async loop, detecting runnable nodes and dispatching
/// them to the worker queue.
pub struct Scheduler {
    backend: Arc<dyn StateBackend>,
    poll_interval: Duration,
}

impl Scheduler {
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        Self {
            backend,
            poll_interval: Duration::from_millis(500),
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Run the scheduler loop. This runs indefinitely until the future is cancelled.
    pub async fn run(&self) {
        info!("Scheduler started (poll_interval={:?})", self.poll_interval);
        loop {
            if let Err(e) = self.tick().await {
                warn!("Scheduler tick error: {e}");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// A single scheduler tick: reclaim expired leases, then find all running
    /// executions and dispatch any newly runnable nodes.
    #[instrument(skip(self))]
    async fn tick(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 1. Reclaim expired leases (worker crashes / stalls).
        self.reclaim_expired_leases().await.unwrap_or_else(|e| {
            warn!("Failed to reclaim expired leases: {e}");
        });

        // 2. Schedule runnable nodes for each running execution.
        let running = self
            .backend
            .list_executions(Some(jamjet_core::workflow::WorkflowStatus::Running), 100, 0)
            .await?;

        for execution in running {
            self.schedule_runnable_nodes(
                &execution.execution_id,
                &execution.workflow_id,
                &execution.workflow_version,
            )
            .await
            .unwrap_or_else(|e| {
                warn!(
                    execution_id = %execution.execution_id,
                    "Failed to schedule runnable nodes: {e}"
                );
            });
        }
        Ok(())
    }

    /// Reclaim expired work item leases and emit appropriate events.
    async fn reclaim_expired_leases(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let reclaimed = self.backend.reclaim_expired_leases().await?;

        for item in &reclaimed.retryable {
            // Emit NodeFailed{retryable: true} + RetryScheduled
            let seq = self.backend.latest_sequence(&item.execution_id).await? + 1;
            let failed_event = jamjet_state::Event::new(
                item.execution_id.clone(),
                seq,
                jamjet_state::event::EventKind::NodeFailed {
                    node_id: item.node_id.clone(),
                    error: "lease expired: worker presumed dead".into(),
                    attempt: item.attempt.saturating_sub(1),
                    retryable: true,
                },
            );
            self.backend.append_event(failed_event).await?;

            let seq = self.backend.latest_sequence(&item.execution_id).await? + 1;
            let retry_event = jamjet_state::Event::new(
                item.execution_id.clone(),
                seq,
                jamjet_state::event::EventKind::RetryScheduled {
                    node_id: item.node_id.clone(),
                    attempt: item.attempt,
                    delay_ms: (1u64 << item.attempt.min(6)) * 1000,
                },
            );
            self.backend.append_event(retry_event).await?;

            warn!(
                execution_id = %item.execution_id,
                node_id = %item.node_id,
                attempt = item.attempt,
                "Lease expired — requeueing for retry"
            );
        }

        for item in &reclaimed.exhausted {
            // Emit NodeFailed{retryable: false} — permanently dead
            let seq = self.backend.latest_sequence(&item.execution_id).await? + 1;
            let failed_event = jamjet_state::Event::new(
                item.execution_id.clone(),
                seq,
                jamjet_state::event::EventKind::NodeFailed {
                    node_id: item.node_id.clone(),
                    error: format!("exhausted {} attempts: lease expired", item.max_attempts),
                    attempt: item.attempt,
                    retryable: false,
                },
            );
            self.backend.append_event(failed_event).await?;

            warn!(
                execution_id = %item.execution_id,
                node_id = %item.node_id,
                attempts = item.attempt,
                "Node exhausted retries — moved to dead-letter queue"
            );
        }

        Ok(())
    }

    /// For a given execution, find all nodes that can now run and enqueue them.
    #[instrument(skip(self), fields(execution_id = %execution_id))]
    async fn schedule_runnable_nodes(
        &self,
        execution_id: &ExecutionId,
        workflow_id: &str,
        workflow_version: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Load the workflow IR from the registry.
        let def = self
            .backend
            .get_workflow(workflow_id, workflow_version)
            .await?;
        let Some(def) = def else {
            warn!(%workflow_id, %workflow_version, "Workflow definition not found; cannot schedule");
            return Ok(());
        };
        let ir: WorkflowIr = serde_json::from_value(def.ir)?;

        // Load events to build the set of completed/scheduled/failed nodes.
        let events = self.backend.get_events(execution_id).await?;

        let mut completed: HashSet<NodeId> = HashSet::new();
        let mut scheduled: HashSet<NodeId> = HashSet::new();
        let mut terminal_failed: HashSet<NodeId> = HashSet::new();

        for event in &events {
            match &event.kind {
                EventKind::NodeCompleted { node_id, .. }
                | EventKind::NodeSkipped { node_id, .. } => {
                    completed.insert(node_id.clone());
                    scheduled.remove(node_id);
                }
                EventKind::NodeScheduled { node_id, .. }
                | EventKind::NodeStarted { node_id, .. } => {
                    scheduled.insert(node_id.clone());
                }
                EventKind::NodeCancelled { node_id } => {
                    completed.insert(node_id.clone());
                    scheduled.remove(node_id);
                }
                EventKind::NodeFailed {
                    node_id,
                    retryable: false,
                    ..
                } => {
                    terminal_failed.insert(node_id.clone());
                    scheduled.remove(node_id);
                }
                EventKind::NodeFailed {
                    node_id,
                    retryable: true,
                    ..
                } => {
                    // Will be re-queued by RetryScheduled event; remove from scheduled.
                    scheduled.remove(node_id);
                }
                EventKind::RetryScheduled { node_id, .. } => {
                    // Node will be re-queued; treat as scheduled.
                    scheduled.insert(node_id.clone());
                }
                _ => {}
            }
        }

        debug!(
            execution_id = %execution_id,
            completed_nodes = completed.len(),
            scheduled_nodes = scheduled.len(),
            terminal_failed_nodes = terminal_failed.len(),
            "Checking for runnable nodes"
        );

        // Find nodes that are runnable and enqueue them.
        let mut enqueued = 0usize;
        for (node_id, node) in &ir.nodes {
            if terminal_failed.contains(node_id.as_str()) {
                continue; // permanently failed
            }
            if is_runnable(node_id, &ir, &completed, &scheduled) {
                // Serialize the QueueType variant name to snake_case string.
                let queue_type = serde_json::to_value(node.kind.queue_type())
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_else(|| "general".to_string());

                // Append NodeScheduled event first (sequenced).
                let seq = self.backend.latest_sequence(execution_id).await? + 1;
                let sched_event = jamjet_state::Event::new(
                    execution_id.clone(),
                    seq,
                    EventKind::NodeScheduled {
                        node_id: node_id.clone(),
                        queue_type: queue_type.clone(),
                    },
                );
                self.backend.append_event(sched_event).await?;

                // Enqueue work item.
                // Default max_attempts based on retry policy name (Phase 2: full policy resolution).
                let max_attempts: u32 = match node.retry_policy.as_deref() {
                    Some("no_retry") => 1,
                    Some("io_default") => 5,
                    Some("llm_default") => 3,
                    _ => 3,
                };

                let item = WorkItem {
                    id: Uuid::new_v4(),
                    execution_id: execution_id.clone(),
                    node_id: node_id.clone(),
                    queue_type,
                    payload: serde_json::json!({
                        "workflow_id": workflow_id,
                        "workflow_version": workflow_version,
                        "node_id": node_id,
                    }),
                    attempt: 0,
                    max_attempts,
                    created_at: chrono::Utc::now(),
                    lease_expires_at: None,
                    worker_id: None,
                };
                self.backend.enqueue_work_item(item).await?;
                enqueued += 1;

                info!(
                    execution_id = %execution_id,
                    node_id = %node_id,
                    "Enqueued node for execution"
                );
            }
        }

        if enqueued > 0 {
            debug!(execution_id = %execution_id, enqueued, "Dispatch complete");
        }

        Ok(())
    }
}

/// Check if a node is runnable: all its predecessors are completed and
/// it hasn't been scheduled yet.
fn is_runnable(
    node_id: &str,
    ir: &WorkflowIr,
    completed: &HashSet<NodeId>,
    scheduled: &HashSet<NodeId>,
) -> bool {
    if scheduled.contains(node_id) || completed.contains(node_id) {
        return false;
    }
    // All predecessors (nodes with an edge TO this node) must be completed.
    ir.edges
        .iter()
        .filter(|e| e.to == node_id)
        .all(|e| completed.contains(&e.from))
}
