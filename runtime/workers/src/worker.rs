use crate::executor::{ExecutionResult, NodeExecutor};
use crate::heartbeat::spawn_heartbeat;
use jamjet_agents::AgentRegistry;
use jamjet_core::node::NodeKind;
use jamjet_core::workflow::ExecutionId;
use jamjet_ir::workflow::{NodeDef, WorkflowIr};
use jamjet_policy::autonomy::{AutonomyContext, AutonomyDecision, AutonomyEnforcer};
use jamjet_policy::engine::node_kind_tag;
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};
use jamjet_state::approvals::NodeApprovalStatus;
use jamjet_state::backend::{StateBackend, StateBackendError, WorkItem, WorkItemId};
use jamjet_state::budget::BudgetState;
use jamjet_state::event::EventKind;
use jamjet_state::{apply_events_seeded, materialize, MaterializedState, Snapshot};
use jamjet_telemetry::{gen_ai_attrs, metrics, record_gen_ai_usage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, instrument, warn};

/// A JamJet worker process.
///
/// Workers pull work items from the queue, execute them, and report results.
pub struct Worker {
    pub worker_id: String,
    backend: Arc<dyn StateBackend>,
    /// Optional agent registry — required for autonomy enforcement.
    agents: Option<Arc<dyn AgentRegistry>>,
    queue_types: Vec<String>,
    poll_interval: Duration,
    executors: HashMap<String, Arc<dyn NodeExecutor>>,
}

impl Worker {
    pub fn new(
        worker_id: String,
        backend: Arc<dyn StateBackend>,
        queue_types: Vec<String>,
    ) -> Self {
        Self {
            worker_id,
            backend,
            agents: None,
            queue_types,
            poll_interval: Duration::from_millis(500),
            executors: HashMap::new(),
        }
    }

    /// Attach an agent registry for autonomy enforcement.
    pub fn with_agents(mut self, agents: Arc<dyn AgentRegistry>) -> Self {
        self.agents = Some(agents);
        self
    }

    pub fn register_executor(
        mut self,
        kind: impl Into<String>,
        executor: Arc<dyn NodeExecutor>,
    ) -> Self {
        self.executors.insert(kind.into(), executor);
        self
    }

    pub async fn run(&self) {
        info!(
            worker_id = %self.worker_id,
            queues = ?self.queue_types,
            "Worker started"
        );
        loop {
            match self.poll_and_execute().await {
                Ok(true) => {}
                Ok(false) => tokio::time::sleep(self.poll_interval).await,
                Err(e) => {
                    warn!(worker_id = %self.worker_id, "Worker error: {e}");
                    tokio::time::sleep(self.poll_interval).await;
                }
            }
        }
    }

    #[instrument(skip(self), fields(worker_id = %self.worker_id))]
    async fn poll_and_execute(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let queue_refs: Vec<&str> = self.queue_types.iter().map(|s| s.as_str()).collect();
        let item = self
            .backend
            .claim_work_item(&self.worker_id, &queue_refs)
            .await?;
        let Some(item) = item else { return Ok(false) };
        info!(
            worker_id = %self.worker_id,
            execution_id = %item.execution_id,
            node_id = %item.node_id,
            attempt = item.attempt,
            "Claimed work item"
        );
        self.execute_item(item).await?;
        Ok(true)
    }

    async fn execute_item(
        &self,
        item: WorkItem,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let start = std::time::Instant::now();
        let execution_id = item.execution_id.clone();
        let node_id = item.node_id.clone();
        let tenant_id = item.tenant_id.clone();
        let attempt = item.attempt;
        let item_id = item.id;
        let lease_fence = item.lease_fence;

        let node_span = tracing::info_span!(
            "jamjet.node",
            "jamjet.execution.id" = %execution_id,
            "jamjet.node.id" = %node_id,
            "jamjet.attempt" = attempt,
            "jamjet.worker.id" = %self.worker_id,
            "jamjet.workflow.id" = tracing::field::Empty,
            "jamjet.workflow.version" = tracing::field::Empty,
            "jamjet.node.kind" = tracing::field::Empty,
            "gen_ai.system" = tracing::field::Empty,
            "gen_ai.request.model" = tracing::field::Empty,
            "gen_ai.response.model" = tracing::field::Empty,
            "gen_ai.operation.name" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
        );
        let _span_guard = node_span.enter();

        // Emit NodeStarted.
        let seq = self.backend.latest_sequence(&execution_id).await? + 1;
        self.backend
            .append_event(jamjet_state::Event::new(
                execution_id.clone(),
                seq,
                EventKind::NodeStarted {
                    node_id: node_id.clone(),
                    worker_id: self.worker_id.clone(),
                    attempt,
                },
            ))
            .await?;

        // Spawn heartbeat.
        let heartbeat = spawn_heartbeat(
            Arc::clone(&self.backend),
            item_id,
            self.worker_id.clone(),
            item.lease_fence,
            Duration::from_secs(15),
        );

        // Load workflow IR.
        let (workflow_id, workflow_version) = parse_payload(&item.payload);
        node_span.record(gen_ai_attrs::JAMJET_WORKFLOW_ID, workflow_id.as_str());
        node_span.record(
            gen_ai_attrs::JAMJET_WORKFLOW_VERSION,
            workflow_version.as_str(),
        );

        let result: Result<ExecutionResult, String> = match self
            .load_ir(&workflow_id, &workflow_version)
            .await
        {
            Err(e) => Err(format!("failed to load IR: {e}")),
            Ok(ir) => match ir.node(&node_id) {
                None => Err(format!("node {node_id} not found in IR")),
                Some(node_def) => {
                    let kind = &node_def.kind;
                    let kind_tag = node_kind_tag(kind);
                    node_span.record(gen_ai_attrs::JAMJET_NODE_KIND, kind_tag.as_str());

                    // Load budget state from latest snapshot.
                    let budget = self.load_budget_state(&execution_id).await;

                    // Policy check.
                    if let Some(r) = self
                        .check_policy(
                            &execution_id,
                            item_id,
                            &node_id,
                            &tenant_id,
                            kind,
                            node_def,
                            &ir,
                        )
                        .await
                    {
                        heartbeat.abort();
                        return r;
                    }

                    // Autonomy check.
                    if let Some(r) = self
                        .check_autonomy(&execution_id, item_id, &node_id, kind, &budget)
                        .await
                    {
                        heartbeat.abort();
                        return r;
                    }

                    // Execute.
                    let exec_result = match self.executors.get(&kind_tag) {
                        Some(executor) if kind_tag == "agent_tool" => {
                            let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
                            let backend = Arc::clone(&self.backend);
                            let eid = execution_id.clone();
                            let receiver_handle = tokio::spawn(async move {
                                while let Some(event) = rx.recv().await {
                                    backend
                                        .patch_append_array(&eid, "agent_tool_events", event)
                                        .await
                                        .map_err(|e| format!("patch_append_array failed: {e}"))?;
                                }
                                Ok::<(), String>(())
                            });
                            let result = executor.execute_streaming(&item, tx).await;
                            match receiver_handle.await {
                                Ok(Err(e)) => Err(e),
                                Err(e) => Err(format!("Receiver task panicked: {e}")),
                                Ok(Ok(())) => result,
                            }
                        }
                        Some(executor) => executor.execute(&item).await,
                        None => {
                            info!(node_id = %node_id, kind = %kind_tag, "No executor; using stub");
                            Ok(ExecutionResult {
                                output: serde_json::json!({}),
                                state_patch: serde_json::json!({}),
                                duration_ms: start.elapsed().as_millis() as u64,
                                gen_ai_system: None,
                                gen_ai_model: None,
                                input_tokens: None,
                                output_tokens: None,
                                finish_reason: None,
                            })
                        }
                    };

                    // Budget enforcement after execution.
                    match exec_result {
                        Ok(mut result) => {
                            let mut budget = budget;
                            if let Some(r) = self
                                .check_budget_after_execution(
                                    &execution_id,
                                    &node_id,
                                    &ir,
                                    &mut budget,
                                    result.input_tokens,
                                    result.output_tokens,
                                    None,
                                    &mut result.state_patch,
                                )
                                .await
                            {
                                heartbeat.abort();
                                return r;
                            }
                            Ok(result)
                        }
                        Err(e) => Err(e),
                    }
                }
            },
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        heartbeat.abort();

        match result {
            Ok(exec_result) => {
                if let (Some(system), Some(model), Some(input), Some(output)) = (
                    &exec_result.gen_ai_system,
                    &exec_result.gen_ai_model,
                    exec_result.input_tokens,
                    exec_result.output_tokens,
                ) {
                    record_gen_ai_usage(&node_span, system, model, input, output);
                    node_span.record(gen_ai_attrs::OPERATION_NAME, "chat");
                    // The executor surfaces the requested model; record it as the
                    // response model too until a distinct response model is exposed.
                    node_span.record(gen_ai_attrs::RESPONSE_MODEL, model.as_str());
                    metrics::gen_ai_token_usage(system, model, "chat", input, output);
                }
                if let Some(finish_reason) = &exec_result.finish_reason {
                    node_span.record(
                        gen_ai_attrs::RESPONSE_FINISH_REASONS,
                        finish_reason.as_str(),
                    );
                }

                let terminal = jamjet_state::Event::new(
                    execution_id.clone(),
                    0, // sequence assigned atomically inside commit_turn
                    EventKind::NodeCompleted {
                        node_id: node_id.clone(),
                        output: exec_result.output,
                        state_patch: exec_result.state_patch,
                        duration_ms,
                        gen_ai_system: exec_result.gen_ai_system,
                        gen_ai_model: exec_result.gen_ai_model,
                        input_tokens: exec_result.input_tokens,
                        output_tokens: exec_result.output_tokens,
                        finish_reason: exec_result.finish_reason,
                        cost_usd: None,
                        provenance: None,
                    },
                );
                // Compute the post-turn snapshot by folding THIS turn's terminal
                // event onto the materialized state that precedes it.
                // `materialize` reflects state before this event (it is committed
                // inside commit_turn). Folding the terminal event onto `prior` gives
                // us the exact state that will exist after the commit, without a
                // second DB round-trip.
                //
                // Note: `terminal.sequence` is 0 here; `commit_turn` overwrites
                // `snap.at_sequence` and `snap.last_sequence` with the real
                // assigned sequence, so the worker's 0 does not corrupt the key.
                let snap = {
                    let prior = materialize(self.backend.as_ref(), &execution_id)
                        .await
                        .unwrap_or_else(|_| MaterializedState {
                            current_state: serde_json::json!({}),
                            status: jamjet_core::workflow::WorkflowStatus::Running,
                            completed_nodes: std::collections::HashMap::new(),
                            active_nodes: std::collections::HashSet::new(),
                            last_sequence: 0,
                        });
                    let next = apply_events_seeded(prior, &[terminal.clone()]);
                    Some(Snapshot::from_materialized(execution_id.clone(), &next))
                };
                match self
                    .backend
                    .commit_turn(item_id, lease_fence, terminal, snap)
                    .await
                {
                    Ok(_) => {
                        info!(execution_id = %execution_id, node_id = %node_id, duration_ms, "Node completed");
                    }
                    Err(StateBackendError::FenceLost(_)) => {
                        // This worker's lease was stolen (stall/failover). Another
                        // worker owns the item now. Stop quietly; emit nothing.
                        warn!(execution_id = %execution_id, node_id = %node_id, "Fence lost on commit; abandoning (lease stolen)");
                        return Ok(());
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
            Err(error) => {
                let terminal = jamjet_state::Event::new(
                    execution_id.clone(),
                    0, // sequence assigned atomically inside commit_node_terminal
                    EventKind::NodeFailed {
                        node_id: node_id.clone(),
                        error: error.clone(),
                        attempt,
                        retryable: false,
                    },
                );
                match self
                    .backend
                    .commit_turn(item_id, lease_fence, terminal, None)
                    .await
                {
                    Ok(_) => {
                        warn!(execution_id = %execution_id, node_id = %node_id, attempt, %error, "Node failed");
                    }
                    Err(StateBackendError::FenceLost(_)) => {
                        warn!(execution_id = %execution_id, node_id = %node_id, "Fence lost on failure commit; abandoning");
                        return Ok(());
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
        }
        Ok(())
    }

    // ── Approval hold helper ──────────────────────────────────────────────────

    /// Park a node pending human approval (or let it run if already approved).
    ///
    /// Consults the event log: an `ApprovalReceived{Approved}` with no newer
    /// `ToolApprovalRequired` means the gate is satisfied — return None so the
    /// caller proceeds to execute. Otherwise ensure exactly one outstanding
    /// `ToolApprovalRequired` exists and settle the work item cleanly so the
    /// lease never expires into the retry/dead-letter path. The node stays
    /// parked because it remains in the scheduler fold's `scheduled` set (so
    /// it is never re-dispatched); the fold's `held` set only gates which
    /// decision events are acted on.
    /// The helper requires the FULL event log because a partial fold could
    /// misreport NotRequested for an already-approved node.
    async fn hold_for_approval(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
        item_id: WorkItemId,
        tool_name: String,
        approver: String,
        context: serde_json::Value,
    ) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        let events = match self.backend.get_events(execution_id).await {
            Ok(events) => events,
            Err(e) => {
                // Fail closed: never run an approval-gated node blind.
                return Some(Err(format!(
                    "approval state unavailable; refusing to run unapproved: {e}"
                )
                .into()));
            }
        };

        match jamjet_state::approvals::node_approval_status(&events, node_id) {
            NodeApprovalStatus::Approved { .. } => {
                info!(execution_id = %execution_id, node_id, "Approval satisfied — proceeding");
                None
            }
            NodeApprovalStatus::Pending(_) | NodeApprovalStatus::Rejected { .. } => {
                // Already requested (or decided rejected — scheduler owns the
                // failure). Settle the item; emit nothing.
                if let Err(e) = self.backend.complete_work_item(item_id).await {
                    return Some(Err(format!("failed to settle held work item: {e}").into()));
                }
                Some(Ok(()))
            }
            NodeApprovalStatus::NotRequested => {
                info!(execution_id = %execution_id, node_id, %approver, "Node requires approval");
                let seq = match self.backend.latest_sequence(execution_id).await {
                    Ok(s) => s + 1,
                    Err(e) => {
                        return Some(Err(format!(
                            "approval required but could not be recorded: {e}"
                        )
                        .into()))
                    }
                };
                if let Err(e) = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::ToolApprovalRequired {
                            node_id: node_id.to_string(),
                            tool_name,
                            approver,
                            context,
                        },
                    ))
                    .await
                {
                    // Fail closed.
                    return Some(Err(format!(
                        "approval required but could not be recorded: {e}"
                    )
                    .into()));
                }
                if let Err(e) = self.backend.complete_work_item(item_id).await {
                    return Some(Err(format!("failed to settle held work item: {e}").into()));
                }
                Some(Ok(()))
            }
        }
    }

    // ── Policy check ──────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    async fn check_policy(
        &self,
        execution_id: &ExecutionId,
        item_id: WorkItemId,
        node_id: &str,
        tenant_id: &str,
        kind: &NodeKind,
        node_def: &NodeDef,
        ir: &WorkflowIr,
    ) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        let ctx = EvaluationContext::from_node_kind(node_id, kind);

        // Load tenant policy (sits between global and workflow in the chain).
        let tenant_policy_set = self
            .backend
            .get_tenant(&jamjet_state::TenantId::from(tenant_id))
            .await
            .ok()
            .flatten()
            .and_then(|t| t.policy_set());

        // Build policy chain: tenant -> workflow -> node (least-specific to most-specific).
        // The evaluator iterates in reverse, so node rules take priority.
        let mut sets: Vec<&jamjet_ir::workflow::PolicySetIr> = Vec::new();
        if let Some(ref tp) = tenant_policy_set {
            sets.push(tp);
        }
        if let Some(p) = &ir.policy {
            sets.push(p);
        }
        if let Some(p) = &node_def.policy {
            sets.push(p);
        }
        if sets.is_empty() {
            return None;
        }

        let decision = PolicyEvaluator.evaluate(&ctx, &sets);

        match decision {
            PolicyDecision::Allow => None,

            PolicyDecision::Block { reason } => {
                let policy_scope = self.identify_policy_scope(
                    &ctx,
                    tenant_policy_set.as_ref(),
                    ir.policy.as_ref(),
                    node_def.policy.as_ref(),
                );
                warn!(execution_id = %execution_id, node_id, %reason, %policy_scope, "Policy blocked node");
                // Best-effort audit. The block is enforced regardless of whether
                // recording the violation succeeds: fail closed, never open.
                if let Ok(latest) = self.backend.latest_sequence(execution_id).await {
                    let _ = self
                        .backend
                        .append_event(jamjet_state::Event::new(
                            execution_id.clone(),
                            latest + 1,
                            EventKind::PolicyViolation {
                                node_id: node_id.to_string(),
                                rule: reason.clone(),
                                decision: "blocked".to_string(),
                                policy_scope,
                            },
                        ))
                        .await;
                }
                Some(Err(format!("policy blocked: {reason}").into()))
            }

            PolicyDecision::RequireApproval { approver } => {
                let tool_name = ctx.tool_name.unwrap_or_else(|| node_id.to_string());
                self.hold_for_approval(
                    execution_id,
                    node_id,
                    item_id,
                    tool_name,
                    approver,
                    serde_json::json!({ "node_id": node_id }),
                )
                .await
            }
        }
    }

    /// Determine which policy scope triggered a non-Allow decision.
    ///
    /// Checks each scope individually from most-specific (node) to least-specific
    /// (tenant), returning the name of the first scope that produces a non-Allow
    /// decision.
    fn identify_policy_scope(
        &self,
        ctx: &EvaluationContext,
        tenant_policy: Option<&jamjet_ir::workflow::PolicySetIr>,
        workflow_policy: Option<&jamjet_ir::workflow::PolicySetIr>,
        node_policy: Option<&jamjet_ir::workflow::PolicySetIr>,
    ) -> String {
        // Check most-specific first (same order as evaluator's reverse iteration).
        if let Some(p) = node_policy {
            if !matches!(PolicyEvaluator.evaluate(ctx, &[p]), PolicyDecision::Allow) {
                return "node".to_string();
            }
        }
        if let Some(p) = workflow_policy {
            if !matches!(PolicyEvaluator.evaluate(ctx, &[p]), PolicyDecision::Allow) {
                return "workflow".to_string();
            }
        }
        if let Some(p) = tenant_policy {
            if !matches!(PolicyEvaluator.evaluate(ctx, &[p]), PolicyDecision::Allow) {
                return "tenant".to_string();
            }
        }
        "unknown".to_string()
    }

    // ── Autonomy check ────────────────────────────────────────────────────────

    async fn check_autonomy(
        &self,
        execution_id: &ExecutionId,
        item_id: WorkItemId,
        node_id: &str,
        kind: &NodeKind,
        budget: &BudgetState,
    ) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        let kind_tag = node_kind_tag(kind);
        if kind_tag != "agent" {
            return None;
        }
        let agents = self.agents.as_ref()?;

        let agent_ref = serde_json::to_value(kind)
            .ok()
            .and_then(|v| {
                v.get("agent_ref")
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| node_id.to_string());

        let agent = match agents.get_by_uri(&agent_ref).await {
            Ok(Some(a)) => a,
            _ => return None,
        };
        let card = agent.card;

        let ctx = AutonomyContext {
            agent_ref: agent_ref.clone(),
            autonomy_level: card.autonomy.clone(),
            constraints: card.constraints.clone(),
            current_iterations: budget.iteration_count,
            current_tool_calls: budget.tool_call_count,
            current_cost_usd: budget.total_cost_usd,
            current_tokens: budget.total_tokens(),
            consecutive_errors: budget.consecutive_error_count,
            circuit_breaker_threshold: 3,
        };

        let decision = AutonomyEnforcer.check(&ctx, None);

        match decision {
            AutonomyDecision::Proceed => None,

            // Currently unreachable at runtime: `AutonomyEnforcer.check` above
            // is called with `tool_name: None` (pre-existing), so it never
            // returns RequireToolApproval. The wiring is in place for when
            // pending tool names are threaded through.
            AutonomyDecision::RequireToolApproval { tool_name } => {
                self.hold_for_approval(
                    execution_id,
                    node_id,
                    item_id,
                    tool_name,
                    "human".to_string(),
                    serde_json::json!({ "agent_ref": agent_ref }),
                )
                .await
            }

            AutonomyDecision::EscalateLimit {
                limit_type,
                limit_value,
                actual_value,
                escalation_target,
            } => {
                warn!(execution_id = %execution_id, node_id, %limit_type, "Autonomy limit reached");
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::AutonomyLimitReached {
                            node_id: node_id.to_string(),
                            agent_ref: agent_ref.clone(),
                            limit_type: limit_type.clone(),
                            limit_value,
                            actual_value,
                        },
                    ))
                    .await;
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::EscalationRequired {
                            node_id: node_id.to_string(),
                            agent_ref,
                            reason: format!("autonomy_limit:{limit_type}"),
                            escalation_target: escalation_target.as_str(),
                        },
                    ))
                    .await;
                Some(Err(format!("autonomy limit: {limit_type}").into()))
            }

            AutonomyDecision::TripCircuitBreaker {
                consecutive_errors,
                threshold,
                escalation_target,
            } => {
                warn!(execution_id = %execution_id, node_id, consecutive_errors, "Circuit breaker tripped");
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::CircuitBreakerTripped {
                            node_id: node_id.to_string(),
                            agent_ref: agent_ref.clone(),
                            consecutive_errors,
                            threshold,
                        },
                    ))
                    .await;
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::EscalationRequired {
                            node_id: node_id.to_string(),
                            agent_ref,
                            reason: "circuit_breaker".to_string(),
                            escalation_target: escalation_target.as_str(),
                        },
                    ))
                    .await;
                Some(Err("circuit breaker tripped".into()))
            }
        }
    }

    // ── Budget enforcement ────────────────────────────────────────────────────

    async fn load_budget_state(&self, execution_id: &ExecutionId) -> BudgetState {
        self.backend
            .latest_snapshot(execution_id)
            .await
            .ok()
            .flatten()
            .map(|s| BudgetState::from_snapshot_state(&s.state))
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_budget_after_execution(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
        ir: &WorkflowIr,
        budget: &mut BudgetState,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
        state_patch: &mut serde_json::Value,
    ) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        budget.accumulate(input_tokens, output_tokens, cost_usd);
        budget.patch_into_snapshot_state(state_patch);

        if let Some(tb) = &ir.token_budget {
            if let Some(limit) = tb.total_tokens {
                let limit = limit as u64;
                let current = budget.total_tokens();
                if current > limit {
                    return self
                        .emit_token_budget_exceeded(
                            execution_id,
                            node_id,
                            "total_tokens",
                            limit,
                            current,
                        )
                        .await;
                }
            }
            if let Some(limit) = tb.input_tokens {
                let limit = limit as u64;
                if budget.total_input_tokens > limit {
                    return self
                        .emit_token_budget_exceeded(
                            execution_id,
                            node_id,
                            "input_tokens",
                            limit,
                            budget.total_input_tokens,
                        )
                        .await;
                }
            }
            if let Some(limit) = tb.output_tokens {
                let limit = limit as u64;
                if budget.total_output_tokens > limit {
                    return self
                        .emit_token_budget_exceeded(
                            execution_id,
                            node_id,
                            "output_tokens",
                            limit,
                            budget.total_output_tokens,
                        )
                        .await;
                }
            }
        }

        if let Some(cost_limit) = ir.cost_budget_usd {
            if budget.total_cost_usd > cost_limit {
                warn!(
                    execution_id = %execution_id,
                    node_id,
                    cost_limit,
                    current = budget.total_cost_usd,
                    "Cost budget exceeded"
                );
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::CostBudgetExceeded {
                            node_id: node_id.to_string(),
                            limit_usd: cost_limit,
                            current_usd: budget.total_cost_usd,
                        },
                    ))
                    .await;
                return Some(Err(format!(
                    "cost budget exceeded: ${:.4} > ${:.4}",
                    budget.total_cost_usd, cost_limit
                )
                .into()));
            }
        }

        None
    }

    async fn emit_token_budget_exceeded(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
        kind: &str,
        limit: u64,
        current: u64,
    ) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        warn!(execution_id = %execution_id, node_id, kind, limit, current, "Token budget exceeded");
        let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
        let _ = self
            .backend
            .append_event(jamjet_state::Event::new(
                execution_id.clone(),
                seq,
                EventKind::TokenBudgetExceeded {
                    node_id: node_id.to_string(),
                    kind: kind.to_string(),
                    limit,
                    current,
                },
            ))
            .await;
        Some(Err(format!(
            "token budget exceeded: {kind} {current} > {limit}"
        )
        .into()))
    }

    // ── IR loading ────────────────────────────────────────────────────────────

    async fn load_ir(
        &self,
        workflow_id: &str,
        workflow_version: &str,
    ) -> Result<WorkflowIr, String> {
        let def = self
            .backend
            .get_workflow(workflow_id, workflow_version)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("workflow {workflow_id} v{workflow_version} not found"))?;
        serde_json::from_value(def.ir).map_err(|e| e.to_string())
    }
}

fn parse_payload(payload: &serde_json::Value) -> (String, String) {
    let workflow_id = payload
        .get("workflow_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let workflow_version = payload
        .get("workflow_version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0")
        .to_string();
    (workflow_id, workflow_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutionResult, NodeExecutor};
    use chrono::Utc;
    use jamjet_core::workflow::{ExecutionId, WorkflowExecution, WorkflowStatus};
    use jamjet_state::{
        backend::{StateBackend, WorkItem, WorkflowDefinition},
        event::EventKind,
        InMemoryBackend,
    };
    use std::sync::Arc;
    use uuid::Uuid;

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Minimal workflow IR with one tool node (no executor registered →
    /// worker falls through to the stub path which succeeds instantly).
    fn minimal_ir_json() -> serde_json::Value {
        serde_json::json!({
            "workflow_id": "test-wf",
            "version": "1.0.0",
            "state_schema": "{}",
            "start_node": "n1",
            "nodes": {
                "n1": {
                    "id": "n1",
                    "kind": {
                        "type": "tool",
                        "tool_ref": "stub",
                        "input_mapping": {},
                        "output_schema": "{}"
                    }
                }
            },
            "edges": [],
            "retry_policies": {},
            "models": {},
            "tools": {},
            "mcp_servers": {},
            "remote_agents": {}
        })
    }

    fn sample_execution(eid: &ExecutionId) -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            execution_id: eid.clone(),
            workflow_id: "test-wf".into(),
            workflow_version: "1.0.0".into(),
            status: WorkflowStatus::Running,
            initial_input: serde_json::json!({}),
            current_state: serde_json::json!({}),
            started_at: now,
            updated_at: now,
            completed_at: None,
            session_type: None,
        }
    }

    fn sample_item(eid: &ExecutionId) -> WorkItem {
        WorkItem {
            id: Uuid::new_v4(),
            execution_id: eid.clone(),
            node_id: "n1".into(),
            queue_type: "default".into(),
            payload: serde_json::json!({
                "workflow_id": "test-wf",
                "workflow_version": "1.0.0"
            }),
            attempt: 0,
            max_attempts: 3,
            created_at: Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            tenant_id: "default".into(),
            lease_fence: 0,
        }
    }

    async fn setup_backend() -> (Arc<InMemoryBackend>, ExecutionId) {
        let backend = Arc::new(InMemoryBackend::new());
        let eid = ExecutionId::new();
        backend
            .create_execution(sample_execution(&eid))
            .await
            .unwrap();
        backend
            .store_workflow(WorkflowDefinition {
                workflow_id: "test-wf".into(),
                version: "1.0.0".into(),
                ir: minimal_ir_json(),
                created_at: Utc::now(),
                tenant_id: "default".into(),
            })
            .await
            .unwrap();
        backend.enqueue_work_item(sample_item(&eid)).await.unwrap();
        (backend, eid)
    }

    fn make_worker(backend: Arc<InMemoryBackend>) -> Worker {
        Worker::new("test-worker".into(), backend, vec!["default".into()])
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Happy path: the worker routes completion through `commit_node_terminal`
    /// (fenced), producing exactly one `NodeCompleted` event.
    ///
    /// Note: this test checks that a single NodeCompleted is emitted; it is NOT
    /// the fence-bypass guard. The old unfenced path (`complete_work_item` +
    /// `append_event`) also emitted exactly one NodeCompleted, so reverting to
    /// it would not break this assertion. The real fence-bypass guard is
    /// `zombie_commit_is_rejected_emits_no_node_completed`, which exercises the
    /// stale-fence rejection path.
    #[tokio::test]
    async fn happy_path_emits_exactly_one_node_completed() {
        let (backend, eid) = setup_backend().await;
        let worker = make_worker(Arc::clone(&backend));

        // Claim the item so we have a valid leased item to pass to execute_item.
        let item = backend
            .claim_work_item("test-worker", &["default"])
            .await
            .unwrap()
            .expect("item must be claimable");

        // execute_item is private but accessible from this child module.
        worker.execute_item(item).await.unwrap();

        let events = backend.get_events(&eid).await.unwrap();
        let completed_count = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::NodeCompleted { .. }))
            .count();
        assert_eq!(
            completed_count, 1,
            "expected exactly one NodeCompleted event; got {completed_count}"
        );
    }

    // ── Failure-path executor stub ────────────────────────────────────────────

    /// A stub executor that always returns an error.  Used to drive the
    /// `Err(error)` arm inside `execute_item`, which must emit exactly one
    /// `NodeFailed` event via the fenced `commit_node_terminal` path.
    struct FailingExecutor;

    #[async_trait::async_trait]
    impl NodeExecutor for FailingExecutor {
        async fn execute(
            &self,
            _item: &jamjet_state::backend::WorkItem,
        ) -> Result<ExecutionResult, String> {
            Err("boom".into())
        }
    }

    /// Failure path: when the executor returns `Err`, `execute_item` must emit
    /// exactly one `NodeFailed` event through the fenced `commit_node_terminal`
    /// path and return `Ok(())` (node failure is not a worker error).
    #[tokio::test]
    async fn failure_path_emits_exactly_one_node_failed() {
        let (backend, eid) = setup_backend().await;
        // Register a failing executor for "tool" — the kind_tag of the node in
        // minimal_ir_json. This drives the Err(error) arm in execute_item.
        let worker = Worker::new(
            "test-worker".into(),
            backend.clone() as Arc<dyn StateBackend>,
            vec!["default".into()],
        )
        .register_executor("tool", Arc::new(FailingExecutor));

        let item = backend
            .claim_work_item("test-worker", &["default"])
            .await
            .unwrap()
            .expect("item must be claimable");

        // execute_item returns Ok(()) even when the node fails — node failure is
        // surfaced as a NodeFailed event, not a worker-level error.
        worker.execute_item(item).await.unwrap();

        let events = backend.get_events(&eid).await.unwrap();
        let failed_count = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::NodeFailed { .. }))
            .count();
        assert_eq!(
            failed_count, 1,
            "expected exactly one NodeFailed event; got {failed_count}"
        );
    }

    /// After a successful node completion the worker must write a per-turn
    /// snapshot through `commit_turn`.  The snapshot must (a) record the
    /// just-completed node in `completed_nodes` and (b) carry the real
    /// `at_sequence` assigned to the `NodeCompleted` event.
    #[tokio::test]
    async fn success_writes_snapshot_with_completed_node() {
        let (backend, eid) = setup_backend().await;
        let worker = make_worker(Arc::clone(&backend));

        let item = backend
            .claim_work_item("test-worker", &["default"])
            .await
            .unwrap()
            .expect("item must be claimable");

        worker.execute_item(item).await.unwrap();

        // Snapshot must exist.
        let snap = backend
            .latest_snapshot(&eid)
            .await
            .unwrap()
            .expect("snapshot must be Some after successful node completion");

        // n1 must appear in completed_nodes.
        assert!(
            snap.completed_nodes.contains_key("n1"),
            "snapshot must record n1 in completed_nodes; got {:?}",
            snap.completed_nodes.keys().collect::<Vec<_>>()
        );

        // at_sequence must match the NodeCompleted event's sequence.
        let events = backend.get_events(&eid).await.unwrap();
        let completed_seq = events
            .iter()
            .find(|e| matches!(e.kind, EventKind::NodeCompleted { .. }))
            .map(|e| e.sequence)
            .expect("NodeCompleted event must exist");
        assert_eq!(
            snap.at_sequence, completed_seq,
            "snapshot.at_sequence must equal the NodeCompleted event's sequence"
        );
    }

    /// Zombie path: a worker holding a stale fence (F1) tries to commit after
    /// the lease was stolen (the item now carries fence F2). The fenced
    /// `commit_node_terminal` must reject the zombie's write with FenceLost;
    /// the worker abandons quietly and emits zero `NodeCompleted` events.
    ///
    /// This is the key fence-bypass guard: even if two workers race to complete
    /// the same work item, only one can commit — the one whose fence matches the
    /// current lease. The zombie returns `Ok(())` (silent abandon) rather than
    /// double-writing.
    #[tokio::test]
    async fn zombie_commit_is_rejected_emits_no_node_completed() {
        let (backend, eid) = setup_backend().await;
        let worker = make_worker(Arc::clone(&backend));

        // Worker A claims the item and gets fence F1.
        let item_a = backend
            .claim_work_item("test-worker", &["default"])
            .await
            .unwrap()
            .expect("item must be claimable");
        let f1 = item_a.lease_fence;
        assert!(f1 > 0, "fence must be non-zero after claim");

        // Steal the lease: fail_work_item resets worker_id + bumps the epoch,
        // making the item re-claimable.
        backend
            .fail_work_item(item_a.id, "lease stolen for test")
            .await
            .unwrap();

        // Worker B claims and gets fence F2 > F1.
        let item_b = backend
            .claim_work_item("worker-B", &["default"])
            .await
            .unwrap()
            .expect("item must be re-claimable after fail");
        assert!(item_b.lease_fence > f1, "F2 must be > F1");

        // Drive Worker A's completion with the stale item (fence F1).
        // The fenced commit must detect the mismatch and return Ok(()) quietly.
        let result = worker.execute_item(item_a).await;
        assert!(
            result.is_ok(),
            "zombie execute_item must not propagate FenceLost as error"
        );

        // No NodeCompleted event must have been written by the zombie.
        let events = backend.get_events(&eid).await.unwrap();
        let completed_count = events
            .iter()
            .filter(|e| matches!(e.kind, EventKind::NodeCompleted { .. }))
            .count();
        assert_eq!(
            completed_count, 0,
            "zombie must not emit NodeCompleted; got {completed_count}"
        );
    }
}
