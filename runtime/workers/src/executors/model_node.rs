//! Executor for `Model` workflow nodes.
//!
//! Resolves the model configuration from the workflow IR, calls the appropriate
//! `ModelAdapter` via `ModelRegistry`, and records GenAI telemetry.

use crate::executor::{ExecutionResult, ExecutorError, NodeExecutor};
use async_trait::async_trait;
use jamjet_models::adapter::{ChatRole, ToolCall};
use jamjet_models::{ChatMessage, ModelConfig, ModelError, ModelRegistry, ModelRequest};
use jamjet_state::backend::WorkItem;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, instrument};

/// Map one conversation message from the payload (OpenAI shape) to a
/// `ChatMessage`, keeping the assistant's tool calls and the tool call id.
fn chat_message_from_json(msg: &Value) -> ChatMessage {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let content = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    match role {
        "system" => ChatMessage::system(content),
        "assistant" => {
            let calls: Vec<ToolCall> = msg
                .get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(tool_call_from_json).collect())
                .unwrap_or_default();
            ChatMessage::assistant_with_tool_calls(content, calls)
        }
        "tool" => {
            let id = msg
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            ChatMessage::tool(id, content)
        }
        _ => ChatMessage::user(content),
    }
}

/// Accepts the OpenAI shape `{id, function: {name, arguments}}` or a flat
/// `{id, name, arguments}`. A string `arguments` is parsed as JSON and kept as
/// a string when it does not parse.
fn tool_call_from_json(tc: &Value) -> ToolCall {
    let str_of = |v: Option<&Value>| v.and_then(|x| x.as_str()).unwrap_or("").to_string();
    let func = tc.get("function");
    let name = str_of(func.and_then(|f| f.get("name")).or_else(|| tc.get("name")));
    let raw = func
        .and_then(|f| f.get("arguments"))
        .or_else(|| tc.get("arguments"))
        .cloned()
        .unwrap_or(Value::Null);
    let arguments = match raw {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    };
    ToolCall {
        id: str_of(tc.get("id")),
        name,
        arguments,
    }
}

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

        // The scheduler could not produce the node's prompt (a template naming
        // state the run does not have, or a prompt file it cannot read). Fail
        // with that reason rather than call the model without the prompt.
        // Not retryable: the item's payload, and so the reason, is fixed.
        if let Some(reason) = item.payload.get("prompt_error").and_then(|v| v.as_str()) {
            return Err(ExecutorError::Fatal(format!("ModelNodeExecutor: {reason}")));
        }

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

        // Build the messages from the payload. `prompt` is the node's prompt,
        // already rendered against the run's state by the scheduler; when the
        // node has one it is the single user message. `messages` (the
        // conversation in state) is used only when the node has no prompt.
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
                    messages.push(chat_message_from_json(msg));
                }
            }
        }

        if messages.is_empty() {
            return Err("ModelNodeExecutor: no prompt or messages provided".into());
        }

        // A system message in the conversation already carries the prompt; do
        // not send it a second time.
        let system_prompt = if messages.iter().any(|m| matches!(m.role, ChatRole::System)) {
            None
        } else {
            system_prompt
        };

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

    /// Records the `ModelRequest` it is called with so a test can assert how the
    /// executor mapped the work-item payload (e.g. `messages`) into the request.
    struct CapturingAdapter {
        captured: Arc<std::sync::Mutex<Option<ModelRequest>>>,
    }

    #[async_trait::async_trait]
    impl ModelAdapter for CapturingAdapter {
        fn system_name(&self) -> &'static str {
            "capturing"
        }
        fn default_model(&self) -> &str {
            "fake-model"
        }
        async fn chat(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
            *self.captured.lock().unwrap() = Some(req);
            Ok(ModelResponse {
                content: "ok".to_string(),
                model: "fake-model".to_string(),
                finish_reason: "stop".to_string(),
                input_tokens: 1,
                output_tokens: 1,
                structured: None,
                tool_calls: vec![],
            })
        }
        async fn structured_output(
            &self,
            _req: StructuredRequest,
        ) -> Result<ModelResponse, ModelError> {
            unimplemented!()
        }
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

    /// A payload whose prompt the scheduler could not produce fails the node
    /// with the reason, and never reaches the model, even with a conversation
    /// in the payload to fall back on.
    #[tokio::test]
    async fn a_prompt_error_fails_the_node_without_calling_the_model() {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let registry = Arc::new(
            ModelRegistry::new()
                .register(Arc::new(CapturingAdapter {
                    captured: captured.clone(),
                }))
                .with_default("capturing"),
        );
        let executor = ModelNodeExecutor::new(registry);
        let mut item = make_item();
        item.payload = serde_json::json!({
            "model": "fake-model",
            "prompt_error": "prompt template: `{{ state.query }}`: the run's state has no `query`",
            "messages": [ { "role": "user", "content": "hi" } ]
        });
        match executor.execute(&item).await {
            Err(ExecutorError::Fatal(msg)) => assert!(msg.contains("no `query`"), "{msg}"),
            other => panic!("expected Fatal, got {other:?}"),
        }
        assert!(
            captured.lock().unwrap().is_none(),
            "the model must not be called"
        );
    }

    fn capturing_executor() -> (
        ModelNodeExecutor,
        Arc<std::sync::Mutex<Option<ModelRequest>>>,
    ) {
        let captured = Arc::new(std::sync::Mutex::new(None));
        let registry = Arc::new(
            ModelRegistry::new()
                .register(Arc::new(CapturingAdapter {
                    captured: captured.clone(),
                }))
                .with_default("capturing"),
        );
        (ModelNodeExecutor::new(registry), captured)
    }

    fn work_item(payload: Value) -> WorkItem {
        let mut item = make_item();
        item.payload = payload;
        item
    }

    /// B4: a durable agent turn must reach the model with the assistant's tool
    /// calls and each tool result tied to its call id, and one system message.
    #[tokio::test]
    async fn executor_keeps_tool_calls_and_tool_results() {
        let (executor, captured) = capturing_executor();
        let item = work_item(json!({
            "model": "openai/gpt-4o-mini",
            "system_prompt": "You book venues.",
            "messages": [
                {"role": "system", "content": "You book venues."},
                {"role": "user", "content": "Find a venue in Lisbon for 40."},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "search_venues", "arguments": "{\"city\": \"Lisbon\", \"guests\": 40}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "[\"Casa do Rio\"]"}
            ]
        }));
        executor
            .execute(&item)
            .await
            .expect("execute must not error");

        let req = captured.lock().unwrap().clone().expect("request captured");
        let roles: Vec<_> = req
            .messages
            .iter()
            .map(|m| format!("{:?}", m.role))
            .collect();
        assert_eq!(roles, ["System", "User", "Assistant", "Tool"]);
        assert_eq!(req.messages[2].tool_calls.len(), 1);
        assert_eq!(req.messages[2].tool_calls[0].id, "call_1");
        assert_eq!(req.messages[2].tool_calls[0].name, "search_venues");
        assert_eq!(
            req.messages[2].tool_calls[0].arguments,
            json!({"city": "Lisbon", "guests": 40})
        );
        assert_eq!(req.messages[3].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(req.messages[3].content, "[\"Casa do Rio\"]");
        assert!(
            req.config.system_prompt.is_none(),
            "messages hold the system prompt; do not send it twice"
        );
    }

    /// With no system message in the conversation, the node's system prompt is
    /// still sent.
    #[tokio::test]
    async fn executor_sends_system_prompt_when_messages_have_none() {
        let (executor, captured) = capturing_executor();
        let item = work_item(json!({
            "model": "fake-model",
            "system_prompt": "You book venues.",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        executor.execute(&item).await.expect("execute");
        let req = captured.lock().unwrap().clone().expect("request captured");
        assert_eq!(
            req.config.system_prompt.as_deref(),
            Some("You book venues.")
        );
    }

    /// Tool-call arguments that are not valid JSON are kept as a string; a flat
    /// `{id, name, arguments}` entry is accepted too.
    #[tokio::test]
    async fn executor_keeps_unparseable_arguments_as_string() {
        let (executor, captured) = capturing_executor();
        let item = work_item(json!({
            "model": "fake-model",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "f", "arguments": "{not json"}},
                    {"id": "c2", "name": "g", "arguments": {"a": 1}}
                ]}
            ]
        }));
        executor.execute(&item).await.expect("execute");
        let req = captured.lock().unwrap().clone().expect("request captured");
        let calls = &req.messages[1].tool_calls;
        assert_eq!(calls[0].arguments, json!("{not json"));
        assert_eq!(calls[1].name, "g");
        assert_eq!(calls[1].arguments, json!({"a": 1}));
    }
}
