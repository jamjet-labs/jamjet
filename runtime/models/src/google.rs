//! Google Gemini adapter (Generative Language API).
//!
//! Supports gemini-2.0-flash, gemini-1.5-flash, gemini-1.5-pro, etc.
//! Reads `GOOGLE_API_KEY` or `GEMINI_API_KEY` from the environment.
//! Uses the REST API directly (no SDK dependency).

use crate::adapter::{
    ChatMessage, ChatRole, ModelAdapter, ModelConfig, ModelError, ModelRequest, ModelResponse,
    StructuredRequest,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, instrument};

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_MODEL: &str = "gemini-2.0-flash";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Google Gemini adapter via the Generative Language REST API.
///
/// Uses API key authentication (not OAuth). Supports all Gemini models
/// available through Google AI Studio.
pub struct GoogleAdapter {
    client: reqwest::Client,
    api_key: String,
    default_model: String,
}

impl GoogleAdapter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            default_model: DEFAULT_MODEL.into(),
        }
    }

    /// Create adapter from `GOOGLE_API_KEY` or `GEMINI_API_KEY` env var.
    pub fn from_env() -> Result<Self, ModelError> {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
            .map_err(|_| ModelError::Network("GOOGLE_API_KEY or GEMINI_API_KEY not set".into()))?;
        Ok(Self::new(key))
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    async fn call_api(&self, model: &str, body: Value) -> Result<Value, ModelError> {
        // Gemini API URL: /v1beta/models/{model}:generateContent?key={key}
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            GEMINI_API_BASE, model, self.api_key
        );

        let resp = self
            .client
            .post(&url)
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
        response_mime_type: Option<&str>,
    ) -> Value {
        let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

        // Gemini uses "contents" with roles "user" and "model" (not "assistant").
        let contents: Vec<Value> = messages
            .iter()
            .filter(|m| !matches!(m.role, ChatRole::System))
            .map(|m| {
                let role = match m.role {
                    ChatRole::User | ChatRole::Tool => "user",
                    ChatRole::Assistant => "model",
                    ChatRole::System => unreachable!(),
                };
                json!({
                    "role": role,
                    "parts": [{"text": m.content}]
                })
            })
            .collect();

        let mut generation_config = json!({
            "maxOutputTokens": max_tokens,
        });

        if let Some(temp) = config.temperature {
            generation_config["temperature"] = json!(temp);
        }
        if let Some(stops) = &config.stop_sequences {
            generation_config["stopSequences"] = json!(stops);
        }
        if let Some(mime) = response_mime_type {
            generation_config["responseMimeType"] = json!(mime);
        }

        let mut body = json!({
            "contents": contents,
            "generationConfig": generation_config,
        });

        // System instruction (separate from contents in Gemini API).
        let system_text = config.system_prompt.as_deref().or_else(|| {
            messages
                .iter()
                .find(|m| matches!(m.role, ChatRole::System))
                .map(|m| m.content.as_str())
        });

        if let Some(sys) = system_text {
            body["systemInstruction"] = json!({
                "parts": [{"text": sys}]
            });
        }

        body
    }

    fn parse_response(&self, resp: Value) -> Result<ModelResponse, ModelError> {
        // Extract text from candidates[0].content.parts[0].text
        let candidate = resp["candidates"]
            .as_array()
            .and_then(|cs| cs.first())
            .ok_or_else(|| ModelError::Api {
                status: 200,
                body: "no candidates in response".into(),
            })?;

        let content = candidate["content"]["parts"]
            .as_array()
            .and_then(|parts| parts.first())
            .and_then(|p| p["text"].as_str())
            .unwrap_or("")
            .to_string();

        let finish_reason = candidate["finishReason"]
            .as_str()
            .unwrap_or("STOP")
            .to_string();

        // Token counts from usageMetadata.
        let usage = &resp["usageMetadata"];
        let input_tokens = usage["promptTokenCount"].as_u64().unwrap_or(0);
        let output_tokens = usage["candidatesTokenCount"].as_u64().unwrap_or(0);

        // Model name from modelVersion if available.
        let model = resp["modelVersion"]
            .as_str()
            .unwrap_or(&self.default_model)
            .to_string();

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
impl ModelAdapter for GoogleAdapter {
    fn system_name(&self) -> &'static str {
        "google"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "google",
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

        debug!(model = %model, "Calling Gemini generateContent API");

        let body = self.build_request_body(&request.messages, &request.config, None);
        let resp_json = self.call_api(&model, body).await?;
        let response = self.parse_response(resp_json)?;

        tracing::Span::current()
            .record("gen_ai.usage.input_tokens", response.input_tokens)
            .record("gen_ai.usage.output_tokens", response.output_tokens);

        Ok(response)
    }

    #[instrument(skip(self, request), fields(
        gen_ai.system = "google",
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

        // Gemini supports responseMimeType: "application/json" for JSON mode.
        // Append schema to system prompt.
        let mut config = request.config.clone();
        let schema_str = serde_json::to_string_pretty(&request.output_schema)
            .map_err(|e| ModelError::Serialization(e.to_string()))?;
        let system = config.system_prompt.get_or_insert_with(String::new);
        system.push_str(&format!(
            "\n\nRespond ONLY with a valid JSON object matching this schema:\n{schema_str}"
        ));

        let body = self.build_request_body(&request.messages, &config, Some("application/json"));
        let resp_json = self.call_api(&model, body).await?;
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
    fn test_build_request_body_system_instruction() {
        let adapter = GoogleAdapter::new("test-key");
        let messages = vec![ChatMessage::user("Hello")];
        let config = ModelConfig {
            model: Some("gemini-2.0-flash".into()),
            system_prompt: Some("You are helpful.".into()),
            max_tokens: Some(100),
            ..Default::default()
        };
        let body = adapter.build_request_body(&messages, &config, None);

        assert!(body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("You are helpful"));
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 100);
    }

    #[test]
    fn test_parse_response() {
        let adapter = GoogleAdapter::new("test-key");
        let resp = json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello!"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 3,
                "totalTokenCount": 13
            },
            "modelVersion": "gemini-2.0-flash"
        });

        let parsed = adapter.parse_response(resp).unwrap();
        assert_eq!(parsed.content, "Hello!");
        assert_eq!(parsed.input_tokens, 10);
        assert_eq!(parsed.output_tokens, 3);
        assert_eq!(parsed.finish_reason, "STOP");
    }
}
