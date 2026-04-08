//! Anthropic Claude LLM client — Messages API.
//!
//! Unlike OpenAI, Anthropic has no native `response_format: json_object` at
//! the time of writing. Structured output is requested via the system prompt
//! and then extracted from the text response. The `llm_util::extract_json_payload`
//! helper handles markdown fences that Claude occasionally emits.

use crate::llm::LlmClient;
use crate::llm_util::extract_json_payload;
use crate::store::MemoryError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct AnthropicLlmClient {
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl AnthropicLlmClient {
    /// Construct a client against the Anthropic public API with
    /// `claude-haiku-4-5-20251001` (the cheapest Claude 4.5 tier).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_config(
            "https://api.anthropic.com",
            api_key,
            "claude-haiku-4-5-20251001",
        )
    }

    pub fn with_config(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 4096,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client build"),
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<UserMessage<'a>>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

impl AnthropicLlmClient {
    async fn call(&self, system: &str, user: &str) -> Result<String, MemoryError> {
        let body = MessagesRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            system,
            messages: vec![UserMessage {
                role: "user",
                content: user,
            }],
        };

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::Database(format!("Anthropic request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(MemoryError::Database(format!(
                "Anthropic returned {status}: {body}"
            )));
        }

        let parsed: MessagesResponse = response
            .json()
            .await
            .map_err(|e| MemoryError::Database(format!("Anthropic parse error: {e}")))?;

        let text = parsed
            .content
            .into_iter()
            .find(|b| b.kind == "text")
            .map(|b| b.text)
            .ok_or_else(|| MemoryError::Database("Anthropic returned no text blocks".into()))?;

        Ok(text)
    }
}

#[async_trait]
impl LlmClient for AnthropicLlmClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, MemoryError> {
        self.call(system, user).await
    }

    async fn structured_output(
        &self,
        system: &str,
        user: &str,
    ) -> Result<serde_json::Value, MemoryError> {
        // Nudge Claude toward JSON-only output by appending to the system
        // prompt. The core extraction prompt already specifies the shape.
        let system_json = format!(
            "{system}\n\nRespond with ONLY the JSON object and no other text, no markdown fences, no commentary."
        );
        let text = self.call(&system_json, user).await?;
        let payload = extract_json_payload(&text);
        serde_json::from_str(payload)
            .map_err(|e| MemoryError::Serialization(format!("Anthropic JSON parse: {e}")))
    }
}
