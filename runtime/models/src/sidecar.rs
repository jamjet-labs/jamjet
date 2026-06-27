//! Sidecar model adapter — POSTs to the Python model-seam sidecar.
//!
//! Set `JAMJET_MODEL_SEAM_URL` (e.g. `http://127.0.0.1:4280`) to route
//! durable-path model calls through the governed Python seam (provider
//! allow-list, PII redaction, cost metering, middleware).
//!
//! Sidecar contract:
//! - `POST /v1/complete` — `{model, messages, temperature?, max_tokens?}`
//!   → `{message:{content,role}, input_tokens, output_tokens, cost_usd, model, finish_reason}`
//! - `GET /health` → `{ok:true}`

use crate::adapter::{
    ChatRole, ModelAdapter, ModelError, ModelRequest, ModelResponse, StructuredRequest,
};
use async_trait::async_trait;
use serde_json::{json, Value};

const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-6";

/// Routes durable-path model calls to the Python model-seam sidecar via HTTP.
///
/// The sidecar wraps `jamjet.model.Model` (Track-1 seam), so every call
/// inherits the same governed middleware as in-process `Agent.run()`.
pub struct SidecarModelAdapter {
    client: reqwest::Client,
    base_url: String,
}

impl SidecarModelAdapter {
    /// Create an adapter that POSTs to `base_url` (e.g. `http://127.0.0.1:4280`).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    async fn call_complete(&self, body: Value) -> Result<Value, ModelError> {
        let url = format!("{}/v1/complete", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| ModelError::Network(e.to_string()))?;

        if status == 429 {
            return Err(ModelError::RateLimited {
                retry_after_secs: 60,
            });
        }
        if status != 200 {
            return Err(ModelError::Api { status, body: text });
        }
        serde_json::from_str(&text).map_err(|e| ModelError::Serialization(e.to_string()))
    }

    fn parse_response(&self, json: Value) -> Result<ModelResponse, ModelError> {
        let content = json["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let model = json["model"].as_str().unwrap_or(DEFAULT_MODEL).to_string();
        let finish_reason = json["finish_reason"].as_str().unwrap_or("stop").to_string();
        let input_tokens = json["input_tokens"].as_u64().unwrap_or(0);
        let output_tokens = json["output_tokens"].as_u64().unwrap_or(0);
        Ok(ModelResponse {
            content,
            model,
            finish_reason,
            input_tokens,
            output_tokens,
            structured: None,
        })
    }

    fn build_messages(
        messages: &[crate::adapter::ChatMessage],
        system_prompt: Option<&str>,
    ) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        // Inject system prompt as leading system message if configured.
        if let Some(sys) = system_prompt {
            if !sys.is_empty() {
                out.push(json!({ "role": "system", "content": sys }));
            }
        }
        for m in messages {
            let role = match m.role {
                ChatRole::System => "system",
                ChatRole::User | ChatRole::Tool => "user",
                ChatRole::Assistant => "assistant",
            };
            out.push(json!({ "role": role, "content": m.content }));
        }
        out
    }
}

#[async_trait]
impl ModelAdapter for SidecarModelAdapter {
    fn system_name(&self) -> &'static str {
        "sidecar"
    }

    fn default_model(&self) -> &str {
        DEFAULT_MODEL
    }

    async fn chat(&self, request: ModelRequest) -> Result<ModelResponse, ModelError> {
        let model = request
            .config
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.into());

        let messages =
            Self::build_messages(&request.messages, request.config.system_prompt.as_deref());

        let mut body = json!({
            "model": model,
            "messages": messages,
        });
        if let Some(temp) = request.config.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(max) = request.config.max_tokens {
            body["max_tokens"] = json!(max);
        }

        let resp_json = self.call_complete(body).await?;
        self.parse_response(resp_json)
    }

    async fn structured_output(
        &self,
        request: StructuredRequest,
    ) -> Result<ModelResponse, ModelError> {
        // Append schema instruction to system prompt (mirrors AnthropicAdapter).
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

// ── Coverage guard ────────────────────────────────────────────────────────────

/// Probe the sidecar `/health` endpoint at startup.
///
/// Returns `Err` with a descriptive message if the sidecar is unreachable or
/// responds with a non-2xx status — so a misconfigured deployment fails loud
/// rather than silently falling through to the native (ungoverned) adapters.
pub async fn check_sidecar_health(
    base_url: &str,
    client: &reqwest::Client,
) -> Result<(), ModelError> {
    let url = format!("{base_url}/health");
    let resp = client.get(&url).send().await.map_err(|e| {
        ModelError::Network(format!(
            "JAMJET_MODEL_SEAM_URL set but sidecar unreachable at {url} — \
             refusing to start so model calls never silently bypass the governed seam. \
             Cause: {e}"
        ))
    })?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(ModelError::Api {
            status,
            body: format!(
                "JAMJET_MODEL_SEAM_URL set but sidecar /health returned {status} — \
                 refusing to start so model calls never silently bypass the governed seam. \
                 Body: {body}"
            ),
        });
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ChatMessage;

    #[tokio::test]
    async fn chat_maps_response_fields() {
        let mut server = mockito::Server::new_async().await;

        let _mock = server
            .mock("POST", "/v1/complete")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "message": {"content": "Hello, world!", "role": "assistant"},
                "input_tokens": 10,
                "output_tokens": 5,
                "cost_usd": 0.001,
                "model": "anthropic/claude-sonnet-4-6",
                "finish_reason": "stop"
            }"#,
            )
            .create_async()
            .await;

        let adapter = SidecarModelAdapter::new(server.url());
        let req = ModelRequest::new(vec![ChatMessage::user("hi")]);
        let resp = adapter.chat(req).await.expect("chat should succeed");

        assert_eq!(resp.content, "Hello, world!");
        assert_eq!(resp.input_tokens, 10);
        assert_eq!(resp.output_tokens, 5);
        assert_eq!(resp.model, "anthropic/claude-sonnet-4-6");
        assert_eq!(resp.finish_reason, "stop");
        assert!(resp.structured.is_none());
    }

    #[tokio::test]
    async fn chat_sends_temperature_and_max_tokens() {
        use crate::adapter::ModelConfig;

        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/v1/complete")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"temperature":0.5,"max_tokens":256}"#.into(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                "message":{"content":"ok","role":"assistant"},
                "input_tokens":1,"output_tokens":1,
                "model":"anthropic/claude-sonnet-4-6","finish_reason":"stop"
            }"#,
            )
            .create_async()
            .await;

        let adapter = SidecarModelAdapter::new(server.url());
        let req = ModelRequest::new(vec![ChatMessage::user("hi")]).with_config(ModelConfig {
            temperature: Some(0.5),
            max_tokens: Some(256),
            ..Default::default()
        });
        adapter.chat(req).await.expect("should succeed");
    }

    #[tokio::test]
    async fn chat_errors_on_non_200() {
        let mut server = mockito::Server::new_async().await;

        let _mock = server
            .mock("POST", "/v1/complete")
            .with_status(500)
            .with_body("internal server error")
            .create_async()
            .await;

        let adapter = SidecarModelAdapter::new(server.url());
        let req = ModelRequest::new(vec![ChatMessage::user("hi")]);
        let result = adapter.chat(req).await;

        assert!(
            matches!(result, Err(ModelError::Api { status: 500, .. })),
            "expected Api error with status 500, got {result:?}"
        );
    }

    #[tokio::test]
    async fn chat_errors_on_rate_limit() {
        let mut server = mockito::Server::new_async().await;

        let _mock = server
            .mock("POST", "/v1/complete")
            .with_status(429)
            .with_body("rate limited")
            .create_async()
            .await;

        let adapter = SidecarModelAdapter::new(server.url());
        let req = ModelRequest::new(vec![ChatMessage::user("hi")]);
        let result = adapter.chat(req).await;

        assert!(
            matches!(result, Err(ModelError::RateLimited { .. })),
            "expected RateLimited, got {result:?}"
        );
    }

    #[tokio::test]
    async fn health_check_passes_on_200() {
        let mut server = mockito::Server::new_async().await;

        let _mock = server
            .mock("GET", "/health")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"ok":true}"#)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        check_sidecar_health(&server.url(), &client)
            .await
            .expect("health check should pass");
    }

    #[tokio::test]
    async fn health_check_errors_on_non_200() {
        let mut server = mockito::Server::new_async().await;

        let _mock = server
            .mock("GET", "/health")
            .with_status(503)
            .with_body("unavailable")
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let result = check_sidecar_health(&server.url(), &client).await;
        assert!(
            matches!(result, Err(ModelError::Api { status: 503, .. })),
            "expected Api error with status 503, got {result:?}"
        );
    }

    #[tokio::test]
    async fn health_check_errors_on_unreachable() {
        // Port 1 is never listening.
        let client = reqwest::Client::new();
        let result = check_sidecar_health("http://127.0.0.1:1", &client).await;
        assert!(
            matches!(result, Err(ModelError::Network(_))),
            "expected Network error, got {result:?}"
        );
    }
}
