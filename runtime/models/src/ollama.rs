//! Ollama adapter — local model inference via Ollama's HTTP API.
//!
//! Supports any model available via `ollama pull`: qwen3, llama3, gemma2, phi3, etc.
//! Reads `OLLAMA_HOST` from the environment (defaults to http://localhost:11434).
//! Uses Ollama's native /api/chat endpoint for accurate token counts.

use crate::adapter::{
    ChatMessage, ChatRole, ModelAdapter, ModelConfig, ModelError, ModelRequest, ModelResponse,
    StructuredRequest,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, instrument};

const OLLAMA_DEFAULT_HOST: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "llama3.2:3b";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Ollama adapter for local model inference.
///
/// Connects to a running Ollama server and uses its native /api/chat endpoint.
/// All inference is free (local GPU/CPU), making this ideal for development,
/// testing, and cost-sensitive workloads.
pub struct OllamaAdapter {
    client: reqwest::Client,
    host: String,
    default_model: String,
}

impl OllamaAdapter {
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            host: host.into(),
            default_model: DEFAULT_MODEL.into(),
        }
    }

    /// Create adapter from `OLLAMA_HOST` env var (defaults to localhost:11434).
    pub fn from_env() -> Result<Self, ModelError> {
        let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| OLLAMA_DEFAULT_HOST.to_string());

        // Quick check: if Ollama is not reachable, fail fast.
        // We skip the actual health check here to keep construction sync;
        // errors will surface on first call_api() instead.
        Ok(Self::new(host))
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    async fn call_api(&self, body: Value) -> Result<Value, ModelError> {
        let resp = self
            .client
            .post(format!("{}/api/chat", self.host))
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Network(format!("Ollama unreachable: {e}")))?;

        let status = resp.status().as_u16();
        let body_text = resp
            .text()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;

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
        format: Option<&str>,
    ) -> Value {
        let model = config.model.as_deref().unwrap_or(&self.default_model);
        let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        let mut ollama_messages: Vec<Value> = Vec::new();

        // Prepend system prompt if provided in config.
        if let Some(sys) = &config.system_prompt {
            ollama_messages.push(json!({ "role": "system", "content": sys }));
        }

        for m in messages {
            let role = match m.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
                ChatRole::Tool => "tool",
            };
            ollama_messages.push(json!({ "role": role, "content": m.content }));
        }

        let mut body = json!({
            "model": model,
            "messages": ollama_messages,
            "stream": false,
            "options": {
                "num_predict": max_tokens,
            },
        });

        if let Some(temp) = config.temperature {
            body["options"]["temperature"] = json!(temp);
        }
        if let Some(stops) = &config.stop_sequences {
            body["options"]["stop"] = json!(stops);
        }
        if let Some(fmt) = format {
            body["format"] = json!(fmt);
        }

        body
    }

    fn parse_response(&self, resp: Value) -> Result<ModelResponse, ModelError> {
        let model = resp["model"]
            .as_str()
            .unwrap_or(&self.default_model)
            .to_string();

        let content = resp["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        // Ollama provides token counts in prompt_eval_count / eval_count.
        let input_tokens = resp["prompt_eval_count"].as_u64().unwrap_or(0);
        let output_tokens = resp["eval_count"].as_u64().unwrap_or(0);

        // Ollama uses "done_reason" (not "finish_reason").
        let finish_reason = resp["done_reason"].as_str().unwrap_or("stop").to_string();

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
impl ModelAdapter for OllamaAdapter {
    fn system_name(&self) -> &'static str {
        "ollama"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "ollama",
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

        debug!(model = %model, host = %self.host, "Calling Ollama /api/chat");

        let body = self.build_request_body(&request.messages, &request.config, None);
        let resp_json = self.call_api(body).await?;
        let response = self.parse_response(resp_json)?;

        tracing::Span::current()
            .record("gen_ai.usage.input_tokens", response.input_tokens)
            .record("gen_ai.usage.output_tokens", response.output_tokens);

        Ok(response)
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "ollama",
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

        // Ollama supports format: "json" for JSON mode.
        // Append the schema to the system prompt so the model knows the structure.
        let mut config = request.config.clone();
        let schema_str = serde_json::to_string_pretty(&request.output_schema)
            .map_err(|e| ModelError::Serialization(e.to_string()))?;
        let system = config.system_prompt.get_or_insert_with(String::new);
        system.push_str(&format!(
            "\n\nRespond ONLY with a valid JSON object matching this schema:\n{schema_str}"
        ));

        let body = self.build_request_body(&request.messages, &config, Some("json"));
        let resp_json = self.call_api(body).await?;
        let mut response = self.parse_response(resp_json)?;

        // Parse JSON from response content.
        let structured =
            serde_json::from_str::<serde_json::Value>(&response.content).map_err(|e| {
                ModelError::Serialization(format!("structured output parse error: {e}"))
            })?;
        response.structured = Some(structured);

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_request_body() {
        let adapter = OllamaAdapter::new("http://localhost:11434");
        let messages = vec![ChatMessage::user("Hello")];
        let config = ModelConfig {
            model: Some("qwen3:8b".into()),
            max_tokens: Some(100),
            temperature: Some(0.7),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, None);

        assert_eq!(body["model"], "qwen3:8b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["num_predict"], 100);
        let temp = body["options"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
    }

    #[test]
    fn test_parse_response() {
        let adapter = OllamaAdapter::new("http://localhost:11434");
        let resp = json!({
            "model": "qwen3:8b",
            "message": {"role": "assistant", "content": "Hello!"},
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 42,
            "eval_count": 5,
        });

        let parsed = adapter.parse_response(resp).unwrap();
        assert_eq!(parsed.content, "Hello!");
        assert_eq!(parsed.model, "qwen3:8b");
        assert_eq!(parsed.input_tokens, 42);
        assert_eq!(parsed.output_tokens, 5);
        assert_eq!(parsed.finish_reason, "stop");
    }
}
