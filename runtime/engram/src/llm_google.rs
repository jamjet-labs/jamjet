//! Google Gemini LLM client — `generateContent` API.
//!
//! Uses Gemini's native `responseMimeType: application/json` for structured
//! output. Defaults to `gemini-2.0-flash` (cheap, fast).

use crate::llm::LlmClient;
use crate::llm_util::extract_json_payload;
use crate::store::MemoryError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub struct GoogleLlmClient {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl GoogleLlmClient {
    /// Construct a client against the Gemini public API with `gemini-flash-latest`.
    /// Use `with_config` if you need to pin a specific version like `gemini-2.5-flash`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_config(
            "https://generativelanguage.googleapis.com/v1beta",
            api_key,
            "gemini-flash-latest",
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
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client build"),
        }
    }
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    #[serde(rename = "systemInstruction")]
    system_instruction: InstructionBlock<'a>,
    contents: Vec<Content<'a>>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig<'a>>,
}

#[derive(Serialize)]
struct InstructionBlock<'a> {
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Content<'a> {
    role: &'a str,
    parts: Vec<Part<'a>>,
}

#[derive(Serialize)]
struct Part<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GenerationConfig<'a> {
    #[serde(rename = "responseMimeType")]
    response_mime_type: &'a str,
}

#[derive(Deserialize)]
struct GenerateResponse {
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Deserialize)]
struct CandidateContent {
    parts: Vec<PartOut>,
}

#[derive(Deserialize)]
struct PartOut {
    #[serde(default)]
    text: String,
}

impl GoogleLlmClient {
    async fn call(&self, system: &str, user: &str, json_mode: bool) -> Result<String, MemoryError> {
        let body = GenerateRequest {
            system_instruction: InstructionBlock {
                parts: vec![Part { text: system }],
            },
            contents: vec![Content {
                role: "user",
                parts: vec![Part { text: user }],
            }],
            generation_config: if json_mode {
                Some(GenerationConfig {
                    response_mime_type: "application/json",
                })
            } else {
                None
            },
        };

        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url, self.model, self.api_key
        );

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| MemoryError::Database(format!("Google request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(MemoryError::Database(format!(
                "Google returned {status}: {body}"
            )));
        }

        let parsed: GenerateResponse = response
            .json()
            .await
            .map_err(|e| MemoryError::Database(format!("Google parse error: {e}")))?;

        parsed
            .candidates
            .into_iter()
            .next()
            .and_then(|c| c.content.parts.into_iter().next())
            .map(|p| p.text)
            .ok_or_else(|| MemoryError::Database("Google returned no candidates".into()))
    }
}

#[async_trait]
impl LlmClient for GoogleLlmClient {
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
            .map_err(|e| MemoryError::Serialization(format!("Google JSON parse: {e}")))
    }
}
