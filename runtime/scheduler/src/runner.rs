use jamjet_core::node::NodeId;
use jamjet_core::workflow::ExecutionId;
use jamjet_ir::WorkflowIr;
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::event::EventKind;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

/// Configuration for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often the scheduler polls for runnable nodes.
    pub poll_interval: Duration,
    /// Maximum number of nodes that may be simultaneously active (scheduled or
    /// running) per execution. Prevents a single large workflow from
    /// monopolising all workers. Default: 16.
    pub max_concurrent_nodes_per_execution: usize,
    /// Maximum number of new nodes dispatched per execution per scheduler tick.
    /// Provides backpressure so a burst of runnable nodes doesn't saturate the
    /// queue all at once. Default: 8.
    pub max_dispatch_per_tick: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(500),
            max_concurrent_nodes_per_execution: 16,
            max_dispatch_per_tick: 8,
        }
    }
}

/// The JamJet scheduler drives workflow execution.
///
/// It runs as a Tokio async loop, detecting runnable nodes and dispatching
/// them to the worker queue.
pub struct Scheduler {
    backend: Arc<dyn StateBackend>,
    config: SchedulerConfig,
    /// Cache of deserialized workflow IR, keyed by (workflow_id, version).
    /// Registered IR is immutable per version, so this avoids re-fetching and
    /// re-deserializing it on every tick for every running execution.
    ir_cache: Mutex<HashMap<(String, String), Arc<WorkflowIr>>>,
    /// Per-execution scheduling progress, folded incrementally from the event
    /// log. Each tick reads only events appended since the last tick instead of
    /// replaying the whole log. The entry is dropped when the execution reaches
    /// a terminal state.
    progress: Mutex<HashMap<ExecutionId, ExecProgress>>,
}

impl Scheduler {
    pub fn new(backend: Arc<dyn StateBackend>) -> Self {
        Self {
            backend,
            config: SchedulerConfig::default(),
            ir_cache: Mutex::new(HashMap::new()),
            progress: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_config(mut self, config: SchedulerConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.config.poll_interval = interval;
        self
    }

    /// Run the scheduler loop. This runs indefinitely until the future is cancelled.
    pub async fn run(&self) {
        info!(
            "Scheduler started (poll_interval={:?}, max_concurrent={}, max_dispatch_per_tick={})",
            self.config.poll_interval,
            self.config.max_concurrent_nodes_per_execution,
            self.config.max_dispatch_per_tick,
        );
        loop {
            if let Err(e) = self.tick().await {
                warn!("Scheduler tick error: {e}");
            }
            tokio::time::sleep(self.config.poll_interval).await;
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
        // Load the workflow IR, using a cache keyed by (workflow_id, version).
        // Registered IR is immutable per version, so we deserialize it once
        // rather than on every tick for every running execution.
        let cache_key = (workflow_id.to_string(), workflow_version.to_string());
        let cached = self.ir_cache.lock().unwrap().get(&cache_key).cloned();
        let ir: Arc<WorkflowIr> = match cached {
            Some(ir) => ir,
            None => {
                let def = self
                    .backend
                    .get_workflow(workflow_id, workflow_version)
                    .await?;
                let Some(def) = def else {
                    warn!(%workflow_id, %workflow_version, "Workflow definition not found; cannot schedule");
                    return Ok(());
                };
                let ir = Arc::new(serde_json::from_value::<WorkflowIr>(def.ir)?);
                self.ir_cache
                    .lock()
                    .unwrap()
                    .insert(cache_key, Arc::clone(&ir));
                ir
            }
        };

        // Fold only the events appended since the last tick into the cached
        // progress for this execution. Completed/failed nodes never revert and
        // the IR is immutable, so replaying the whole event log every tick is
        // pure overhead that grows with execution length — we apply just the
        // delta instead, keeping a tick's cost bounded by the graph size.
        let mut progress = self
            .progress
            .lock()
            .unwrap()
            .get(execution_id)
            .cloned()
            .unwrap_or_default();

        let new_events = self
            .backend
            .get_events_since(execution_id, progress.last_sequence)
            .await?;
        for event in &new_events {
            progress.apply(event);
        }

        // Write the advanced progress back before any early return below, so a
        // tick that bails out (e.g. at the concurrency cap) doesn't have to
        // re-read the same delta next time.
        self.progress
            .lock()
            .unwrap()
            .insert(execution_id.clone(), progress.clone());

        // Fail nodes whose approval was rejected. Guarded by terminal_failed so
        // the emission fires exactly once: the next tick folds the NodeFailed,
        // which clears the `rejected` marker and inserts into terminal_failed.
        for (node_id, reason) in progress
            .rejected
            .iter()
            .filter(|(n, _)| !progress.terminal_failed.contains(n.as_str()))
            .map(|(n, r)| (n.clone(), r.clone()))
            .collect::<Vec<_>>()
        {
            let seq = self.backend.latest_sequence(execution_id).await? + 1;
            self.backend
                .append_event(jamjet_state::Event::new(
                    execution_id.clone(),
                    seq,
                    EventKind::NodeFailed {
                        node_id: node_id.clone(),
                        error: reason,
                        attempt: 0,
                        retryable: false,
                    },
                ))
                .await?;
            info!(execution_id = %execution_id, node_id = %node_id, "Approval rejected — node failed");
        }

        let completed = &progress.completed;
        let scheduled = &progress.scheduled;
        let terminal_failed = &progress.terminal_failed;

        debug!(
            execution_id = %execution_id,
            completed_nodes = completed.len(),
            scheduled_nodes = scheduled.len(),
            terminal_failed_nodes = terminal_failed.len(),
            held_nodes = progress.held.len(),
            "Checking for runnable nodes"
        );

        // Concurrency limit: if this execution is already at the cap, skip dispatch.
        if scheduled.len() >= self.config.max_concurrent_nodes_per_execution {
            debug!(
                execution_id = %execution_id,
                active = scheduled.len(),
                limit = self.config.max_concurrent_nodes_per_execution,
                "Concurrency limit reached — skipping dispatch"
            );
            return Ok(());
        }

        // Find nodes that are runnable and enqueue them.
        let mut enqueued = 0usize;
        for (node_id, node) in &ir.nodes {
            // Backpressure: stop dispatching if we've hit the per-tick cap or the
            // concurrency limit (accounting for nodes dispatched this tick).
            if enqueued >= self.config.max_dispatch_per_tick {
                debug!(
                    execution_id = %execution_id,
                    dispatched = enqueued,
                    "Per-tick dispatch limit reached — deferring remaining nodes"
                );
                break;
            }
            if scheduled.len() + enqueued >= self.config.max_concurrent_nodes_per_execution {
                break;
            }
            if terminal_failed.contains(node_id.as_str()) {
                continue; // permanently failed
            }
            if is_runnable(node_id, &ir, completed, scheduled) {
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
                    lease_fence: 0,
                    tenant_id: jamjet_state::DEFAULT_TENANT.to_string(),
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

        // Completion detection: if nothing is in flight (no scheduled/running nodes)
        // and nothing new was dispatched this tick, the execution has reached a
        // terminal state. Emit the terminal event and flip the stored status so the
        // execution drops out of the running set on the next tick (fires once).
        if enqueued == 0 && scheduled.is_empty() {
            let (status, event_kind) = if terminal_failed.is_empty() {
                (
                    jamjet_core::workflow::WorkflowStatus::Completed,
                    EventKind::WorkflowCompleted {
                        final_state: serde_json::Value::Object(progress.final_state.clone()),
                    },
                )
            } else {
                let mut failed: Vec<String> =
                    terminal_failed.iter().map(|n| n.to_string()).collect();
                failed.sort();
                (
                    jamjet_core::workflow::WorkflowStatus::Failed,
                    EventKind::WorkflowFailed {
                        error: format!("node(s) failed terminally: {}", failed.join(", ")),
                    },
                )
            };

            let seq = self.backend.latest_sequence(execution_id).await? + 1;
            self.backend
                .append_event(jamjet_state::Event::new(
                    execution_id.clone(),
                    seq,
                    event_kind,
                ))
                .await?;
            info!(execution_id = %execution_id, ?status, "Execution reached terminal state");
            self.backend
                .update_execution_status(execution_id, status)
                .await?;
            // Execution is terminal; drop its progress so the cache doesn't grow
            // without bound as executions complete.
            self.progress.lock().unwrap().remove(execution_id);
        }

        Ok(())
    }
}

/// Incrementally-folded scheduling state for a single execution.
///
/// Rebuilt lazily from the event log: each scheduler tick reads only events with
/// a sequence greater than `last_sequence` and folds them in via [`ExecProgress::apply`].
/// This makes a tick's cost a function of the workflow's graph size rather than
/// the ever-growing length of its event log.
#[derive(Default, Clone)]
struct ExecProgress {
    completed: HashSet<NodeId>,
    scheduled: HashSet<NodeId>,
    terminal_failed: HashSet<NodeId>,
    /// `state_patch`es from completed nodes, merged in sequence order. This is
    /// the `final_state` reported on `WorkflowCompleted`.
    final_state: serde_json::Map<String, serde_json::Value>,
    /// Highest event sequence already folded in.
    last_sequence: jamjet_state::EventSequence,
    /// Nodes awaiting a human approval decision. This set does NOT keep the
    /// node parked — that comes from the node remaining in `scheduled` (the
    /// dispatcher won't re-enqueue it). `held` only gates which
    /// `ApprovalReceived` decisions are acted on.
    held: HashSet<NodeId>,
    /// Rejected approvals awaiting their `NodeFailed` emission, node -> reason.
    /// Cleared when the corresponding `NodeFailed{retryable: false}` folds in.
    /// Note: a rejected node stays in `scheduled` (consuming a concurrency slot)
    /// until the follow-up `NodeFailed` emission clears it.
    rejected: HashMap<NodeId, String>,
}

impl ExecProgress {
    /// Fold one event into the running state. Must be applied in sequence order.
    fn apply(&mut self, event: &jamjet_state::Event) {
        match &event.kind {
            EventKind::NodeCompleted {
                node_id,
                state_patch,
                ..
            } => {
                self.completed.insert(node_id.clone());
                self.scheduled.remove(node_id);
                // Stale-hold cleanup: a completed node no longer awaits approval.
                self.held.remove(node_id);
                if let serde_json::Value::Object(patch) = state_patch {
                    for (k, v) in patch {
                        self.final_state.insert(k.clone(), v.clone());
                    }
                }
            }
            EventKind::NodeSkipped { node_id, .. } => {
                self.completed.insert(node_id.clone());
                self.scheduled.remove(node_id);
                // Stale-hold cleanup: a skipped node no longer awaits approval.
                self.held.remove(node_id);
            }
            EventKind::NodeScheduled { node_id, .. } | EventKind::NodeStarted { node_id, .. } => {
                self.scheduled.insert(node_id.clone());
            }
            EventKind::NodeCancelled { node_id } => {
                self.completed.insert(node_id.clone());
                self.scheduled.remove(node_id);
                self.held.remove(node_id);
                self.rejected.remove(node_id);
            }
            EventKind::NodeFailed {
                node_id,
                retryable: false,
                ..
            } => {
                self.terminal_failed.insert(node_id.clone());
                self.scheduled.remove(node_id);
                self.rejected.remove(node_id);
                self.held.remove(node_id);
            }
            EventKind::NodeFailed {
                node_id,
                retryable: true,
                ..
            } => {
                // Re-queued by the subsequent RetryScheduled event.
                self.scheduled.remove(node_id);
                // Deliberately do NOT clear `held`: a lease reclaim can fire
                // while the node is parked for approval, and the re-claimed
                // worker settles quietly without re-emitting
                // ToolApprovalRequired. Clearing the hold here would drop the
                // eventual ApprovalReceived decision and wedge the execution.
            }
            EventKind::RetryScheduled { node_id, .. } => {
                self.scheduled.insert(node_id.clone());
            }
            EventKind::ToolApprovalRequired { node_id, .. } => {
                self.held.insert(node_id.clone());
            }
            EventKind::ApprovalReceived {
                node_id,
                user_id,
                decision,
                comment,
                ..
            } => {
                // Only act on decisions that resolve an actual hold; the API
                // validates, but the fold must stay total and tolerant.
                if self.held.remove(node_id) {
                    match decision {
                        jamjet_state::event::ApprovalDecision::Approved => {
                            // Free the slot so is_runnable() re-dispatches
                            // through the normal path.
                            self.scheduled.remove(node_id);
                        }
                        jamjet_state::event::ApprovalDecision::Rejected => {
                            let mut reason = format!("approval rejected by {user_id}");
                            if let Some(c) = comment {
                                reason.push_str(": ");
                                reason.push_str(c);
                            }
                            self.rejected.insert(node_id.clone(), reason);
                        }
                    }
                }
            }
            _ => {}
        }
        self.last_sequence = self.last_sequence.max(event.sequence);
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

#[cfg(test)]
mod tests {
    use super::*;
    use jamjet_core::workflow::{WorkflowExecution, WorkflowStatus};
    use jamjet_state::backend::WorkflowDefinition;
    use jamjet_state::{Event, InMemoryBackend, DEFAULT_TENANT};

    /// A linear two-node workflow: `a -> b -> end`, with `a` as the start node.
    /// Both are condition nodes (the scheduler dispatches purely off edges, so the
    /// concrete kind doesn't matter for these tests).
    fn linear_ir() -> serde_json::Value {
        let node = |id: &str| {
            serde_json::json!({
                "id": id,
                "kind": { "type": "condition", "branches": [] },
                "retry_policy": null,
                "node_timeout_secs": null,
                "description": null,
                "labels": {}
            })
        };
        serde_json::json!({
            "workflow_id": "wf",
            "version": "0.1.0",
            "name": null,
            "description": null,
            "state_schema": "",
            "start_node": "a",
            "nodes": { "a": node("a"), "b": node("b") },
            "edges": [
                { "from": "a", "to": "b", "condition": null },
                { "from": "b", "to": "end", "condition": null }
            ],
            "retry_policies": {},
            "timeouts": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {}
        })
    }

    async fn setup(ir: serde_json::Value) -> (Scheduler, Arc<dyn StateBackend>, ExecutionId) {
        let backend: Arc<dyn StateBackend> = Arc::new(InMemoryBackend::new());
        backend
            .store_workflow(WorkflowDefinition {
                workflow_id: "wf".into(),
                version: "0.1.0".into(),
                ir,
                created_at: chrono::Utc::now(),
                tenant_id: DEFAULT_TENANT.into(),
            })
            .await
            .unwrap();
        let exec_id = ExecutionId::new();
        let now = chrono::Utc::now();
        backend
            .create_execution(WorkflowExecution {
                execution_id: exec_id.clone(),
                workflow_id: "wf".into(),
                workflow_version: "0.1.0".into(),
                status: WorkflowStatus::Running,
                initial_input: serde_json::json!({}),
                current_state: serde_json::json!({}),
                started_at: now,
                updated_at: now,
                completed_at: None,
                session_type: None,
            })
            .await
            .unwrap();
        (Scheduler::new(backend.clone()), backend, exec_id)
    }

    async fn tick(s: &Scheduler, e: &ExecutionId) {
        s.schedule_runnable_nodes(e, "wf", "0.1.0").await.unwrap();
    }

    /// Append an event the way a worker would: at the next per-execution sequence.
    async fn append(b: &Arc<dyn StateBackend>, e: &ExecutionId, kind: EventKind) {
        let seq = b.latest_sequence(e).await.unwrap() + 1;
        b.append_event(Event::new(e.clone(), seq, kind))
            .await
            .unwrap();
    }

    async fn status(b: &Arc<dyn StateBackend>, e: &ExecutionId) -> WorkflowStatus {
        b.get_execution(e).await.unwrap().unwrap().status
    }

    fn scheduled_nodes(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|ev| match &ev.kind {
                EventKind::NodeScheduled { node_id, .. } => Some(node_id.to_string()),
                _ => None,
            })
            .collect()
    }

    fn node_completed(node_id: &str, patch: serde_json::Value) -> EventKind {
        EventKind::NodeCompleted {
            node_id: node_id.into(),
            output: serde_json::json!({}),
            state_patch: patch,
            duration_ms: 1,
            gen_ai_system: None,
            gen_ai_model: None,
            input_tokens: None,
            output_tokens: None,
            finish_reason: None,
            cost_usd: None,
            provenance: None,
            idempotency_key: None,
        }
    }

    #[tokio::test]
    async fn schedules_in_dependency_order_and_completes() {
        let (s, b, e) = setup(linear_ir()).await;

        // Tick 1: only `a` (no predecessors) is runnable.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(
            scheduled_nodes(&evs).contains(&"a".to_string()),
            "a should be scheduled first"
        );
        assert!(
            !scheduled_nodes(&evs).contains(&"b".to_string()),
            "b must wait for its predecessor a"
        );
        assert_eq!(status(&b, &e).await, WorkflowStatus::Running);

        // Worker finishes `a`; tick 2 makes `b` runnable.
        append(&b, &e, node_completed("a", serde_json::json!({ "x": 1 }))).await;
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(
            scheduled_nodes(&evs).contains(&"b".to_string()),
            "b should be scheduled once a completes"
        );
        assert_eq!(status(&b, &e).await, WorkflowStatus::Running);
        assert!(
            s.progress.lock().unwrap().contains_key(&e),
            "progress should be cached while the execution is running"
        );

        // Worker finishes `b`; tick 3 detects completion.
        append(&b, &e, node_completed("b", serde_json::json!({ "y": 2 }))).await;
        tick(&s, &e).await;
        assert_eq!(status(&b, &e).await, WorkflowStatus::Completed);
        assert!(
            !s.progress.lock().unwrap().contains_key(&e),
            "progress should be dropped once the execution is terminal"
        );

        // Final state merges both nodes' patches in sequence order.
        let evs = b.get_events(&e).await.unwrap();
        let final_state = evs
            .iter()
            .find_map(|ev| match &ev.kind {
                EventKind::WorkflowCompleted { final_state } => Some(final_state.clone()),
                _ => None,
            })
            .expect("WorkflowCompleted should be emitted");
        assert_eq!(final_state, serde_json::json!({ "x": 1, "y": 2 }));
    }

    #[tokio::test]
    async fn terminal_node_failure_fails_the_workflow() {
        let (s, b, e) = setup(linear_ir()).await;

        tick(&s, &e).await; // schedules `a`
        append(
            &b,
            &e,
            EventKind::NodeFailed {
                node_id: "a".into(),
                error: "boom".into(),
                attempt: 1,
                retryable: false,
            },
        )
        .await;

        // `a` is terminally failed, `b` can never run, nothing is in flight.
        tick(&s, &e).await;
        assert_eq!(status(&b, &e).await, WorkflowStatus::Failed);
    }

    fn fold_events(kinds: Vec<EventKind>) -> ExecProgress {
        let exec_id = ExecutionId::new();
        let mut progress = ExecProgress::default();
        for (i, kind) in kinds.into_iter().enumerate() {
            progress.apply(&Event::new(exec_id.clone(), (i + 1) as i64, kind));
        }
        progress
    }

    #[test]
    fn approval_required_marks_node_held() {
        let progress = fold_events(vec![
            EventKind::NodeScheduled {
                node_id: "a".into(),
                queue_type: "general".into(),
            },
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
        ]);
        assert!(progress.held.contains("a"));
        // Still in scheduled: nothing must re-enqueue a held node.
        assert!(progress.scheduled.contains("a"));
    }

    #[test]
    fn approval_approved_unblocks_node_for_rescheduling() {
        let progress = fold_events(vec![
            EventKind::NodeScheduled {
                node_id: "a".into(),
                queue_type: "general".into(),
            },
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
            EventKind::ApprovalReceived {
                node_id: "a".into(),
                user_id: "u".into(),
                decision: jamjet_state::event::ApprovalDecision::Approved,
                comment: None,
                state_patch: None,
            },
        ]);
        assert!(!progress.held.contains("a"));
        // Removed from scheduled so is_runnable() can re-dispatch it.
        assert!(!progress.scheduled.contains("a"));
        assert!(progress.rejected.is_empty());
    }

    #[test]
    fn approval_rejected_marks_node_for_failure() {
        let progress = fold_events(vec![
            EventKind::NodeScheduled {
                node_id: "a".into(),
                queue_type: "general".into(),
            },
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
            EventKind::ApprovalReceived {
                node_id: "a".into(),
                user_id: "alice".into(),
                decision: jamjet_state::event::ApprovalDecision::Rejected,
                comment: Some("too risky".into()),
                state_patch: None,
            },
        ]);
        assert!(!progress.held.contains("a"));
        let reason = progress.rejected.get("a").expect("rejected entry");
        assert!(reason.contains("alice"));
        assert!(reason.contains("too risky"));
    }

    #[test]
    fn node_failed_clears_rejected_marker() {
        let progress = fold_events(vec![
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
            EventKind::ApprovalReceived {
                node_id: "a".into(),
                user_id: "u".into(),
                decision: jamjet_state::event::ApprovalDecision::Rejected,
                comment: None,
                state_patch: None,
            },
            EventKind::NodeFailed {
                node_id: "a".into(),
                error: "approval rejected".into(),
                attempt: 0,
                retryable: false,
            },
        ]);
        assert!(progress.rejected.is_empty());
        assert!(progress.terminal_failed.contains("a"));
    }

    #[test]
    fn approval_decision_without_hold_is_ignored() {
        let progress = fold_events(vec![EventKind::ApprovalReceived {
            node_id: "ghost".into(),
            user_id: "u".into(),
            decision: jamjet_state::event::ApprovalDecision::Approved,
            comment: None,
            state_patch: None,
        }]);
        assert!(progress.held.is_empty());
        assert!(progress.rejected.is_empty());
        assert!(progress.scheduled.is_empty());
    }

    #[tokio::test]
    async fn rejected_approval_fails_node_and_workflow() {
        let (s, b, e) = setup(linear_ir()).await;

        // Seed: node "a" scheduled, held, then rejected.
        append(
            &b,
            &e,
            EventKind::NodeScheduled {
                node_id: "a".into(),
                queue_type: "general".into(),
            },
        )
        .await;
        append(
            &b,
            &e,
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
        )
        .await;
        append(
            &b,
            &e,
            EventKind::ApprovalReceived {
                node_id: "a".into(),
                user_id: "alice".into(),
                decision: jamjet_state::event::ApprovalDecision::Rejected,
                comment: Some("nope".into()),
                state_patch: None,
            },
        )
        .await;

        // Tick 1: scheduler folds the rejection and emits NodeFailed.
        tick(&s, &e).await;

        let evs = b.get_events(&e).await.unwrap();
        let failed: Vec<_> = evs
            .iter()
            .filter(|ev| {
                matches!(
                    &ev.kind,
                    EventKind::NodeFailed { node_id, retryable: false, .. } if node_id == "a"
                )
            })
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "exactly one NodeFailed for the rejected node after tick 1"
        );
        if let EventKind::NodeFailed { error, .. } = &failed[0].kind {
            assert!(
                error.contains("alice"),
                "reason must include decider: {error}"
            );
            assert!(
                error.contains("nope"),
                "reason must include comment: {error}"
            );
        }

        // Tick 2: folds the NodeFailed, detects terminal state, emits WorkflowFailed.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(
            evs.iter()
                .any(|ev| matches!(ev.kind, EventKind::WorkflowFailed { .. })),
            "WorkflowFailed must be emitted"
        );
        assert_eq!(status(&b, &e).await, WorkflowStatus::Failed);

        // Tick 3: execution is terminal — must not duplicate NodeFailed.
        // The execution is no longer Running so tick() won't visit it,
        // but we call schedule_runnable_nodes directly to prove idempotence.
        s.schedule_runnable_nodes(&e, "wf", "0.1.0").await.unwrap();
        let evs = b.get_events(&e).await.unwrap();
        let count = evs
            .iter()
            .filter(|ev| matches!(&ev.kind, EventKind::NodeFailed { .. }))
            .count();
        assert_eq!(
            count, 1,
            "NodeFailed must not be duplicated on subsequent ticks"
        );
    }

    #[test]
    fn reclaimed_hold_still_resumes_on_approval() {
        // Worker crashes mid-hold: the lease reclaim emits a retryable
        // NodeFailed, the retry is re-queued, and the re-claimed worker sees
        // the approval still Pending so it settles quietly — emitting nothing.
        // The hold must survive all of that so the eventual approval still
        // resumes the node instead of being silently dropped.
        let progress = fold_events(vec![
            EventKind::NodeScheduled {
                node_id: "a".into(),
                queue_type: "general".into(),
            },
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
            EventKind::NodeFailed {
                node_id: "a".into(),
                error: "worker crashed mid-hold".into(),
                attempt: 0,
                retryable: true,
            },
            EventKind::RetryScheduled {
                node_id: "a".into(),
                attempt: 1,
                delay_ms: 1000,
            },
            EventKind::ApprovalReceived {
                node_id: "a".into(),
                user_id: "u".into(),
                decision: jamjet_state::event::ApprovalDecision::Approved,
                comment: None,
                state_patch: None,
            },
        ]);
        assert!(
            progress.held.is_empty(),
            "hold must be consumed by the approval"
        );
        assert!(
            !progress.scheduled.contains("a"),
            "approved node must leave `scheduled` so it is re-dispatchable"
        );
        assert!(progress.rejected.is_empty(), "rejected must be empty");
    }

    #[test]
    fn reclaimed_hold_still_fails_on_rejection() {
        let progress = fold_events(vec![
            EventKind::NodeScheduled {
                node_id: "a".into(),
                queue_type: "general".into(),
            },
            EventKind::ToolApprovalRequired {
                node_id: "a".into(),
                tool_name: "t".into(),
                approver: "human".into(),
                context: serde_json::json!({}),
            },
            EventKind::NodeFailed {
                node_id: "a".into(),
                error: "worker crashed mid-hold".into(),
                attempt: 0,
                retryable: true,
            },
            EventKind::RetryScheduled {
                node_id: "a".into(),
                attempt: 1,
                delay_ms: 1000,
            },
            EventKind::ApprovalReceived {
                node_id: "a".into(),
                user_id: "u".into(),
                decision: jamjet_state::event::ApprovalDecision::Rejected,
                comment: Some("no".into()),
                state_patch: None,
            },
        ]);
        let reason = progress.rejected.get("a").expect("rejected entry for a");
        assert!(
            reason.contains("u"),
            "reason must include decider: {reason}"
        );
        assert!(
            reason.contains("no"),
            "reason must include comment: {reason}"
        );
        assert!(
            progress.held.is_empty(),
            "hold must be consumed by the rejection"
        );
    }
}
