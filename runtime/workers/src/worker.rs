use crate::executor::{ExecutionResult, NodeExecutor};
use crate::heartbeat::spawn_heartbeat;
use jamjet_agents::AgentRegistry;
use jamjet_core::node::NodeKind;
use jamjet_core::workflow::ExecutionId;
use jamjet_ir::workflow::{NodeDef, WorkflowIr};
use jamjet_policy::autonomy::{AutonomyContext, AutonomyDecision, AutonomyEnforcer};
use jamjet_policy::engine::node_kind_tag;
use jamjet_policy::{EvaluationContext, PolicyDecision, PolicyEvaluator};
use jamjet_state::backend::{StateBackend, WorkItem};
use jamjet_state::budget::BudgetState;
use jamjet_state::event::EventKind;
use jamjet_telemetry::{gen_ai_attrs, record_gen_ai_usage};
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
        let attempt = item.attempt;
        let item_id = item.id;

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
                        .check_policy(&execution_id, &node_id, kind, node_def, &ir)
                        .await
                    {
                        heartbeat.abort();
                        return r;
                    }

                    // Autonomy check.
                    if let Some(r) = self
                        .check_autonomy(&execution_id, &node_id, kind, &budget)
                        .await
                    {
                        heartbeat.abort();
                        return r;
                    }

                    // Execute.
                    let exec_result = match self.executors.get(&kind_tag) {
                        Some(executor) if kind_tag == "agent_tool" => {
                            let (tx, mut rx) =
                                tokio::sync::mpsc::channel::<serde_json::Value>(64);
                            let backend = Arc::clone(&self.backend);
                            let eid = execution_id.clone();
                            let receiver_handle = tokio::spawn(async move {
                                while let Some(event) = rx.recv().await {
                                    if let Err(e) = backend
                                        .patch_append_array(
                                            &eid,
                                            "agent_tool_events",
                                            event,
                                        )
                                        .await
                                    {
                                        warn!("patch_append_array failed: {e}");
                                    }
                                }
                            });
                            let result = executor.execute_streaming(&item, tx).await;
                            let _ = receiver_handle.await;
                            result
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
                }
                if let Some(finish_reason) = &exec_result.finish_reason {
                    node_span.record(
                        gen_ai_attrs::RESPONSE_FINISH_REASONS,
                        finish_reason.as_str(),
                    );
                }

                self.backend.complete_work_item(item_id).await?;

                let seq = self.backend.latest_sequence(&execution_id).await? + 1;
                self.backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
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
                    ))
                    .await?;

                info!(execution_id = %execution_id, node_id = %node_id, duration_ms, "Node completed");
            }
            Err(error) => {
                self.backend.fail_work_item(item_id, &error).await?;
                let seq = self.backend.latest_sequence(&execution_id).await? + 1;
                self.backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::NodeFailed {
                            node_id: node_id.clone(),
                            error: error.clone(),
                            attempt,
                            retryable: false,
                        },
                    ))
                    .await?;
                warn!(execution_id = %execution_id, node_id = %node_id, attempt, %error, "Node failed");
            }
        }
        Ok(())
    }

    // ── Policy check ──────────────────────────────────────────────────────────

    async fn check_policy(
        &self,
        execution_id: &ExecutionId,
        node_id: &str,
        kind: &NodeKind,
        node_def: &NodeDef,
        ir: &WorkflowIr,
    ) -> Option<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
        let ctx = EvaluationContext::from_node_kind(node_id, kind);
        let mut sets: Vec<&jamjet_ir::workflow::PolicySetIr> = Vec::new();
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
                warn!(execution_id = %execution_id, node_id, %reason, "Policy blocked node");
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::PolicyViolation {
                            node_id: node_id.to_string(),
                            rule: reason.clone(),
                            decision: "blocked".to_string(),
                            policy_scope: "workflow".to_string(),
                        },
                    ))
                    .await;
                Some(Err(format!("policy blocked: {reason}").into()))
            }

            PolicyDecision::RequireApproval { approver } => {
                info!(execution_id = %execution_id, node_id, %approver, "Node requires approval");
                let tool_name = ctx.tool_name.unwrap_or_else(|| node_id.to_string());
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::ToolApprovalRequired {
                            node_id: node_id.to_string(),
                            tool_name,
                            approver,
                            context: serde_json::json!({ "node_id": node_id }),
                        },
                    ))
                    .await;
                Some(Ok(()))
            }
        }
    }

    // ── Autonomy check ────────────────────────────────────────────────────────

    async fn check_autonomy(
        &self,
        execution_id: &ExecutionId,
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

            AutonomyDecision::RequireToolApproval { tool_name } => {
                let seq = self.backend.latest_sequence(execution_id).await.ok()? + 1;
                let _ = self
                    .backend
                    .append_event(jamjet_state::Event::new(
                        execution_id.clone(),
                        seq,
                        EventKind::ToolApprovalRequired {
                            node_id: node_id.to_string(),
                            tool_name,
                            approver: "human".to_string(),
                            context: serde_json::json!({ "agent_ref": agent_ref }),
                        },
                    ))
                    .await;
                Some(Ok(()))
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
