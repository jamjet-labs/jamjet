//! Ollama LLM client — free, local chat completion.

use crate::llm::LlmClient;
use crate::store::MemoryError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct OllamaLlmClient {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OllamaLlmClient {
    pub fn new() -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_config(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    format: Option<String>,
}

#[derive(Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

#[async_trait]
impl LlmClient for OllamaLlmClient {
    async fn complete(&self, system: &str, user: &str) -> Result<String, MemoryError> {
        let request = OllamaChatRequest {
            model: self.model.clone(),
            messages: vec![
                OllamaMessage {
                    role: "system".into(),
                    content: system.into(),
                },
                OllamaMessage {
                    role: "user".into(),
                    content: user.into(),
                },
            ],
            stream: false,
            format: None,
        };

        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| MemoryError::Database(format!("Ollama chat error: {e}")))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MemoryError::Database(format!("Ollama error: {body}")));
        }

        let result: OllamaChatResponse = response
            .json()
            .await
            .map_err(|e| MemoryError::Database(format!("Ollama parse error: {e}")))?;

        Ok(result.message.content)
    }

    async fn structured_output(
        &self,
        system: &str,
        user: &str,
    ) -> Result<serde_json::Value, MemoryError> {
        let request = OllamaChatRequest {
            model: self.model.clone(),
            messages: vec![
                OllamaMessage {
                    role: "system".into(),
                    content: system.into(),
                },
                OllamaMessage {
                    role: "user".into(),
                    content: user.into(),
                },
            ],
            stream: false,
            format: Some("json".into()),
        };

        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| MemoryError::Database(format!("Ollama chat error: {e}")))?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MemoryError::Database(format!("Ollama error: {body}")));
        }

        let result: OllamaChatResponse = response
            .json()
            .await
            .map_err(|e| MemoryError::Database(format!("Ollama parse error: {e}")))?;

        serde_json::from_str(&result.message.content)
            .map_err(|e| MemoryError::Serialization(format!("Ollama JSON parse: {e}")))
    }
}
