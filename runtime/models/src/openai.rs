//! OpenAI adapter (Chat Completions API).
//!
//! Supports gpt-4o, gpt-4o-mini, gpt-4-turbo, o1, o3, etc.
//! Reads `OPENAI_API_KEY` from the environment.
//! Also works with OpenAI-compatible APIs (e.g. local Ollama) via `base_url`.

use crate::adapter::{
    ChatMessage, ChatRole, ModelAdapter, ModelConfig, ModelError, ModelRequest, ModelResponse,
    StructuredRequest,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, instrument};

const OPENAI_API_BASE: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "gpt-4o";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// OpenAI Chat Completions adapter.
pub struct OpenAiAdapter {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
    default_model: String,
}

impl OpenAiAdapter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: OPENAI_API_BASE.into(),
            default_model: DEFAULT_MODEL.into(),
        }
    }

    /// Create adapter from `OPENAI_API_KEY` env var.
    pub fn from_env() -> Result<Self, ModelError> {
        let key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ModelError::Network("OPENAI_API_KEY not set".into()))?;
        Ok(Self::new(key))
    }

    /// Override the base URL (for OpenAI-compatible APIs).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    async fn call_api(&self, body: Value) -> Result<Value, ModelError> {
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
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

    fn build_request_body(
        &self,
        messages: &[ChatMessage],
        config: &ModelConfig,
        response_format: Option<Value>,
    ) -> Value {
        let model = config.model.as_deref().unwrap_or(&self.default_model);
        let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        let openai_messages: Vec<Value> = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    ChatRole::System => "system",
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                    ChatRole::Tool => "tool",
                };
                // Prepend system_prompt as system message if provided.
                json!({ "role": role, "content": m.content })
            })
            .collect();

        // If a system_prompt is in config, prepend it.
        let mut final_messages = openai_messages;
        if let Some(sys) = &config.system_prompt {
            final_messages.insert(0, json!({ "role": "system", "content": sys }));
        }

        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "messages": final_messages,
        });

        if let Some(temp) = config.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(stops) = &config.stop_sequences {
            body["stop"] = json!(stops);
        }
        if let Some(fmt) = response_format {
            body["response_format"] = fmt;
        }

        body
    }

    fn parse_response(&self, resp: Value) -> Result<ModelResponse, ModelError> {
        let model = resp["model"]
            .as_str()
            .unwrap_or(&self.default_model)
            .to_string();

        let choice = resp["choices"]
            .as_array()
            .and_then(|cs| cs.first())
            .ok_or_else(|| ModelError::Api {
                status: 200,
                body: "no choices in response".into(),
            })?;

        let content = choice["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        let finish_reason = choice["finish_reason"]
            .as_str()
            .unwrap_or("stop")
            .to_string();
        let input_tokens = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let output_tokens = resp["usage"]["completion_tokens"].as_u64().unwrap_or(0);

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
impl ModelAdapter for OpenAiAdapter {
    fn system_name(&self) -> &'static str {
        "openai"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "openai",
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
        tracing::Span::current().record("gen_ai.request.model", model.as_str());

        debug!(model = %model, "Calling OpenAI Chat Completions API");

        let body = self.build_request_body(&request.messages, &request.config, None);
        let resp_json = self.call_api(body).await?;
        let response = self.parse_response(resp_json)?;

        tracing::Span::current()
            .record("gen_ai.usage.input_tokens", response.input_tokens)
            .record("gen_ai.usage.output_tokens", response.output_tokens);

        Ok(response)
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "openai",
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
        tracing::Span::current().record("gen_ai.request.model", model.as_str());

        // Use OpenAI's native JSON mode (response_format: json_object).
        // For models that support json_schema, we pass the schema directly.
        let response_format = json!({ "type": "json_object" });

        let mut config = request.config.clone();
        let schema_str = serde_json::to_string_pretty(&request.output_schema)
            .map_err(|e| ModelError::Serialization(e.to_string()))?;
        let system = config.system_prompt.get_or_insert_with(String::new);
        system.push_str(&format!(
            "\n\nRespond ONLY with a valid JSON object matching this schema:\n{schema_str}"
        ));

        let body = self.build_request_body(&request.messages, &config, Some(response_format));
        let resp_json = self.call_api(body).await?;
        let mut response = self.parse_response(resp_json)?;

        let structured =
            serde_json::from_str::<serde_json::Value>(&response.content).map_err(|e| {
                ModelError::Serialization(format!("structured output parse error: {e}"))
            })?;
        response.structured = Some(structured);

        Ok(response)
    }
}
