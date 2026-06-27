//! Executor for `Model` workflow nodes.
//!
//! Resolves the model configuration from the workflow IR, calls the appropriate
//! `ModelAdapter` via `ModelRegistry`, and records GenAI telemetry.

use crate::executor::{ExecutionResult, ExecutorError, NodeExecutor};
use async_trait::async_trait;
use jamjet_models::{ChatMessage, ModelConfig, ModelError, ModelRegistry, ModelRequest};
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
    async fn execute(&self, item: &WorkItem) -> Result<ExecutionResult, ExecutorError> {
        let start = std::time::Instant::now();

        // Extract model config from the work item payload.
        // The payload is populated by the scheduler from the IR node definition.
        // Default to the fully-qualified provider-prefixed string so that:
        //  • the Python sidecar's parse_model_ref maps it to provider=anthropic
        //    (a bare "claude-sonnet-4-6" mis-maps to provider=openai — C2 fix).
        //  • the non-seam registry routes it via the "anthropic/" prefix route.
        let model = item
            .payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic/claude-sonnet-4-6")
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

        // Read tool schemas from the work-item payload (scheduler-enriched from the
        // IR Model node's `tools` field).
        let tools: Vec<serde_json::Value> = item
            .payload
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        debug!(model = %model, messages = messages.len(), "Calling model");

        let request = ModelRequest::new(messages)
            .with_config(config)
            .with_tools(tools);
        // Intercept RateLimited before erasing the type. ONLY RateLimited becomes
        // ExecutorError::RateLimited; every other ModelError variant becomes Fatal.
        let response = match self.registry.chat(request).await {
            Ok(r) => r,
            Err(ModelError::RateLimited { retry_after_secs }) => {
                return Err(ExecutorError::RateLimited { retry_after_secs });
            }
            Err(e) => return Err(ExecutorError::Fatal(format!("Model call failed: {e}"))),
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        // Serialize tool_calls to JSON values.  These are small (id/name/arguments
        // only) and are intentionally placed in state_patch so they NEVER get
        // spilled to the artifact store (state_patch stays inline per the 2i
        // spill comment in worker.rs:430-434).
        let tool_calls_json: Vec<serde_json::Value> = response
            .tool_calls
            .iter()
            .map(|tc| {
                serde_json::json!({
                    "id": tc.id,
                    "name": tc.name,
                    "arguments": tc.arguments,
                })
            })
            .collect();

        // Build output — carries content + tool_calls + finish_reason.
        // Note: if content is large, the output blob may be spilled to the
        // artifact store (2i).  tool_calls and finish_reason are ALSO written to
        // state_patch (see below) so the condition gate and tool nodes always
        // find them inline in current_state, even if the output was spilled.
        let output: Value = json!({
            "content": response.content,
            "model": response.model,
            "finish_reason": response.finish_reason,
            "tool_calls": tool_calls_json,
        });

        // State patch: write model outputs inline into workflow state.
        // Convention for 2j-3's condition gate and tool-dispatch nodes:
        //   state["last_model_finish_reason"] — "tool_calls", "stop", etc.
        //   state["last_model_tool_calls"]    — [{id, name, arguments}, ...]
        //   state["last_model_output"]        — content text (last model turn)
        // These keys are safe to overwrite per turn because the static unroll
        // executes model nodes sequentially; the condition gate for turn N always
        // reads after that turn's model node completes.
        let state_patch = json!({
            "last_model_output": response.content,
            "last_model_finish_reason": response.finish_reason,
            "last_model_tool_calls": tool_calls_json,
        });

        Ok(ExecutionResult {
            output,
            state_patch,
            duration_ms,
            gen_ai_system: Some(
                {
                    // Infer the provider from the model name, tolerating an optional
                    // "provider/" prefix (e.g. "anthropic/claude-sonnet-4-6" or bare
                    // "claude-sonnet-4-6" both classify as "anthropic").
                    let bare = response
                        .model
                        .split_once('/')
                        .map(|(_, m)| m)
                        .unwrap_or(response.model.as_str());
                    if response.model.starts_with("anthropic/") || bare.starts_with("claude") {
                        "anthropic"
                    } else if response.model.starts_with("openai/")
                        || bare.starts_with("gpt")
                        || bare.starts_with("o1")
                    {
                        "openai"
                    } else {
                        "unknown"
                    }
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutorError;
    use jamjet_models::adapter::ToolCall;
    use jamjet_models::{ModelAdapter, ModelError, ModelRequest, ModelResponse, StructuredRequest};
    use jamjet_state::backend::WorkItem;
    use std::sync::Arc;

    // ── Fake adapter ──────────────────────────────────────────────────────────

    #[allow(dead_code)]
    enum FakeKind {
        RateLimited { retry_after_secs: u64 },
        ApiError,
        ToolCallResponse,
        TextResponse,
    }

    struct FakeAdapter {
        kind: FakeKind,
    }

    #[async_trait::async_trait]
    impl ModelAdapter for FakeAdapter {
        fn system_name(&self) -> &'static str {
            "fake"
        }
        fn default_model(&self) -> &str {
            "fake-model"
        }
        async fn chat(&self, _req: ModelRequest) -> Result<ModelResponse, ModelError> {
            match &self.kind {
                FakeKind::RateLimited { retry_after_secs } => Err(ModelError::RateLimited {
                    retry_after_secs: *retry_after_secs,
                }),
                FakeKind::ApiError => Err(ModelError::Api {
                    status: 500,
                    body: "internal server error".into(),
                }),
                FakeKind::ToolCallResponse => Ok(ModelResponse {
                    content: "".to_string(),
                    model: "fake-model".to_string(),
                    finish_reason: "tool_calls".to_string(),
                    input_tokens: 10,
                    output_tokens: 5,
                    structured: None,
                    tool_calls: vec![ToolCall {
                        id: "call_abc123".to_string(),
                        name: "get_weather".to_string(),
                        arguments: serde_json::json!({"location": "London"}),
                    }],
                }),
                FakeKind::TextResponse => Ok(ModelResponse {
                    content: "Hello, world!".to_string(),
                    model: "fake-model".to_string(),
                    finish_reason: "stop".to_string(),
                    input_tokens: 10,
                    output_tokens: 5,
                    structured: None,
                    tool_calls: vec![],
                }),
            }
        }
        async fn structured_output(
            &self,
            _req: StructuredRequest,
        ) -> Result<ModelResponse, ModelError> {
            unimplemented!()
        }
    }

    fn make_item() -> WorkItem {
        WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: jamjet_core::workflow::ExecutionId::new(),
            node_id: "test-node".into(),
            queue_type: "model".into(),
            payload: serde_json::json!({
                "model": "fake-model",
                "prompt": "hello",
            }),
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "default".into(),
        }
    }

    fn make_item_with_tools() -> WorkItem {
        WorkItem {
            id: uuid::Uuid::new_v4(),
            execution_id: jamjet_core::workflow::ExecutionId::new(),
            node_id: "test-node".into(),
            queue_type: "model".into(),
            payload: serde_json::json!({
                "model": "fake-model",
                "prompt": "What is the weather in London?",
                "tools": [
                    {"type": "function", "function": {"name": "get_weather", "description": "Get weather", "parameters": {}}}
                ]
            }),
            attempt: 0,
            max_attempts: 3,
            created_at: chrono::Utc::now(),
            lease_expires_at: None,
            worker_id: None,
            lease_fence: 0,
            tenant_id: "default".into(),
        }
    }

    fn make_executor(kind: FakeKind) -> ModelNodeExecutor {
        let registry = Arc::new(
            ModelRegistry::new()
                .register(Arc::new(FakeAdapter { kind }))
                .with_default("fake"),
        );
        ModelNodeExecutor::new(registry)
    }

    /// ONLY ModelError::RateLimited maps to ExecutorError::RateLimited, with the
    /// correct retry_after_secs forwarded.
    #[tokio::test]
    async fn rate_limited_maps_to_executor_rate_limited() {
        let executor = make_executor(FakeKind::RateLimited {
            retry_after_secs: 7,
        });
        let result = executor.execute(&make_item()).await;
        assert!(
            matches!(
                result,
                Err(ExecutorError::RateLimited {
                    retry_after_secs: 7
                })
            ),
            "expected RateLimited(7), got: {:?}",
            result
        );
    }

    /// Non-rate-limit ModelErrors (Api, Network, Timeout, etc.) must map to
    /// ExecutorError::Fatal, never to RateLimited.
    #[tokio::test]
    async fn non_rate_limit_error_maps_to_fatal() {
        let executor = make_executor(FakeKind::ApiError);
        let result = executor.execute(&make_item()).await;
        assert!(
            matches!(result, Err(ExecutorError::Fatal(_))),
            "expected Fatal, got: {:?}",
            result
        );
    }

    /// When the model returns tool_calls, the output and state_patch must carry
    /// the serialized tool calls and the finish_reason "tool_calls".
    #[tokio::test]
    async fn tool_calls_response_lands_in_output_and_state_patch() {
        let executor = make_executor(FakeKind::ToolCallResponse);
        let result = executor
            .execute(&make_item_with_tools())
            .await
            .expect("ToolCallResponse must not error");

        // output["tool_calls"] must be an array with one entry.
        let tool_calls = result.output["tool_calls"]
            .as_array()
            .expect("output.tool_calls must be an array");
        assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
        assert_eq!(tool_calls[0]["id"], "call_abc123");
        assert_eq!(tool_calls[0]["name"], "get_weather");
        assert_eq!(tool_calls[0]["arguments"]["location"], "London");

        // output["finish_reason"] must be "tool_calls".
        assert_eq!(
            result.output["finish_reason"], "tool_calls",
            "output.finish_reason must be 'tool_calls'"
        );

        // state_patch must carry finish_reason and tool_calls inline.
        assert_eq!(
            result.state_patch["last_model_finish_reason"], "tool_calls",
            "state_patch.last_model_finish_reason must be 'tool_calls'"
        );
        let sp_tool_calls = result.state_patch["last_model_tool_calls"]
            .as_array()
            .expect("state_patch.last_model_tool_calls must be an array");
        assert_eq!(
            sp_tool_calls.len(),
            1,
            "expected exactly one tool call in state_patch"
        );
        assert_eq!(sp_tool_calls[0]["name"], "get_weather");
    }

    /// When the model returns a plain text response with no tool_calls, the
    /// output and state_patch must contain empty tool_calls arrays and
    /// finish_reason "stop".
    #[tokio::test]
    async fn no_tool_calls_response_has_empty_tool_calls() {
        let executor = make_executor(FakeKind::TextResponse);
        let result = executor
            .execute(&make_item())
            .await
            .expect("TextResponse must not error");

        // output["tool_calls"] must be an empty array.
        let tool_calls = result.output["tool_calls"]
            .as_array()
            .expect("output.tool_calls must be an array");
        assert!(tool_calls.is_empty(), "expected empty tool_calls array");

        // output["finish_reason"] must be "stop".
        assert_eq!(
            result.output["finish_reason"], "stop",
            "output.finish_reason must be 'stop'"
        );

        // state_patch must carry finish_reason "stop" and empty tool_calls.
        assert_eq!(
            result.state_patch["last_model_finish_reason"], "stop",
            "state_patch.last_model_finish_reason must be 'stop'"
        );
        let sp_tool_calls = result.state_patch["last_model_tool_calls"]
            .as_array()
            .expect("state_patch.last_model_tool_calls must be an array");
        assert!(
            sp_tool_calls.is_empty(),
            "expected empty tool_calls in state_patch"
        );
    }
}
