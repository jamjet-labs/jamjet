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

        // FDL-3 skip cascade (the dead-edge fixpoint). A node whose EVERY
        // in-edge is dead can never run, so emit `NodeSkipped` for it. The skip
        // folds into `completed` (satisfying downstream AND-joins without it) and
        // into `skipped` (making this node's OUT-edges dead), which can make MORE
        // nodes skippable — so iterate to a fixpoint. A node reachable via ANY
        // live in-edge is never all-dead, so shared joins (the agent loop's
        // `end`, a `__finalize__` join) survive; only a dead branch's exclusive
        // tail is skipped. Capped at node-count passes defensively — each pass
        // skips >= 1 new node or stops, so it converges well within the cap.
        let max_passes = ir.nodes.len() + 1;
        for _ in 0..max_passes {
            // Collect the nodes that become skippable in this pass against the
            // current progress, then emit + fold them. A node is skippable iff
            // it is not already resolved, has >= 1 in-edge, and ALL its in-edges
            // are dead. (Roots — no in-edges — are never skipped here.)
            let mut newly_skippable: Vec<NodeId> = Vec::new();
            for node_id in ir.nodes.keys() {
                let n = node_id.as_str();
                if progress.completed.contains(n)
                    || progress.scheduled.contains(n)
                    || progress.skipped.contains(n)
                    || progress.terminal_failed.contains(n)
                {
                    continue;
                }
                let mut has_in_edge = false;
                let mut all_dead = true;
                for e in ir.edges.iter().filter(|e| e.to == *node_id) {
                    has_in_edge = true;
                    if !edge_dead(
                        &e.from,
                        n,
                        &ir,
                        &progress.completed,
                        &progress.skipped,
                        &progress.routes,
                    ) {
                        all_dead = false;
                        break;
                    }
                }
                if has_in_edge && all_dead {
                    newly_skippable.push(node_id.clone());
                }
            }
            if newly_skippable.is_empty() {
                break;
            }
            // Sort for deterministic event sequencing (ir.nodes is a HashMap).
            newly_skippable.sort();
            for node_id in newly_skippable {
                let seq = self.backend.latest_sequence(execution_id).await? + 1;
                let skip_event = jamjet_state::Event::new(
                    execution_id.clone(),
                    seq,
                    EventKind::NodeSkipped {
                        node_id: node_id.clone(),
                        reason: "branch not taken".to_string(),
                    },
                );
                self.backend.append_event(skip_event.clone()).await?;
                // Fold the skip into LOCAL progress immediately so the next pass
                // (and this tick's dispatch) sees it — reaching the fixpoint
                // within one tick rather than one skip per tick.
                progress.apply(&skip_event);
                info!(
                    execution_id = %execution_id,
                    node_id = %node_id,
                    "Skipped node (dead branch — all in-edges dead)"
                );
            }
        }

        // Persist the cascade's advance (skips + last_sequence) so an early
        // return below (e.g. the concurrency cap) doesn't replay the same delta.
        self.progress
            .lock()
            .unwrap()
            .insert(execution_id.clone(), progress.clone());

        let completed = &progress.completed;
        let scheduled = &progress.scheduled;
        let skipped = &progress.skipped;
        let routes = &progress.routes;
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
            if is_runnable(node_id, &ir, completed, scheduled, skipped, routes) {
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

                // For JavaFn nodes, enrich the payload so the external Java
                // tool-worker is self-contained — exactly mirroring the PythonFn
                // arm above, bar the language-appropriate dispatch coordinates:
                // it needs the class to reflect, the method to invoke, and the
                // current workflow state as the call input. The Java worker
                // claims these `java_tool` items over the same HTTP claim/complete
                // API the Python worker uses, so the contract is identical.
                if let NodeKind::JavaFn {
                    class_name, method, ..
                } = &node.kind
                {
                    let obj = payload
                        .as_object_mut()
                        .expect("payload is always a JSON object");
                    obj.insert(
                        "class".into(),
                        serde_json::Value::String(class_name.clone()),
                    );
                    obj.insert("method".into(), serde_json::Value::String(method.clone()));
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
    /// FDL-3: a Condition node's recorded route — condition node id -> the single
    /// chosen branch target. Populated when a Condition `NodeCompleted` carries a
    /// non-null `chosen_target` (the condition executor's routing decision). The
    /// route is a deterministic function of committed state recorded once at the
    /// condition's completion, so replay reproduces the exact route.
    ///
    /// A routing Condition that completed with a NULL `chosen_target` (no branch
    /// matched and no default) records NO entry here; the dead-edge predicate
    /// then reads `routes.get(cond) == None` for a completed routing Condition as
    /// "every out-edge is dead" — so all its branch targets are skipped.
    routes: HashMap<NodeId, NodeId>,
    /// FDL-3: nodes that were emitted as `NodeSkipped` (the dead-branch tails).
    /// A skipped node also folds into `completed` (so a downstream AND-join is
    /// satisfied without it), but is tracked separately here so the dead-edge
    /// predicate can treat a skipped node's OUT-edges as dead — which is what
    /// cascades a skip down a fully-dead branch to its exclusive tail.
    skipped: HashSet<NodeId>,
}

impl ExecProgress {
    /// Fold one event into the running state. Must be applied in sequence order.
    fn apply(&mut self, event: &jamjet_state::Event) {
        match &event.kind {
            EventKind::NodeCompleted {
                node_id,
                output,
                state_patch,
                ..
            } => {
                self.completed.insert(node_id.clone());
                self.scheduled.remove(node_id);
                // Stale-hold cleanup: a completed node no longer awaits approval.
                self.held.remove(node_id);
                // FDL-3: record a Condition node's routing decision. The condition
                // executor (FDL-2) records `chosen_target` on its `output`: a
                // non-null string is the single live out-edge; a null/absent
                // target (no branch matched, no default) records no route so the
                // dead-edge predicate treats every out-edge of this completed
                // Condition as dead. A spurious `chosen_target` on a NON-Condition
                // node's output is harmless: the dead-edge predicate only consults
                // `routes` for nodes the IR confirms are Condition nodes with
                // non-empty branches.
                if let Some(target) = output.get("chosen_target").and_then(|v| v.as_str()) {
                    self.routes.insert(node_id.clone(), target.to_string());
                }
                if let serde_json::Value::Object(patch) = state_patch {
                    for (k, v) in patch {
                        self.final_state.insert(k.clone(), v.clone());
                    }
                }
            }
            EventKind::NodeSkipped { node_id, .. } => {
                // A skipped node folds into `completed` (satisfying downstream
                // AND-joins) AND into `skipped` (so its OUT-edges read as dead in
                // the dead-edge predicate, cascading the skip down the branch).
                self.completed.insert(node_id.clone());
                self.skipped.insert(node_id.clone());
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

/// FDL-3: is the edge `src -> dst` DEAD (i.e. it must never enable `dst`)?
///
/// An edge is dead iff EITHER:
///   - `src` was skipped — a skipped node's out-edges cannot enable anything
///     (this is what cascades a skip down a fully-dead branch); OR
///   - `src` is a routing Condition (a `Condition` node with NON-EMPTY
///     `branches`) that has COMPLETED and routed somewhere other than `dst`.
///     A completed routing Condition routes to exactly one target — the live
///     out-edge — so every other out-edge is dead. The null-route case (no
///     branch matched, no default) records no entry in `routes`, so
///     `routes.get(src)` is `None`, which differs from `Some(dst)` for every
///     `dst`: ALL its out-edges are dead.
///
/// Edges that are LIVE (this returns false):
///   - a routing Condition that has NOT YET completed — its out-edges are
///     pending, not dead (the route is unknown until it runs);
///   - an empty-`branches` Condition (a generic pass-through) — never routes,
///     so its out-edges are governed only by the skip rule (backward-compat);
///   - any non-Condition source that has not been skipped.
fn edge_dead(
    src: &str,
    dst: &str,
    ir: &WorkflowIr,
    completed: &HashSet<NodeId>,
    skipped: &HashSet<NodeId>,
    routes: &HashMap<NodeId, NodeId>,
) -> bool {
    if skipped.contains(src) {
        return true;
    }
    if let Some(node) = ir.nodes.get(src) {
        if let NodeKind::Condition { branches } = &node.kind {
            if !branches.is_empty() && completed.contains(src) {
                // Completed routing Condition: live edge is `routes[src]` only.
                return routes.get(src).map(String::as_str) != Some(dst);
            }
        }
    }
    false
}

/// Check if a node is runnable. Route-aware AND-join over LIVE edges only.
///
/// A node runs iff it is not already completed/scheduled/skipped, it has at
/// least one LIVE in-edge (an in-edge `src -> node` that is not [`edge_dead`]),
/// and EVERY live in-edge's source has COMPLETED. A node with NO in-edges is a
/// root — runnable until it has run (matches the original topology behaviour).
///
/// A node whose every in-edge is dead is NOT runnable here (it is a skip
/// candidate the cascade marks `NodeSkipped`); a node reachable via any live
/// in-edge is never skipped, so the dead branch's exclusive tail is skipped
/// while shared joins (the agent loop's `end`, a `__finalize__` join) survive.
fn is_runnable(
    node_id: &str,
    ir: &WorkflowIr,
    completed: &HashSet<NodeId>,
    scheduled: &HashSet<NodeId>,
    skipped: &HashSet<NodeId>,
    routes: &HashMap<NodeId, NodeId>,
) -> bool {
    if scheduled.contains(node_id) || completed.contains(node_id) || skipped.contains(node_id) {
        return false;
    }
    let mut has_in_edge = false;
    let mut has_live_edge = false;
    let mut all_live_sources_done = true;
    for e in ir.edges.iter().filter(|e| e.to == node_id) {
        has_in_edge = true;
        if !edge_dead(&e.from, node_id, ir, completed, skipped, routes) {
            has_live_edge = true;
            if !completed.contains(&e.from) {
                all_live_sources_done = false;
            }
        }
    }
    // A node with no in-edges is a root — runnable (subject to the already-run
    // guards above). A node with in-edges runs iff it has a live in-edge AND
    // every live in-edge's source has completed.
    if !has_in_edge {
        return true;
    }
    has_live_edge && all_live_sources_done
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

    /// A single-node workflow whose only step is a `JavaFn` (the Java analog of
    /// the `python_fn_ir` fixture above).
    fn java_fn_ir() -> serde_json::Value {
        serde_json::json!({
            "workflow_id": "wf",
            "version": "0.1.0",
            "name": null,
            "description": null,
            "state_schema": "",
            "start_node": "java_step",
            "nodes": {
                "java_step": {
                    "id": "java_step",
                    "kind": {
                        "type": "java_fn",
                        "class_name": "com.example.tools.WeatherTool",
                        "method": "getWeather",
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

    /// The scheduler must enrich a `JavaFn` work-item payload with `class`,
    /// `method`, and `input` so an external Java tool-worker is self-contained —
    /// exactly mirroring the PythonFn enrichment (bar the dispatch coordinates),
    /// and the work item must route to the `java_tool` queue.
    #[tokio::test]
    async fn java_fn_work_item_payload_is_enriched() {
        let (s, b, e) = setup(java_fn_ir()).await;

        // Seed some accumulated state so we can assert it lands in `input`.
        append(
            &b,
            &e,
            node_completed("prev_node", serde_json::json!({ "key": "value" })),
        )
        .await;

        // Tick: `java_step` is the start node with no predecessors — runnable.
        tick(&s, &e).await;

        // Claim the work item from the java_tool queue.
        let item = b
            .claim_work_item("test-worker", &["java_tool"])
            .await
            .expect("backend claim must not error")
            .expect("a JavaFn node must produce exactly one work item");

        // Queue type
        assert_eq!(item.queue_type, "java_tool");

        // JavaFn-specific enrichment
        assert_eq!(
            item.payload["class"], "com.example.tools.WeatherTool",
            "payload must carry the class name to reflect"
        );
        assert_eq!(
            item.payload["method"], "getWeather",
            "payload must carry the method name to invoke"
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
        assert_eq!(item.payload["node_id"], "java_step");
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

    // ── FDL-3: route-aware is_runnable + NodeSkipped skip cascade ───────────────

    /// Assemble a workflow IR JSON value from a start node, a `nodes` object and
    /// an `edges` array — the empty config maps are boilerplate every IR needs.
    fn build_ir(
        start: &str,
        nodes: serde_json::Value,
        edges: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "workflow_id": "wf",
            "version": "0.1.0",
            "name": null,
            "description": null,
            "state_schema": "",
            "start_node": start,
            "nodes": nodes,
            "edges": edges,
            "retry_policies": {},
            "timeouts": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {},
            "labels": {}
        })
    }

    /// A `condition`-kind node. Non-empty `branches` make it a routing gate;
    /// empty `branches` make it a generic pass-through (the scheduler dispatches
    /// off edges, so the concrete kind doesn't matter for non-gate nodes).
    fn cond_node(id: &str, branches: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": { "type": "condition", "branches": branches },
            "retry_policy": null,
            "node_timeout_secs": null,
            "description": null,
            "labels": {}
        })
    }

    /// A `NodeCompleted` the way the FDL-2 condition executor emits it: the
    /// routing decision recorded on `output` only; `state_patch` is empty so
    /// routing metadata never bleeds into workflow state.
    /// `chosen_target = None` is the null-route case (no branch matched, no
    /// default) — the scheduler records no route, so every branch target dies.
    fn condition_completed(
        node_id: &str,
        chosen_target: Option<&str>,
        branch_targets: &[&str],
    ) -> EventKind {
        let chosen = match chosen_target {
            Some(t) => serde_json::Value::String(t.to_string()),
            None => serde_json::Value::Null,
        };
        let targets: Vec<serde_json::Value> = branch_targets
            .iter()
            .map(|t| serde_json::Value::String(t.to_string()))
            .collect();
        let routing = serde_json::json!({
            "chosen_target": chosen,
            "branch_targets": targets,
        });
        EventKind::NodeCompleted {
            node_id: node_id.into(),
            output: routing,
            state_patch: serde_json::json!({}),
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

    fn skipped_nodes(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|ev| match &ev.kind {
                EventKind::NodeSkipped { node_id, .. } => Some(node_id.to_string()),
                _ => None,
            })
            .collect()
    }

    /// A static agent-loop unroll (turns 0,1 + a final model_2):
    /// `model_0 -> gate_0`; `gate_0` branches `{tool_calls -> tools_0 | else -> end}`;
    /// `tools_0 -> model_1 -> gate_1 -> {tools_1 | end}`; `tools_1 -> model_2 -> end`.
    /// `end` is a shared JOIN: both gates' `else` edges and the final model feed it.
    fn agent_loop_ir() -> serde_json::Value {
        let tool_calls = "state.last_model_finish_reason == \"tool_calls\"";
        build_ir(
            "model_0",
            serde_json::json!({
                "model_0": cond_node("model_0", serde_json::json!([])),
                "gate_0": cond_node("gate_0", serde_json::json!([
                    {"condition": tool_calls, "target": "tools_0"},
                    {"condition": null, "target": "end"}
                ])),
                "tools_0": cond_node("tools_0", serde_json::json!([])),
                "model_1": cond_node("model_1", serde_json::json!([])),
                "gate_1": cond_node("gate_1", serde_json::json!([
                    {"condition": tool_calls, "target": "tools_1"},
                    {"condition": null, "target": "end"}
                ])),
                "tools_1": cond_node("tools_1", serde_json::json!([])),
                "model_2": cond_node("model_2", serde_json::json!([])),
                "end": cond_node("end", serde_json::json!([]))
            }),
            serde_json::json!([
                {"from": "model_0", "to": "gate_0", "condition": null},
                {"from": "gate_0", "to": "tools_0", "condition": tool_calls},
                {"from": "gate_0", "to": "end", "condition": null},
                {"from": "tools_0", "to": "model_1", "condition": null},
                {"from": "model_1", "to": "gate_1", "condition": null},
                {"from": "gate_1", "to": "tools_1", "condition": tool_calls},
                {"from": "gate_1", "to": "end", "condition": null},
                {"from": "tools_1", "to": "model_2", "condition": null},
                {"from": "model_2", "to": "end", "condition": null}
            ]),
        )
    }

    /// FOLD: a Condition `NodeCompleted` with a non-null `chosen_target` records
    /// the route; the node also folds into `completed`.
    #[test]
    fn condition_completion_records_route() {
        let progress = fold_events(vec![condition_completed(
            "gate",
            Some("end"),
            &["tools", "end"],
        )]);
        assert_eq!(
            progress.routes.get("gate").map(String::as_str),
            Some("end"),
            "non-null chosen_target must be recorded as the route"
        );
        assert!(progress.completed.contains("gate"));
    }

    /// FOLD: a null `chosen_target` records NO route entry (the dead-edge
    /// predicate reads a completed routing Condition with no route as
    /// "every out-edge dead").
    #[test]
    fn null_route_records_no_route_entry() {
        let progress = fold_events(vec![condition_completed("gate", None, &["A", "B"])]);
        assert!(
            !progress.routes.contains_key("gate"),
            "null chosen_target must record no route"
        );
        assert!(progress.completed.contains("gate"));
    }

    /// FOLD: `NodeSkipped` folds into BOTH `completed` (satisfies AND-joins) and
    /// `skipped` (so its out-edges read as dead, cascading the skip).
    #[test]
    fn skipped_event_folds_into_completed_and_skipped() {
        let progress = fold_events(vec![EventKind::NodeSkipped {
            node_id: "x".into(),
            reason: "branch not taken".into(),
        }]);
        assert!(
            progress.completed.contains("x"),
            "skipped folds into completed"
        );
        assert!(
            progress.skipped.contains("x"),
            "skipped is tracked for the dead-edge predicate"
        );
    }

    /// HEADLINE — agent-loop early-exit. With `gate_0` routing to `end`
    /// (model_0 returned finish_reason "stop"), the ENTIRE remaining loop is
    /// skipped, `end` is reached via its live in-edge from `gate_0`, and the
    /// workflow completes WITHOUT ever scheduling `model_1`.
    #[tokio::test]
    async fn agent_loop_early_exit_skips_unused_turns() {
        let (s, b, e) = setup(agent_loop_ir()).await;

        // Tick 1: model_0 (start, no in-edges) is scheduled.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(scheduled_nodes(&evs).contains(&"model_0".to_string()));

        // model_0 completes — the model is done (finish_reason "stop").
        append(
            &b,
            &e,
            node_completed(
                "model_0",
                serde_json::json!({"last_model_finish_reason": "stop"}),
            ),
        )
        .await;

        // Tick 2: gate_0 becomes runnable.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(scheduled_nodes(&evs).contains(&"gate_0".to_string()));

        // gate_0 routes to `end` (the FDL-2 executor records chosen_target="end").
        append(
            &b,
            &e,
            condition_completed("gate_0", Some("end"), &["tools_0", "end"]),
        )
        .await;

        // Tick 3: the cascade skips the whole dead branch and dispatches `end`.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        let skipped = skipped_nodes(&evs);
        for n in ["tools_0", "model_1", "gate_1", "tools_1", "model_2"] {
            assert!(
                skipped.contains(&n.to_string()),
                "{n} must be skipped (dead branch tail)"
            );
        }
        // `end` survives (live in-edge from gate_0) and is reached.
        assert!(
            scheduled_nodes(&evs).contains(&"end".to_string()),
            "end must be reached via its live in-edge from gate_0"
        );
        // The early-exit: model_1 must NEVER be scheduled.
        assert!(
            !scheduled_nodes(&evs).contains(&"model_1".to_string()),
            "model_1 must never be scheduled (early exit)"
        );
        // `end` is not skipped.
        assert!(
            !skipped.contains(&"end".to_string()),
            "end must NOT be skipped — it has a live in-edge"
        );

        // end completes -> workflow completes.
        append(&b, &e, node_completed("end", serde_json::json!({}))).await;
        tick(&s, &e).await;
        assert_eq!(status(&b, &e).await, WorkflowStatus::Completed);
    }

    /// Inverse — when `gate_0` routes to `tools_0` (the model asked for a tool),
    /// the loop proceeds and NOTHING is wrongly skipped.
    #[tokio::test]
    async fn agent_loop_route_to_tools_proceeds_without_skipping() {
        let (s, b, e) = setup(agent_loop_ir()).await;

        tick(&s, &e).await; // model_0
        append(
            &b,
            &e,
            node_completed(
                "model_0",
                serde_json::json!({"last_model_finish_reason": "tool_calls"}),
            ),
        )
        .await;
        tick(&s, &e).await; // gate_0
        append(
            &b,
            &e,
            condition_completed("gate_0", Some("tools_0"), &["tools_0", "end"]),
        )
        .await;
        tick(&s, &e).await;

        let evs = b.get_events(&e).await.unwrap();
        assert!(
            scheduled_nodes(&evs).contains(&"tools_0".to_string()),
            "tools_0 must proceed when gate_0 routes to it"
        );
        assert!(
            skipped_nodes(&evs).is_empty(),
            "no node may be skipped while the loop continues"
        );
        assert!(
            !scheduled_nodes(&evs).contains(&"end".to_string()),
            "end must not be reached yet (later gates/model still pending)"
        );
        assert_eq!(status(&b, &e).await, WorkflowStatus::Running);
    }

    /// A `java_fn`-kind tool-dispatch node — the Java analog of the PythonFn
    /// tool node `compile_agent_to_ir` emits. The Java `Agent` builder (Phase B)
    /// emits these so the agent loop's tools route to the durable `java_tool`
    /// queue an external Java worker drains. `no_retry` mirrors the PythonFn tool
    /// node: the dispatch runs user `@Tool` methods (possible non-idempotent
    /// writes), so an already-succeeded dispatch must not re-run on retry.
    fn java_fn_node(id: &str, class_name: &str, method: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": {
                "type": "java_fn",
                "class_name": class_name,
                "method": method,
                "output_schema": ""
            },
            "retry_policy": "no_retry",
            "node_timeout_secs": null,
            "description": null,
            "labels": {}
        })
    }

    /// A `model`-kind agent-turn node carrying OpenAI tool schemas, the way
    /// `compile_agent_to_ir` emits them. `with_tools=false` is the final-answer
    /// node (no tools, so the model must return text).
    fn agent_model_node(id: &str, with_tools: bool) -> serde_json::Value {
        let tools = if with_tools {
            serde_json::json!([
                {"type": "function", "function": {"name": "get_weather", "description": "Get weather", "parameters": {"type": "object", "properties": {"location": {"type": "string"}}, "required": ["location"]}}}
            ])
        } else {
            serde_json::json!([])
        };
        serde_json::json!({
            "id": id,
            "kind": {
                "type": "model",
                "model_ref": "agent_model",
                "prompt_ref": "",
                "output_schema": "",
                "system_prompt": "You are a helpful agent.",
                "tools": tools
            },
            "retry_policy": "llm_default",
            "node_timeout_secs": null,
            "description": null,
            "labels": {}
        })
    }

    /// The Java analog of `agent_loop_ir`: the SAME static-unroll agent loop
    /// (`model -> tool-gate -> tool-dispatch -> model`), but the tool-dispatch
    /// nodes are `java_fn` (route to `java_tool`) instead of `python_fn`. This
    /// documents the IR shape the Java `Agent` builder (Phase B) must emit.
    fn java_agent_loop_ir() -> serde_json::Value {
        let tool_calls = "state.last_model_finish_reason == \"tool_calls\"";
        let mut ir = build_ir(
            "model_0",
            serde_json::json!({
                "model_0": agent_model_node("model_0", true),
                "gate_0": cond_node("gate_0", serde_json::json!([
                    {"condition": tool_calls, "target": "tools_0"},
                    {"condition": null, "target": "end"}
                ])),
                "tools_0": java_fn_node("tools_0", "com.example.AgentTools", "dispatch"),
                "model_1": agent_model_node("model_1", true),
                "gate_1": cond_node("gate_1", serde_json::json!([
                    {"condition": tool_calls, "target": "tools_1"},
                    {"condition": null, "target": "end"}
                ])),
                "tools_1": java_fn_node("tools_1", "com.example.AgentTools", "dispatch"),
                "model_2": agent_model_node("model_2", false)
                // `end` is the terminal sentinel — edges target it, but it is NOT
                // a node (the validator + scheduler treat edge-to-"end" as the
                // terminal), matching `compile_agent_to_ir`'s emitted IR.
            }),
            serde_json::json!([
                {"from": "model_0", "to": "gate_0", "condition": null},
                {"from": "gate_0", "to": "tools_0", "condition": tool_calls},
                {"from": "gate_0", "to": "end", "condition": null},
                {"from": "tools_0", "to": "model_1", "condition": null},
                {"from": "model_1", "to": "gate_1", "condition": null},
                {"from": "gate_1", "to": "tools_1", "condition": tool_calls},
                {"from": "gate_1", "to": "end", "condition": null},
                {"from": "tools_1", "to": "model_2", "condition": null},
                {"from": "model_2", "to": "end", "condition": null}
            ]),
        );
        // Register the model the agent-turn model nodes reference, so the IR
        // validates (the engine accepts/registers the agent loop).
        ir["models"] = serde_json::json!({
            "agent_model": {
                "provider": "anthropic",
                "model": "claude-sonnet-4-6",
                "timeout_secs": null,
                "retry_policy": null,
                "temperature": null,
                "max_tokens": 1024
            }
        });
        ir
    }

    /// A-4: the engine REGISTERS (validates) a JavaFn agent-loop IR and SCHEDULES
    /// its tool node to the `java_tool` queue. The model turn is a deterministic
    /// mock — we append the model's `NodeCompleted` (finish_reason "tool_calls")
    /// the way a model worker would, then the gate routes to the JavaFn node.
    #[tokio::test]
    async fn java_agent_loop_ir_registers_and_schedules_java_tool_node() {
        let ir_json = java_agent_loop_ir();

        // REGISTERS: the engine's IR validator accepts the JavaFn agent loop.
        let parsed: jamjet_ir::WorkflowIr = serde_json::from_value(ir_json.clone())
            .expect("JavaFn agent-loop IR must deserialize into WorkflowIr");
        jamjet_ir::validate_workflow(&parsed)
            .expect("the engine must accept (register) a JavaFn agent-loop IR");

        let (s, b, e) = setup(ir_json).await;

        // Tick 1: model_0 (start) is scheduled.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(scheduled_nodes(&evs).contains(&"model_0".to_string()));

        // Deterministic mock model: model_0 asks for a tool (finish_reason
        // "tool_calls"), exactly what drives the gate to the tool-dispatch node.
        append(
            &b,
            &e,
            node_completed(
                "model_0",
                serde_json::json!({"last_model_finish_reason": "tool_calls"}),
            ),
        )
        .await;

        // Tick 2: gate_0 becomes runnable; route it to tools_0.
        tick(&s, &e).await;
        append(
            &b,
            &e,
            condition_completed("gate_0", Some("tools_0"), &["tools_0", "end"]),
        )
        .await;

        // Tick 3: the JavaFn tool node is scheduled.
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(
            scheduled_nodes(&evs).contains(&"tools_0".to_string()),
            "the JavaFn tool node must be scheduled when the gate routes to it"
        );
        // SCHEDULES to java_tool: the NodeScheduled event carries the queue.
        let tools_queue = evs.iter().find_map(|ev| match &ev.kind {
            EventKind::NodeScheduled {
                node_id,
                queue_type,
            } if node_id == "tools_0" => Some(queue_type.clone()),
            _ => None,
        });
        assert_eq!(
            tools_queue.as_deref(),
            Some("java_tool"),
            "the agent-loop tool node must be scheduled to the java_tool queue"
        );

        // The enqueued work item is a claimable java_tool item carrying the
        // dispatch enrichment the Phase B Java worker reflects against.
        let item = b
            .claim_work_item("java-tool-worker", &["java_tool"])
            .await
            .unwrap()
            .expect("the scheduled JavaFn node must enqueue a java_tool work item");
        assert_eq!(item.queue_type, "java_tool");
        assert_eq!(item.node_id, "tools_0");
        assert_eq!(item.payload["class"], "com.example.AgentTools");
        assert_eq!(item.payload["method"], "dispatch");
        assert!(
            item.payload["input"].is_object(),
            "payload.input must carry the accumulated agent state"
        );

        // The loop is still live: nothing wrongly skipped, terminal not reached.
        assert!(skipped_nodes(&evs).is_empty());
        assert_eq!(status(&b, &e).await, WorkflowStatus::Running);
    }

    /// A diamond AND-join: `C -> {L, R}`, `L -> J`, `R -> J`, `J -> end`.
    /// `C` routes to `L`. The dead branch `R` is skipped, but `J` STILL RUNS
    /// (live in-edge from L) once L completes — neither premature completion
    /// (J skipped) nor hang (J waiting on the skipped R).
    fn diamond_ir() -> serde_json::Value {
        build_ir(
            "C",
            serde_json::json!({
                "C": cond_node("C", serde_json::json!([
                    {"condition": "state.go == \"L\"", "target": "L"},
                    {"condition": null, "target": "R"}
                ])),
                "L": cond_node("L", serde_json::json!([])),
                "R": cond_node("R", serde_json::json!([])),
                "J": cond_node("J", serde_json::json!([])),
                "end": cond_node("end", serde_json::json!([]))
            }),
            serde_json::json!([
                {"from": "C", "to": "L", "condition": "state.go == \"L\""},
                {"from": "C", "to": "R", "condition": null},
                {"from": "L", "to": "J", "condition": null},
                {"from": "R", "to": "J", "condition": null},
                {"from": "J", "to": "end", "condition": null}
            ]),
        )
    }

    #[tokio::test]
    async fn diamond_join_skips_dead_branch_but_join_survives() {
        let (s, b, e) = setup(diamond_ir()).await;

        tick(&s, &e).await; // C
        append(&b, &e, condition_completed("C", Some("L"), &["L", "R"])).await;
        tick(&s, &e).await;

        let evs = b.get_events(&e).await.unwrap();
        assert!(
            skipped_nodes(&evs).contains(&"R".to_string()),
            "R (dead branch) must be skipped"
        );
        assert!(
            !skipped_nodes(&evs).contains(&"J".to_string()),
            "J must NOT be skipped — it has a live in-edge from L"
        );
        assert!(
            scheduled_nodes(&evs).contains(&"L".to_string()),
            "L (taken branch) must be scheduled"
        );
        assert!(
            !scheduled_nodes(&evs).contains(&"J".to_string()),
            "J must not run until L completes"
        );

        // L completes -> J becomes runnable via the AND-join over LIVE edges only.
        append(&b, &e, node_completed("L", serde_json::json!({}))).await;
        tick(&s, &e).await;
        let evs = b.get_events(&e).await.unwrap();
        assert!(
            scheduled_nodes(&evs).contains(&"J".to_string()),
            "J must run once L completes — it must not wait on the skipped R"
        );

        // J -> end -> complete.
        append(&b, &e, node_completed("J", serde_json::json!({}))).await;
        tick(&s, &e).await; // schedules end
        append(&b, &e, node_completed("end", serde_json::json!({}))).await;
        tick(&s, &e).await; // completes
        assert_eq!(
            status(&b, &e).await,
            WorkflowStatus::Completed,
            "workflow completes via J (no premature completion, no hang)"
        );
    }

    /// BACKWARD-COMPAT: an empty-`branches` Condition is a pass-through — both
    /// successors run, nothing is skipped (a workflow with no routing gates is
    /// entirely unaffected by FDL-3).
    #[tokio::test]
    async fn empty_branches_condition_runs_all_successors() {
        let ir = build_ir(
            "start",
            serde_json::json!({
                "start": cond_node("start", serde_json::json!([])),
                "a": cond_node("a", serde_json::json!([])),
                "b": cond_node("b", serde_json::json!([]))
            }),
            serde_json::json!([
                {"from": "start", "to": "a", "condition": null},
                {"from": "start", "to": "b", "condition": null}
            ]),
        );
        let (s, b, e) = setup(ir).await;

        tick(&s, &e).await; // start
        append(&b, &e, node_completed("start", serde_json::json!({}))).await;
        tick(&s, &e).await;

        let evs = b.get_events(&e).await.unwrap();
        assert!(
            scheduled_nodes(&evs).contains(&"a".to_string()),
            "both successors of an empty-branches condition must run (a)"
        );
        assert!(
            scheduled_nodes(&evs).contains(&"b".to_string()),
            "both successors of an empty-branches condition must run (b)"
        );
        assert!(
            skipped_nodes(&evs).is_empty(),
            "an empty-branches condition must never skip a successor"
        );
    }

    /// NO-ROUTE: a Condition that matched no branch and had no default
    /// (`chosen_target = null`) skips ALL its branch targets' exclusive tails,
    /// and the run still TERMINATES (no hang).
    #[tokio::test]
    async fn no_matching_branch_skips_all_targets_and_terminates() {
        let ir = build_ir(
            "start",
            serde_json::json!({
                "start": cond_node("start", serde_json::json!([])),
                "gate": cond_node("gate", serde_json::json!([
                    {"condition": "state.x == \"1\"", "target": "A"},
                    {"condition": "state.y == \"2\"", "target": "B"}
                ])),
                "A": cond_node("A", serde_json::json!([])),
                "B": cond_node("B", serde_json::json!([]))
            }),
            serde_json::json!([
                {"from": "start", "to": "gate", "condition": null},
                {"from": "gate", "to": "A", "condition": "state.x == \"1\""},
                {"from": "gate", "to": "B", "condition": "state.y == \"2\""}
            ]),
        );
        let (s, b, e) = setup(ir).await;

        tick(&s, &e).await; // start
        append(&b, &e, node_completed("start", serde_json::json!({}))).await;
        tick(&s, &e).await; // gate
                            // gate matched no branch and has no default -> chosen_target null.
        append(&b, &e, condition_completed("gate", None, &["A", "B"])).await;
        tick(&s, &e).await;

        let evs = b.get_events(&e).await.unwrap();
        let skipped = skipped_nodes(&evs);
        assert!(
            skipped.contains(&"A".to_string()),
            "A must be skipped (null route -> every target dead)"
        );
        assert!(
            skipped.contains(&"B".to_string()),
            "B must be skipped (null route -> every target dead)"
        );
        assert_eq!(
            status(&b, &e).await,
            WorkflowStatus::Completed,
            "the run must still terminate when no branch matched (no hang)"
        );
    }

    // ── FDL-4: determinism / replay of the route + skip set ─────────────────────

    /// Re-fold an event log into a FRESH `ExecProgress` — exactly what a recovery
    /// or replay does (rebuild scheduling state purely from the durable log).
    fn replay(events: &[Event]) -> ExecProgress {
        let mut progress = ExecProgress::default();
        for ev in events {
            progress.apply(ev);
        }
        progress
    }

    /// The sorted set of nodes the dispatcher would consider runnable against a
    /// folded progress — the pure routing decision `is_runnable` makes each tick.
    fn runnable_set(ir: &WorkflowIr, p: &ExecProgress) -> Vec<String> {
        let mut v: Vec<String> = ir
            .nodes
            .keys()
            .filter(|nid| is_runnable(nid, ir, &p.completed, &p.scheduled, &p.skipped, &p.routes))
            .cloned()
            .collect();
        v.sort();
        v
    }

    /// REPLAY DETERMINISM (headline): drive the agent-loop early-exit to
    /// completion, then re-fold the durable event log from scratch. The recorded
    /// `chosen_target` + the emitted `NodeSkipped` events reproduce the EXACT same
    /// route + skip set + completed set on every replay — no re-evaluation drift.
    #[tokio::test]
    async fn early_exit_route_and_skips_replay_deterministically() {
        let (s, b, e) = setup(agent_loop_ir()).await;

        // Run the early-exit (gate_0 -> end) all the way to terminal.
        tick(&s, &e).await; // model_0
        append(
            &b,
            &e,
            node_completed(
                "model_0",
                serde_json::json!({"last_model_finish_reason": "stop"}),
            ),
        )
        .await;
        tick(&s, &e).await; // gate_0
        append(
            &b,
            &e,
            condition_completed("gate_0", Some("end"), &["tools_0", "end"]),
        )
        .await;
        tick(&s, &e).await; // cascade skips the dead branch, dispatches `end`
        append(&b, &e, node_completed("end", serde_json::json!({}))).await;
        tick(&s, &e).await; // completes
        assert_eq!(status(&b, &e).await, WorkflowStatus::Completed);

        // Re-fold the SAME durable log twice (two independent replays).
        let evs = b.get_events(&e).await.unwrap();
        let r1 = replay(&evs);
        let r2 = replay(&evs);

        // The fold is a pure function of the log: replay == replay (no drift).
        assert_eq!(
            r1.routes, r2.routes,
            "re-folding the log is deterministic (routes)"
        );
        assert_eq!(
            r1.skipped, r2.skipped,
            "re-folding the log is deterministic (skips)"
        );
        assert_eq!(
            r1.completed, r2.completed,
            "re-folding the log is deterministic (completed)"
        );

        // ...and reproduces the exact recorded route + dead-branch skip set.
        assert_eq!(
            r1.routes.get("gate_0").map(String::as_str),
            Some("end"),
            "the recorded route is reproduced verbatim on replay"
        );
        for n in ["tools_0", "model_1", "gate_1", "tools_1", "model_2"] {
            assert!(
                r1.skipped.contains(n),
                "{n} must be skipped on replay (dead branch)"
            );
        }
        assert!(
            !r1.skipped.contains("end"),
            "end (shared join, live in-edge) must never be skipped on replay"
        );

        // A completed run replays to NO runnable node — replay never resurrects a
        // skipped node nor strands the terminal.
        let ir: WorkflowIr = serde_json::from_value(agent_loop_ir()).unwrap();
        assert!(
            runnable_set(&ir, &r1).is_empty(),
            "a completed run replays with an empty runnable set"
        );
    }

    /// DETERMINISM — the route is the RECORDED `chosen_target`, never recomputed.
    /// Fold a durable record whose gate routed to `end` even though the committed
    /// state (`finish_reason == "tool_calls"`) would, if re-evaluated, route to
    /// `tools_0`. The recorded route + `NodeSkipped` events ALONE determine the
    /// runnable set: `end` runs, the dead branch never does — identical on replay.
    #[test]
    fn recorded_route_and_skips_determine_runnable_node() {
        let ir: WorkflowIr = serde_json::from_value(agent_loop_ir()).unwrap();

        // The committed state says "tool_calls" (re-evaluation would pick tools_0)
        // but the RECORDED route is `end`; the durable skips fold the dead branch.
        let skip = |n: &str| EventKind::NodeSkipped {
            node_id: n.into(),
            reason: "branch not taken".into(),
        };
        let log = vec![
            node_completed(
                "model_0",
                serde_json::json!({"last_model_finish_reason": "tool_calls"}),
            ),
            condition_completed("gate_0", Some("end"), &["tools_0", "end"]),
            skip("tools_0"),
            skip("model_1"),
            skip("gate_1"),
            skip("tools_1"),
            skip("model_2"),
        ];
        let p1 = fold_events(log.clone());
        let p2 = fold_events(log);

        // The recorded route is honored, NOT recomputed from the (tool_calls) state.
        assert_eq!(
            p1.routes.get("gate_0").map(String::as_str),
            Some("end"),
            "the recorded route stands even when live state would evaluate otherwise"
        );
        assert_eq!(
            p1.routes, p2.routes,
            "route fold is deterministic across replays"
        );
        assert_eq!(
            p1.skipped, p2.skipped,
            "skip fold is deterministic across replays"
        );

        // The route + skips alone make `end` the sole runnable node; the dead
        // branch's `tools_0` is never runnable — identical on every replay.
        assert_eq!(
            runnable_set(&ir, &p1),
            vec!["end".to_string()],
            "the recorded route + skips determine `end` as the only runnable node"
        );
        assert_eq!(
            runnable_set(&ir, &p1),
            runnable_set(&ir, &p2),
            "the runnable decision is deterministic across replays"
        );
    }

    /// NULL-ROUTE replay determinism: a Condition that matched no branch and had no
    /// default records NO route, and that absence replays deterministically — every
    /// branch target is skipped on every re-fold and the run still terminates.
    /// (`no_matching_branch_skips_all_targets_and_terminates` covers the live run;
    /// this locks the REPLAY of that null-route decision.)
    #[tokio::test]
    async fn null_route_skips_replay_deterministically() {
        let ir = build_ir(
            "start",
            serde_json::json!({
                "start": cond_node("start", serde_json::json!([])),
                "gate": cond_node("gate", serde_json::json!([
                    {"condition": "state.x == \"1\"", "target": "A"},
                    {"condition": "state.y == \"2\"", "target": "B"}
                ])),
                "A": cond_node("A", serde_json::json!([])),
                "B": cond_node("B", serde_json::json!([]))
            }),
            serde_json::json!([
                {"from": "start", "to": "gate", "condition": null},
                {"from": "gate", "to": "A", "condition": "state.x == \"1\""},
                {"from": "gate", "to": "B", "condition": "state.y == \"2\""}
            ]),
        );
        let (s, b, e) = setup(ir).await;

        tick(&s, &e).await; // start
        append(&b, &e, node_completed("start", serde_json::json!({}))).await;
        tick(&s, &e).await; // gate
        append(&b, &e, condition_completed("gate", None, &["A", "B"])).await;
        tick(&s, &e).await; // cascade skips A + B
        assert_eq!(status(&b, &e).await, WorkflowStatus::Completed);

        // Re-fold the durable log twice: the null route records NO entry and both
        // targets are skipped — deterministically, on every replay.
        let evs = b.get_events(&e).await.unwrap();
        let r1 = replay(&evs);
        let r2 = replay(&evs);
        assert!(
            !r1.routes.contains_key("gate"),
            "a null route records no entry — reproduced on replay"
        );
        assert_eq!(r1.routes, r2.routes, "null-route fold is deterministic");
        assert_eq!(
            r1.skipped, r2.skipped,
            "null-route skip fold is deterministic"
        );
        for n in ["A", "B"] {
            assert!(
                r1.skipped.contains(n),
                "{n} must be skipped on replay (null route)"
            );
        }
    }
}
