use crate::executor::{ExecutionResult, NodeExecutor};
use crate::heartbeat::spawn_heartbeat;
use jamjet_core::node::NodeKind;
use jamjet_ir::WorkflowIr;
use jamjet_state::backend::{StateBackend, WorkItem};
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
    queue_types: Vec<String>,
    poll_interval: Duration,
    /// Registered executors by node kind discriminant string (e.g. "tool", "model").
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
            queue_types,
            poll_interval: Duration::from_millis(500),
            executors: HashMap::new(),
        }
    }

    /// Register a node executor for a given kind tag (the serde tag value from `NodeKind`).
    pub fn register_executor(
        mut self,
        kind: impl Into<String>,
        executor: Arc<dyn NodeExecutor>,
    ) -> Self {
        self.executors.insert(kind.into(), executor);
        self
    }

    /// Run the worker loop. Runs indefinitely until the future is cancelled.
    pub async fn run(&self) {
        info!(
            worker_id = %self.worker_id,
            queues = ?self.queue_types,
            "Worker started"
        );
        loop {
            match self.poll_and_execute().await {
                Ok(true) => {} // processed a work item, poll again immediately
                Ok(false) => {
                    tokio::time::sleep(self.poll_interval).await;
                }
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
            queue = %item.queue_type,
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

        // Create a node-level span with OTel GenAI semantic convention fields.
        // Field names must be string literals in the macro; values are filled in via span.record().
        let node_span = tracing::info_span!(
            "jamjet.node",
            "jamjet.execution.id" = %execution_id,
            "jamjet.node.id" = %node_id,
            "jamjet.attempt" = attempt,
            "jamjet.worker.id" = %self.worker_id,
            // Populated after IR load:
            "jamjet.workflow.id" = tracing::field::Empty,
            "jamjet.workflow.version" = tracing::field::Empty,
            "jamjet.node.kind" = tracing::field::Empty,
            // Populated for model nodes:
            "gen_ai.system" = tracing::field::Empty,
            "gen_ai.request.model" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
        );
        let _span_guard = node_span.enter();

        // Emit NodeStarted event.
        let seq = self.backend.latest_sequence(&execution_id).await? + 1;
        let started_event = jamjet_state::Event::new(
            execution_id.clone(),
            seq,
            EventKind::NodeStarted {
                node_id: node_id.clone(),
                worker_id: self.worker_id.clone(),
                attempt,
            },
        );
        self.backend.append_event(started_event).await?;

        // Spawn heartbeat to keep the lease alive during execution.
        let heartbeat = spawn_heartbeat(
            Arc::clone(&self.backend),
            item_id,
            self.worker_id.clone(),
            Duration::from_secs(15),
        );

        // Load workflow IR to get the node definition.
        let (workflow_id, workflow_version) = parse_payload(&item.payload);
        node_span.record(gen_ai_attrs::JAMJET_WORKFLOW_ID, workflow_id.as_str());
        node_span.record(
            gen_ai_attrs::JAMJET_WORKFLOW_VERSION,
            workflow_version.as_str(),
        );
        let node_kind = self
            .load_node_kind(&workflow_id, &workflow_version, &node_id)
            .await;

        // Dispatch to the appropriate executor.
        let result = match node_kind {
            Ok(kind) => {
                let kind_tag = node_kind_tag(&kind);
                node_span.record(gen_ai_attrs::JAMJET_NODE_KIND, kind_tag.as_str());
                match self.executors.get(&kind_tag) {
                    Some(executor) => executor.execute(&item).await,
                    None => {
                        // No registered executor — use a pass-through for unknown/stub kinds.
                        info!(node_id = %node_id, kind = %kind_tag, "No executor registered; using stub");
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
                }
            }
            Err(e) => Err(format!("failed to load node kind: {e}")),
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        // Stop heartbeat regardless of outcome.
        heartbeat.abort();

        match result {
            Ok(exec_result) => {
                // Record GenAI span attributes if this was a model call.
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

                // Mark work item complete.
                self.backend.complete_work_item(item_id).await?;

                // Emit NodeCompleted event (includes GenAI telemetry for model nodes).
                let seq = self.backend.latest_sequence(&execution_id).await? + 1;
                let completed_event = jamjet_state::Event::new(
                    execution_id.clone(),
                    seq,
                    EventKind::NodeCompleted {
                        node_id: node_id.clone(),
                        output: exec_result.output,
                        state_patch: exec_result.state_patch,
                        duration_ms,
                        gen_ai_system: exec_result.gen_ai_system.clone(),
                        gen_ai_model: exec_result.gen_ai_model.clone(),
                        input_tokens: exec_result.input_tokens,
                        output_tokens: exec_result.output_tokens,
                        finish_reason: exec_result.finish_reason.clone(),
                        cost_usd: None, // computed by model adapter in Phase 2
                    },
                );
                self.backend.append_event(completed_event).await?;

                info!(
                    execution_id = %execution_id,
                    node_id = %node_id,
                    duration_ms,
                    "Node completed"
                );
            }
            Err(error) => {
                // Mark work item failed.
                self.backend.fail_work_item(item_id, &error).await?;

                // Emit NodeFailed event.
                let seq = self.backend.latest_sequence(&execution_id).await? + 1;
                let failed_event = jamjet_state::Event::new(
                    execution_id.clone(),
                    seq,
                    EventKind::NodeFailed {
                        node_id: node_id.clone(),
                        error: error.clone(),
                        attempt,
                        retryable: false, // TODO: check retry policy
                    },
                );
                self.backend.append_event(failed_event).await?;

                warn!(
                    execution_id = %execution_id,
                    node_id = %node_id,
                    attempt,
                    %error,
                    "Node failed"
                );
            }
        }

        Ok(())
    }

    async fn load_node_kind(
        &self,
        workflow_id: &str,
        workflow_version: &str,
        node_id: &str,
    ) -> Result<NodeKind, String> {
        let def = self
            .backend
            .get_workflow(workflow_id, workflow_version)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("workflow {workflow_id} v{workflow_version} not found"))?;

        let ir: WorkflowIr = serde_json::from_value(def.ir).map_err(|e| e.to_string())?;
        let node_def = ir
            .node(node_id)
            .ok_or_else(|| format!("node {node_id} not found in IR"))?;

        Ok(node_def.kind.clone())
    }
}

/// Extract workflow_id and workflow_version from a work item payload.
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

/// Map a `NodeKind` to its serde tag string (for executor registry lookup).
fn node_kind_tag(kind: &NodeKind) -> String {
    // Serialize to JSON and extract the "type" field from the tagged enum.
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}
