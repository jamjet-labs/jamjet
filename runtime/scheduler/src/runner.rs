use jamjet_core::node::{NodeId, NodeKind};
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

                // Build the base payload that every work item carries.
                let mut payload = serde_json::json!({
                    "workflow_id": workflow_id,
                    "workflow_version": workflow_version,
                    "node_id": node_id,
                });
                // For PythonFn nodes, enrich the payload so the external Python
                // worker is self-contained: it needs the module, the function to
                // invoke, and the current workflow state as the call input.
                // `progress.final_state` is the accumulated state (all
                // `state_patch`es from completed nodes so far) and is available
                // here without an additional backend call.
                if let NodeKind::PythonFn {
                    module, function, ..
                } = &node.kind
                {
                    let obj = payload
                        .as_object_mut()
                        .expect("payload is always a JSON object");
                    obj.insert("module".into(), serde_json::Value::String(module.clone()));
                    obj.insert(
                        "function".into(),
                        serde_json::Value::String(function.clone()),
                    );
                    obj.insert(
                        "input".into(),
                        serde_json::Value::Object(progress.final_state.clone()),
                    );
                }

                // For Model nodes, enrich the payload with model configuration and tool
                // schemas so the model worker is self-contained.
                if let NodeKind::Model {
                    model_ref,
                    system_prompt,
                    tools,
                    ..
                } = &node.kind
                {
                    let obj = payload
                        .as_object_mut()
                        .expect("payload is always a JSON object");
                    // Resolve model identifier from the IR's named models map.
                    let model_id = ir
                        .models
                        .get(model_ref.as_str())
                        .map(|cfg| {
                            if cfg.provider.is_empty() {
                                cfg.model.clone()
                            } else {
                                format!("{}/{}", cfg.provider, cfg.model)
                            }
                        })
                        .unwrap_or_else(|| model_ref.clone());
                    obj.insert("model".into(), serde_json::Value::String(model_id));
                    // Optional model config overrides.
                    if let Some(cfg) = ir.models.get(model_ref.as_str()) {
                        if let Some(temp) = cfg.temperature {
                            obj.insert("temperature".into(), serde_json::json!(temp));
                        }
                        if let Some(mt) = cfg.max_tokens {
                            obj.insert("max_tokens".into(), serde_json::json!(mt));
                        }
                    }
                    // Node-level system prompt.
                    if let Some(sp) = system_prompt {
                        obj.insert(
                            "system_prompt".into(),
                            serde_json::Value::String(sp.clone()),
                        );
                    }
                    // Tool schemas — passed to the model so it can emit tool_calls.
                    if !tools.is_empty() {
                        obj.insert("tools".into(), serde_json::Value::Array(tools.clone()));
                    }
                    // Running conversation history — thread the accumulated
                    // `messages` from state into the payload so each turn's model
                    // call sees the full history (the agent loop accumulates them
                    // in `state["messages"]`; the executor builds its ChatMessage
                    // list from this when the node carries no inline `prompt`).
                    // Without this the loop cannot feed tool results forward.
                    if let Some(messages) = progress.final_state.get("messages") {
                        obj.insert("messages".into(), messages.clone());
                    }
                }

                // For Condition nodes, enrich the payload with the branch list
                // and the current workflow state so the condition executor is
                // self-contained.  The executor evaluates each branch's expression
                // against `state` and records the routing decision on NodeCompleted.
                // `progress.final_state` is the accumulated state from all
                // completed nodes so far — exactly what the condition gate reads.
                if let NodeKind::Condition { branches } = &node.kind {
                    if !branches.is_empty() {
                        let obj = payload
                            .as_object_mut()
                            .expect("payload is always a JSON object");
                        obj.insert(
                            "branches".into(),
                            serde_json::to_value(branches).unwrap_or(serde_json::json!([])),
                        );
                        obj.insert(
                            "state".into(),
                            serde_json::Value::Object(progress.final_state.clone()),
                        );
                    }
                }

                let item = WorkItem {
                    id: Uuid::new_v4(),
                    execution_id: execution_id.clone(),
                    node_id: node_id.clone(),
                    queue_type,
                    payload,
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
            EventKind::NodeParked { .. } => {
                // NodeParked is an audit/observability event. The node is
                // already in `self.scheduled` (put there by NodeScheduled) and
                // stays there — the work item is back in `pending` with a
                // future `retry_after`; a worker picks it up when ready.
                // No change to self.scheduled here.
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
            EventKind::WorkflowStarted {
                initial_input: serde_json::Value::Object(input),
                ..
            } => {
                // Seed the running state from the execution's `initial_input` so
                // the FIRST node sees it — e.g. the agent loop's seeded
                // `messages` (system + user) and the `{name: "module:function"}`
                // tool-resolver map, which turn 0's model node and every
                // tool-dispatch node read before any node has produced a
                // `state_patch`. Mirrors the state materializer, which starts
                // `current_state` from `initial_input` before folding patches,
                // so the scheduler's `final_state` stays consistent with the
                // persisted `current_state`. A non-object `initial_input` falls
                // through to the no-op arm (nothing to seed).
                for (k, v) in input {
                    self.final_state.insert(k.clone(), v.clone());
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
                parent_execution_id: None,
                segment_number: 0,
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

    /// A single-node workflow whose only step is a `PythonFn`.
    fn python_fn_ir() -> serde_json::Value {
        serde_json::json!({
            "workflow_id": "wf",
            "version": "0.1.0",
            "name": null,
            "description": null,
            "state_schema": "",
            "start_node": "py_step",
            "nodes": {
                "py_step": {
                    "id": "py_step",
                    "kind": {
                        "type": "python_fn",
                        "module": "myapp.tools",
                        "function": "run_check",
                        "output_schema": ""
                    },
                    "retry_policy": null,
                    "node_timeout_secs": null,
                    "description": null,
                    "labels": {}
                }
            },
            "edges": [],
            "retry_policies": {},
            "timeouts": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {}
        })
    }

    /// The scheduler must enrich a `PythonFn` work-item payload with
    /// `module`, `function`, and `input` so an external Python worker is
    /// self-contained.  The base fields (`workflow_id`, `workflow_version`,
    /// `node_id`) must still be present.
    #[tokio::test]
    async fn python_fn_work_item_payload_is_enriched() {
        let (s, b, e) = setup(python_fn_ir()).await;

        // Seed some accumulated state so we can assert it lands in `input`.
        append(
            &b,
            &e,
            node_completed("prev_node", serde_json::json!({ "key": "value" })),
        )
        .await;

        // Tick: `py_step` is the start node with no predecessors — runnable.
        tick(&s, &e).await;

        // Claim the work item from the python_tool queue.
        let item = b
            .claim_work_item("test-worker", &["python_tool"])
            .await
            .expect("backend claim must not error")
            .expect("a PythonFn node must produce exactly one work item");

        // Queue type
        assert_eq!(item.queue_type, "python_tool");

        // PythonFn-specific enrichment
        assert_eq!(
            item.payload["module"], "myapp.tools",
            "payload must carry the module path"
        );
        assert_eq!(
            item.payload["function"], "run_check",
            "payload must carry the function name"
        );
        assert!(
            item.payload["input"].is_object(),
            "payload.input must be a JSON object (accumulated workflow state)"
        );
        // The state seeded above must be reflected in input.
        assert_eq!(
            item.payload["input"]["key"], "value",
            "payload.input must reflect accumulated state patches"
        );

        // Base fields must still be present.
        assert_eq!(item.payload["workflow_id"], "wf");
        assert_eq!(item.payload["workflow_version"], "0.1.0");
        assert_eq!(item.payload["node_id"], "py_step");
    }

    /// A single-node workflow whose only step is a `Model` node with tools.
    fn model_node_ir() -> serde_json::Value {
        serde_json::json!({
            "workflow_id": "wf",
            "version": "0.1.0",
            "name": null,
            "description": null,
            "state_schema": "",
            "start_node": "model_step",
            "nodes": {
                "model_step": {
                    "id": "model_step",
                    "kind": {
                        "type": "model",
                        "model_ref": "test_model",
                        "prompt_ref": "prompts/test.md",
                        "output_schema": "",
                        "system_prompt": "You are a test assistant.",
                        "tools": [
                            {"type": "function", "function": {"name": "get_weather", "description": "Get the weather", "parameters": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}}},
                            {"type": "function", "function": {"name": "search_web", "description": "Search the web", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}}
                        ]
                    },
                    "retry_policy": null,
                    "node_timeout_secs": null,
                    "description": null,
                    "labels": {}
                }
            },
            "edges": [],
            "retry_policies": {},
            "timeouts": {},
            "models": {
                "test_model": {
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-6",
                    "timeout_secs": null,
                    "retry_policy": null,
                    "temperature": null,
                    "max_tokens": 1024
                }
            },
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {}
        })
    }

    /// The scheduler must enrich a `Model` work-item payload with `model`,
    /// `max_tokens`, `system_prompt`, and `tools` so the model worker is
    /// self-contained.
    #[tokio::test]
    async fn model_node_tools_payload_is_enriched() {
        let (s, b, e) = setup(model_node_ir()).await;

        // Seed some accumulated state.
        append(
            &b,
            &e,
            node_completed("prev_node", serde_json::json!({ "key": "value" })),
        )
        .await;

        // Tick: `model_step` is the start node with no predecessors — runnable.
        tick(&s, &e).await;

        // Claim the work item from the model queue.
        let item = b
            .claim_work_item("test-worker", &["model"])
            .await
            .expect("backend claim must not error")
            .expect("a Model node must produce exactly one work item");

        // Queue type
        assert_eq!(item.queue_type, "model");

        // Model identifier resolved from IR models map.
        assert_eq!(
            item.payload["model"], "anthropic/claude-sonnet-4-6",
            "payload must carry the resolved model identifier"
        );

        // max_tokens from model config.
        assert_eq!(
            item.payload["max_tokens"], 1024,
            "payload must carry max_tokens from model config"
        );

        // system_prompt from node definition.
        assert_eq!(
            item.payload["system_prompt"], "You are a test assistant.",
            "payload must carry the node-level system prompt"
        );

        // Tools array must be present and have the expected length.
        assert!(
            item.payload["tools"].is_array(),
            "payload.tools must be a JSON array"
        );
        assert_eq!(
            item.payload["tools"].as_array().unwrap().len(),
            2,
            "payload.tools must contain all 2 tool schemas"
        );

        // Base fields must still be present.
        assert_eq!(item.payload["workflow_id"], "wf");
        assert_eq!(item.payload["workflow_version"], "0.1.0");
        assert_eq!(item.payload["node_id"], "model_step");
    }

    /// 2j-4 G1a: a `WorkflowStarted` event seeds `final_state` from its
    /// `initial_input` so the first node (turn 0's model node, every
    /// tool-dispatch node) sees the seeded `messages` + tool-resolver `tools`
    /// map before any node has produced a `state_patch`.
    #[tokio::test]
    async fn workflow_started_seeds_final_state_from_initial_input() {
        let progress = fold_events(vec![EventKind::WorkflowStarted {
            workflow_id: "wf".into(),
            workflow_version: "0.1.0".into(),
            initial_input: serde_json::json!({
                "messages": [
                    {"role": "system", "content": "You are helpful."},
                    {"role": "user", "content": "Hi"}
                ],
                "tools": {"get_weather": "myapp.tools:get_weather"}
            }),
        }]);

        assert_eq!(
            progress.final_state["messages"]
                .as_array()
                .expect("seeded messages must be an array")
                .len(),
            2,
            "initial_input.messages must seed final_state.messages"
        );
        assert_eq!(
            progress.final_state["tools"]["get_weather"], "myapp.tools:get_weather",
            "initial_input.tools resolver map must seed final_state.tools"
        );
    }

    /// 2j-4 G1b: the Model work-item payload must carry the running `messages`
    /// threaded from state (seeded here via `initial_input`) so each turn's
    /// model call sees the accumulated conversation history.
    #[tokio::test]
    async fn model_node_payload_carries_messages_from_state() {
        let (s, b, e) = setup(model_node_ir()).await;

        // Seed the running conversation the way `start_execution` does — a
        // WorkflowStarted event whose initial_input carries `messages`.
        append(
            &b,
            &e,
            EventKind::WorkflowStarted {
                workflow_id: "wf".into(),
                workflow_version: "0.1.0".into(),
                initial_input: serde_json::json!({
                    "messages": [
                        {"role": "system", "content": "You are a test assistant."},
                        {"role": "user", "content": "What is the weather in London?"}
                    ]
                }),
            },
        )
        .await;

        tick(&s, &e).await;

        let item = b
            .claim_work_item("test-worker", &["model"])
            .await
            .expect("backend claim must not error")
            .expect("a Model node must produce exactly one work item");

        let messages = item.payload["messages"]
            .as_array()
            .expect("payload.messages must be a JSON array threaded from state");
        assert_eq!(
            messages.len(),
            2,
            "payload.messages must carry the full accumulated conversation"
        );
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "What is the weather in London?");
    }

    /// Condition node work-item payload carries `branches` and committed `state`.
    ///
    /// A Condition node with non-empty branches must have both:
    ///   - `payload["branches"]`: the serialized branch list from the IR
    ///   - `payload["state"]`: the accumulated workflow state at dispatch time
    ///
    /// This is the payload the condition executor (FDL-2) reads to evaluate the route.
    #[tokio::test]
    async fn condition_node_payload_carries_branches_and_state() {
        // Build a 2-node IR:
        //   start (empty-branches Condition — used as a simple pass-through) -> gate
        //   gate  (Condition with non-empty branches)                         -> a | b
        let ir = serde_json::json!({
            "workflow_id": "wf",
            "version": "0.1.0",
            "name": null,
            "description": null,
            "state_schema": "",
            "start_node": "start",
            "nodes": {
                "start": {
                    "id": "start",
                    "kind": { "type": "condition", "branches": [] },
                    "retry_policy": null,
                    "node_timeout_secs": null,
                    "description": null,
                    "labels": {}
                },
                "gate": {
                    "id": "gate",
                    "kind": {
                        "type": "condition",
                        "branches": [
                            {"condition": "state.x == \"stop\"", "target": "end"},
                            {"condition": null, "target": "tools"}
                        ]
                    },
                    "retry_policy": null,
                    "node_timeout_secs": null,
                    "description": null,
                    "labels": {}
                }
            },
            "edges": [
                {"from": "start", "to": "gate", "condition": null},
                {"from": "gate", "to": "end",   "condition": "state.x == \"stop\""},
                {"from": "gate", "to": "tools", "condition": null}
            ],
            "retry_policies": {},
            "timeouts": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {}
        });
        let (s, b, e) = setup(ir).await;

        // Tick 1: schedules `start` (no predecessors).
        tick(&s, &e).await;

        // Drain the `start` work item so it is no longer in the queue.
        // Without this the next claim_work_item would return `start` instead of
        // `gate` (work items are returned in FIFO order).
        let _start_item = b
            .claim_work_item("test-worker-a", &["general"])
            .await
            .expect("claim start must not error")
            .expect("start work item must be present");

        // Simulate `start` completing with some workflow state.
        let patch = serde_json::json!({ "x": "stop", "other": 42 });
        append(&b, &e, node_completed("start", patch)).await;

        // Tick 2: `gate` is now runnable; the scheduler must enrich its payload.
        tick(&s, &e).await;

        // Claim from the general queue (Condition nodes use QueueType::General).
        let item = b
            .claim_work_item("test-worker-b", &["general"])
            .await
            .expect("backend claim must not error")
            .expect("gate Condition node must produce exactly one work item");

        // The claimed item is for `gate`, not `start` (which has no branches).
        assert_eq!(
            item.node_id, "gate",
            "claimed item must be for the gate node"
        );

        // payload["branches"] must carry the two branches from the IR.
        let branches = item.payload["branches"]
            .as_array()
            .expect("payload.branches must be a JSON array");
        assert_eq!(branches.len(), 2, "both branches must be in the payload");
        assert_eq!(
            branches[0]["target"], "end",
            "first branch target must be 'end'"
        );
        assert!(
            !branches[0]["condition"].is_null(),
            "first branch must have a condition expression"
        );
        assert_eq!(
            branches[1]["target"], "tools",
            "second branch target must be 'tools'"
        );
        assert!(
            branches[1]["condition"].is_null(),
            "second branch must be the default (condition: null)"
        );

        // payload["state"] must carry the accumulated workflow state.
        let state = &item.payload["state"];
        assert_eq!(
            state["x"], "stop",
            "payload.state must carry the state_patch from 'start'"
        );
        assert_eq!(
            state["other"], 42,
            "payload.state must carry all state keys"
        );
    }
}
