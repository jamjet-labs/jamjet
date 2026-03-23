//! Executor for `A2aTask` workflow nodes.
//!
//! When a workflow node has kind `A2aTask`, this executor:
//! 1. Resolves the remote agent URI from the IR.
//! 2. Submits a task via the A2A client.
//! 3. Polls (or SSE-streams) for completion.
//! 4. Maps artifacts into the node output and workflow state patch.
//!
//! The executor is crash-resumable: if the worker dies mid-poll, the scheduler
//! will reclaim the lease and re-submit the task on the next attempt.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_a2a_proto::A2aMessage as Message;
use jamjet_a2a_proto::A2aPart as Part;
use jamjet_a2a_proto::{
    A2aArtifact, A2aClient, A2aTaskState, PartContent, Role, SendMessageRequest,
    SendMessageResponse,
};
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

/// Executor for `a2a_task` workflow nodes.
pub struct A2aTaskExecutor {
    client: A2aClient,
    /// Default poll interval when SSE is not available.
    poll_interval: Duration,
}

impl A2aTaskExecutor {
    pub fn new() -> Self {
        Self {
            client: A2aClient::new(),
            poll_interval: Duration::from_secs(2),
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }
}

impl Default for A2aTaskExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NodeExecutor for A2aTaskExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();

        // Extract agent URI and skill from the work item payload.
        let agent_uri = item
            .payload
            .get("agent_uri")
            .and_then(|v| v.as_str())
            .ok_or("A2aTaskExecutor: missing 'agent_uri' in payload")?;

        let skill = item
            .payload
            .get("skill")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        let input = item.payload.get("input").cloned().unwrap_or(json!({}));

        let task_id = item
            .payload
            .get("task_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        debug!(
            agent_uri = %agent_uri,
            skill = %skill,
            task_id = %task_id,
            "Submitting A2A task"
        );

        // Open a protocol-level span for A2A round-trip tracking (H2.4, H2.5, H2.9).
        let a2a_span = tracing::info_span!(
            "jamjet.a2a_task",
            "jamjet.tool.protocol" = "a2a",
            "jamjet.a2a.agent_uri" = %agent_uri,
            "jamjet.a2a.skill" = %skill,
            "jamjet.a2a.task_id" = %task_id,
        );
        let _a2a_guard = a2a_span.enter();

        // Build the message using the published crate's types.
        let mut metadata_map = HashMap::new();
        metadata_map.insert("skill".to_string(), json!(skill));

        let message = Message {
            message_id: Uuid::new_v4().to_string(),
            context_id: None,
            task_id: Some(task_id.clone()),
            role: Role::User,
            parts: vec![Part {
                content: PartContent::Data(input),
                metadata: None,
                filename: None,
                media_type: None,
            }],
            metadata: Some(metadata_map),
            extensions: vec![],
            reference_task_ids: vec![],
        };

        let request = SendMessageRequest {
            tenant: None,
            message,
            configuration: None,
            metadata: None,
        };

        let submitted = self
            .client
            .send_message(agent_uri, request)
            .await
            .map_err(|e| format!("A2A task submission failed: {e}"))?;

        // Extract the task ID from the response (may differ from our generated one).
        let response_task_id = match &submitted {
            SendMessageResponse::Task(t) => t.id.clone(),
            SendMessageResponse::WrappedTask(w) => w.task.id.clone(),
            SendMessageResponse::Message(m) => m.task_id.clone().unwrap_or(task_id.clone()),
            SendMessageResponse::WrappedMessage(w) => {
                w.message.task_id.clone().unwrap_or(task_id.clone())
            }
        };

        debug!(task_id = %response_task_id, "A2A task submitted");

        // Poll until completion.
        let final_task = self
            .client
            .wait_for_completion(agent_uri, &response_task_id, self.poll_interval, None)
            .await
            .map_err(|e| format!("A2A task polling failed: {e}"))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        match final_task.status.state {
            A2aTaskState::Completed => {
                // Extract output from the first artifact.
                let output = extract_output(&final_task.artifacts);
                Ok(ExecutionResult {
                    output: output.clone(),
                    state_patch: json!({ "last_a2a_output": output }),
                    duration_ms,
                    gen_ai_system: None,
                    gen_ai_model: None,
                    input_tokens: None,
                    output_tokens: None,
                    finish_reason: Some("completed".into()),
                })
            }
            A2aTaskState::Failed => {
                let error = final_task
                    .status
                    .message
                    .as_ref()
                    .and_then(|m| {
                        m.parts.iter().find_map(|p| {
                            if let PartContent::Text(ref text) = p.content {
                                Some(text.clone())
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_else(|| "A2A task failed".into());
                Err(error)
            }
            A2aTaskState::InputRequired => {
                // The workflow should be paused for user input — return error
                // so the retry mechanism handles re-submission.
                warn!(task_id = %response_task_id, "A2A task requires input — not yet handled");
                Err("A2A task requires input — multi-turn not yet supported in this node".into())
            }
            other => Err(format!("A2A task ended in unexpected state: {other:?}")),
        }
    }
}

fn extract_output(artifacts: &[A2aArtifact]) -> Value {
    artifacts
        .first()
        .map(|a| {
            a.parts
                .iter()
                .map(|p| match &p.content {
                    PartContent::Data(data) => data.clone(),
                    PartContent::Text(text) => json!({ "text": text }),
                    PartContent::Url(url) => json!({ "uri": url }),
                    PartContent::Raw(_) => json!({ "type": "raw_binary" }),
                    _ => json!({}),
                })
                .next()
                .unwrap_or(json!({}))
        })
        .unwrap_or(json!({}))
}
