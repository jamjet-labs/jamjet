//! Anthropic Claude adapter (Messages API).
//!
//! Supports claude-3-haiku, claude-3-sonnet, claude-3-opus, claude-sonnet-4-6, etc.
//! Reads `ANTHROPIC_API_KEY` from the environment.

use crate::adapter::{
    ChatMessage, ChatRole, ModelAdapter, ModelConfig, ModelError, ModelRequest, ModelResponse,
    StructuredRequest,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, instrument};

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Anthropic Claude adapter.
pub struct AnthropicAdapter {
    client: reqwest::Client,
    api_key: String,
    default_model: String,
}

impl AnthropicAdapter {
    /// Create an adapter with the given API key.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            default_model: DEFAULT_MODEL.into(),
        }
    }

    /// Create adapter from `ANTHROPIC_API_KEY` env var.
    pub fn from_env() -> Result<Self, ModelError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ModelError::Network("ANTHROPIC_API_KEY not set".into()))?;
        Ok(Self::new(key))
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    async fn call_api(&self, body: Value) -> Result<Value, ModelError> {
        let resp = self
            .client
            .post(format!("{ANTHROPIC_API_BASE}/v1/messages"))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        let body_text = resp
            .text()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;

        if status == 429 {
            return Err(ModelError::RateLimited {
                retry_after_secs: 60,
            });
        }
        if status != 200 {
            return Err(ModelError::Api {
                status,
                body: body_text,
            });
        }

        serde_json::from_str(&body_text).map_err(|e| ModelError::Serialization(e.to_string()))
    }

    fn build_request_body(&self, messages: &[ChatMessage], config: &ModelConfig) -> Value {
        let model = config.model.as_deref().unwrap_or(&self.default_model);
        let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        // Anthropic separates system prompt from messages.
        let system_prompt = config.system_prompt.as_deref().or_else(|| {
            // Extract system message from messages list if present.
            messages
                .iter()
                .find(|m| matches!(m.role, ChatRole::System))
                .map(|m| m.content.as_str())
        });

        let anthropic_messages: Vec<Value> = messages
            .iter()
            .filter(|m| !matches!(m.role, ChatRole::System))
            .map(|m| {
                let role = match m.role {
                    ChatRole::User | ChatRole::Tool => "user",
                    ChatRole::Assistant => "assistant",
                    ChatRole::System => "user", // already filtered
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();

        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": anthropic_messages,
        });

        if let Some(system) = system_prompt {
            body["system"] = json!(system);
        }
        if let Some(temp) = config.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(stops) = &config.stop_sequences {
            body["stop_sequences"] = json!(stops);
        }

        body
    }

    fn parse_response(&self, resp: Value) -> Result<ModelResponse, ModelError> {
        let model = resp["model"]
            .as_str()
            .unwrap_or(&self.default_model)
            .to_string();

        let content = resp["content"]
            .as_array()
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|b| b["type"].as_str() == Some("text"))
                    .and_then(|b| b["text"].as_str())
            })
            .unwrap_or("")
            .to_string();

        let finish_reason = resp["stop_reason"].as_str().unwrap_or("stop").to_string();
        let input_tokens = resp["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = resp["usage"]["output_tokens"].as_u64().unwrap_or(0);

        Ok(ModelResponse {
            content,
            model,
            finish_reason,
            input_tokens,
            output_tokens,
            structured: None,
        })
    }
}

#[async_trait]
impl ModelAdapter for AnthropicAdapter {
    fn system_name(&self) -> &'static str {
        "anthropic"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "anthropic",
        gen_ai.request.model = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
    ))]
    async fn chat(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let model = request
            .config
            .model
            .as_deref()
            .unwrap_or(&self.default_model)
            .to_string();
        tracing::Span::current().record("gen_ai.request.model", &model.as_str());

        debug!(model = %model, "Calling Anthropic Messages API");

        let body = self.build_request_body(&request.messages, &request.config);
        let resp_json = self.call_api(body).await?;
        let response = self.parse_response(resp_json)?;

        tracing::Span::current()
            .record("gen_ai.usage.input_tokens", response.input_tokens)
            .record("gen_ai.usage.output_tokens", response.output_tokens);

        Ok(response)
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "anthropic",
        gen_ai.request.model = tracing::field::Empty,
    ))]
    async fn structured_output(
        &self,
        request: StructuredRequest,
    ) -> Result<ModelResponse, ModelError> {
        let model = request
            .config
            .model
            .as_deref()
            .unwrap_or(&self.default_model)
            .to_string();
        tracing::Span::current().record("gen_ai.request.model", &model.as_str());

        // Append schema instruction to system prompt.
        let schema_str = serde_json::to_string_pretty(&request.output_schema)
            .map_err(|e| ModelError::Serialization(e.to_string()))?;
        let mut config = request.config.clone();
        let system = config.system_prompt.get_or_insert_with(String::new);
        system.push_str(&format!(
            "\n\nRespond ONLY with a valid JSON object matching this schema:\n{schema_str}\nDo not include any other text."
        ));

        let chat_req = ModelRequest {
            messages: request.messages,
            config,
        };
        let mut response = self.chat(chat_req).await?;

        // Parse structured output from the response content.
        let structured = serde_json::from_str::<Value>(&response.content)
            .or_else(|_| {
                // Try to extract JSON from markdown code blocks.
                let trimmed = response.content.trim();
                let inner = trimmed
                    .trim_start_matches("```json")
                    .trim_start_matches("```")
                    .trim_end_matches("```")
                    .trim();
                serde_json::from_str::<Value>(inner)
            })
            .map_err(|e| {
                ModelError::Serialization(format!("failed to parse structured output: {e}"))
            })?;

        response.structured = Some(structured);
        Ok(response)
    }
}
