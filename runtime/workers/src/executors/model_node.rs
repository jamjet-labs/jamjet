//! Executor for `Model` workflow nodes.
//!
//! Resolves the model configuration from the workflow IR, calls the appropriate
//! `ModelAdapter` via `ModelRegistry`, and records GenAI telemetry.

use crate::executor::{ExecutionResult, NodeExecutor};
use async_trait::async_trait;
use jamjet_models::{ChatMessage, ModelConfig, ModelRegistry, ModelRequest};
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, instrument};

/// Executor for `model` workflow nodes.
pub struct ModelNodeExecutor {
    registry: Arc<ModelRegistry>,
}

impl ModelNodeExecutor {
    pub fn new(registry: Arc<ModelRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl NodeExecutor for ModelNodeExecutor {
    #[instrument(skip(self, item), fields(node_id = %item.node_id))]
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, String> {
        let start = std::time::Instant::now();

        // Extract model config from the work item payload.
        // The payload is populated by the scheduler from the IR node definition.
        let model = item
            .payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-6")
            .to_string();

        let system_prompt = item
            .payload
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let max_tokens = item
            .payload
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);

        let temperature = item
            .payload
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32);

        // Build the messages from state and payload.
        // The `prompt` field in payload may reference workflow state via template strings.
        let prompt = item
            .payload
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let mut messages = Vec::new();
        if !prompt.is_empty() {
            messages.push(ChatMessage::user(prompt));
        } else {
            // Use any explicit messages array from payload.
            if let Some(msgs) = item.payload.get("messages").and_then(|v| v.as_array()) {
                for msg in msgs {
                    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                    let content = msg
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    match role {
                        "system" => messages.push(ChatMessage::system(content)),
                        "assistant" => messages.push(ChatMessage::assistant(content)),
                        _ => messages.push(ChatMessage::user(content)),
                    }
                }
            }
        }

        if messages.is_empty() {
            return Err("ModelNodeExecutor: no prompt or messages provided".into());
        }

        let config = ModelConfig {
            model: Some(model.clone()),
            max_tokens,
            temperature,
            system_prompt,
            stop_sequences: None,
        };

        debug!(model = %model, messages = messages.len(), "Calling model");

        let request = ModelRequest::new(messages).with_config(config);
        let response = self
            .registry
            .chat(request)
            .await
            .map_err(|e| format!("Model call failed: {e}"))?;

        let duration_ms = start.elapsed().as_millis() as u64;

        // Build output — structured or plain text.
        let output: Value = json!({
            "content": response.content,
            "model": response.model,
            "finish_reason": response.finish_reason,
        });

        // State patch: write content to `last_model_output` in workflow state.
        let state_patch = json!({
            "last_model_output": response.content,
        });

        Ok(ExecutionResult {
            output,
            state_patch,
            duration_ms,
            gen_ai_system: Some(
                // Infer system from model name prefix.
                if response.model.starts_with("claude") {
                    "anthropic"
                } else if response.model.starts_with("gpt") || response.model.starts_with("o1") {
                    "openai"
                } else {
                    "unknown"
                }
                .to_string(),
            ),
            gen_ai_model: Some(response.model),
            input_tokens: Some(response.input_tokens),
            output_tokens: Some(response.output_tokens),
            finish_reason: Some(response.finish_reason),
        })
    }
}
