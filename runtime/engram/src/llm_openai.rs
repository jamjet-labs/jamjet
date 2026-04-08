//! OpenAI-compatible LLM client — chat completions with native JSON mode.
//!
//! Works against the OpenAI public API (`https://api.openai.com/v1`) and any
//! OpenAI-compatible endpoint (Azure OpenAI, vLLM, LM Studio, groqcloud) by
//! setting a custom `base_url`.

use crate::llm::LlmClient;
use crate::llm_util::extract_json_payload;
use crate::store::MemoryError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct OpenAiLlmClient {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiLlmClient {
    /// Construct a client against the OpenAI public API with `gpt-4o-mini`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_config("https://api.openai.com/v1", api_key, "gpt-4o-mini")
    }

    /// Construct a client with explicit base URL, API key, and model name.
    pub fn with_config(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client build"),
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat<'a>>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ResponseFormat<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

impl OpenAiLlmClient {
    async fn call(&self, system: &str, user: &str, json_mode: bool) -> Result<String, MemoryError> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            response_format: if json_mode {
                Some(ResponseFormat {
                    kind: "json_object",
                })
            } else {
                None
            },
        };

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::Database(format!("OpenAI request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(MemoryError::Database(format!(
                "OpenAI returned {status}: {body}"
            )));
        }

        let parsed: ChatResponse = response
            .json()
            .await
            .map_err(|e| MemoryError::Database(format!("OpenAI parse error: {e}")))?;

        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| MemoryError::Database("OpenAI returned no choices".into()))
    }
}

#[async_trait]
impl LlmClient for OpenAiLlmClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, MemoryError> {
        self.call(system, user, false).await
    }

    async fn structured_output(
        &self,
        system: &str,
        user: &str,
    ) -> Result<serde_json::Value, MemoryError> {
        let text = self.call(system, user, true).await?;
        let payload = extract_json_payload(&text);
        serde_json::from_str(payload)
            .map_err(|e| MemoryError::Serialization(format!("OpenAI JSON parse: {e}")))
    }
}
