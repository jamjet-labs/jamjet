//! Unified model adapter trait and shared types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("provider API error ({status}): {body}")]
    Api { status: u16, body: String },

    #[error("rate limited — retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("context window exceeded: {input_tokens} tokens > {limit} limit")]
    ContextWindowExceeded { input_tokens: u64, limit: u64 },

    #[error("network error: {0}")]
    Network(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("timeout")]
    Timeout,
}

// ── Shared types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
        }
    }
}

/// Configuration for a single model call (overrides adapter defaults).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Model name (e.g. "claude-sonnet-4-6", "gpt-4o").
    pub model: Option<String>,
    /// Max tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature (0.0–1.0).
    pub temperature: Option<f32>,
    /// System prompt to prepend (overrides messages).
    pub system_prompt: Option<String>,
    /// Stop sequences.
    pub stop_sequences: Option<Vec<String>>,
}

/// A tool call returned by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-issued call id (e.g. "call_abc123").
    pub id: String,
    /// Name of the tool to invoke (matches a name in the request `tools` list).
    pub name: String,
    /// Arguments as a JSON value.  Providers (and the Python sidecar) normalise
    /// the arguments string to a JSON object when possible; callers should
    /// tolerate a `Value::String` if the model emits malformed JSON.
    pub arguments: serde_json::Value,
}

/// A request to a chat model.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub config: ModelConfig,
    /// OpenAI-format tool/function schemas passed to the model.  Empty means no
    /// tools are offered to the model for this call.
    pub tools: Vec<serde_json::Value>,
}

impl ModelRequest {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            messages,
            config: ModelConfig::default(),
            tools: vec![],
        }
    }

    pub fn with_config(mut self, config: ModelConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_tools(mut self, tools: Vec<serde_json::Value>) -> Self {
        self.tools = tools;
        self
    }
}

/// A request for structured (JSON) output.
#[derive(Debug, Clone)]
pub struct StructuredRequest {
    pub messages: Vec<ChatMessage>,
    pub config: ModelConfig,
    /// JSON Schema describing the expected output object.
    pub output_schema: serde_json::Value,
}

/// A response from a chat model.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// The generated text content.  Empty string when finish_reason is
    /// "tool_calls" (the model is requesting tool invocations, not producing text).
    pub content: String,
    /// The model that actually served the request (may differ from requested).
    pub model: String,
    /// Finish reason: "stop", "length", "tool_calls", "content_filter".
    pub finish_reason: String,
    /// Input tokens consumed.
    pub input_tokens: u64,
    /// Output tokens generated.
    pub output_tokens: u64,
    /// Structured output parsed from JSON (for `structured_output()` calls).
    pub structured: Option<serde_json::Value>,
    /// Tool calls requested by the model.  Empty when finish_reason != "tool_calls".
    pub tool_calls: Vec<ToolCall>,
}

// ── Trait ─────────────────────────────────────────────────────────────────────

/// Unified interface for LLM providers.
///
/// Implement this trait to add a new model provider.
/// The `system` string returned by `system_name()` is used as the
/// `gen_ai.system` OTel attribute.
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    /// OTel GenAI system name (e.g. "anthropic", "openai").
    fn system_name(&self) -> &'static str;

    /// Default model for this adapter (e.g. "claude-sonnet-4-6").
    fn default_model(&self) -> &str;

    /// Send a chat request and return the response.
    async fn chat(&self, request: ModelRequest) -> Result<ModelResponse, ModelError>;

    /// Send a structured output request, returning a JSON value.
    ///
    /// The response is validated against `request.output_schema` if possible.
    async fn structured_output(
        &self,
        request: StructuredRequest,
    ) -> Result<ModelResponse, ModelError>;
}

/// One-time warning that a native (direct-to-provider) adapter received tool
/// schemas it does not forward.
///
/// The native adapters (anthropic/openai/google/ollama) map only `role` +
/// `content` and never send `request.tools` to the provider, so a Model call
/// that carries tools silently no-ops and an agent tool loop degenerates. Tool
/// calls must route through the model-seam sidecar (`JAMJET_MODEL_SEAM_URL`),
/// which forwards tools. Logged once per process so a degenerate loop's
/// repeated calls do not spam the log.
pub(crate) fn warn_tools_not_forwarded(adapter: &'static str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            adapter,
            "tools provided but this adapter does not forward them; \
             use the model seam sidecar (JAMJET_MODEL_SEAM_URL) for tool calls"
        );
    });
}
